use crate::{
    ConfigScope,
    config::{AppConfig, ConfigError, ProxyProtocol, ResolvedHostConfig},
    control::{ControlEndpoint, ControlListener, serve_control},
    inbound::{
        DetectedProtocol, InboundError, SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED,
        SOCKS5_REPLY_COMMAND_NOT_SUPPORTED, SOCKS5_REPLY_GENERAL_FAILURE, SOCKS5_REPLY_SUCCEEDED,
        accept_socks5, detect_protocol, write_socks5_reply,
    },
    network::{ConnectivityHandle, ConnectivityMonitor},
    outbound::{BoxedProxyStream, DialContext, OutboundDialer, TargetAddr},
    reload::ReloadHandle,
    ssh::{SshPoolDialer, register_idle_ssh_host},
    ssh_config::inherit_ssh_config_forwards,
    stats::{self, BodyTiming, TimedBody, TimedIo, elapsed_ms, next_connection_id},
};
use anyhow::{Context, anyhow, bail};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode, Uri,
    body::{Bytes, Incoming},
    client::conn::http1 as client_http1,
    header::{
        CONNECTION, CONTENT_TYPE, HOST, HeaderName, HeaderValue, PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE, VIA,
    },
    http::uri::Authority,
    server::conn::http1 as server_http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    future::Future,
    io::ErrorKind,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinSet,
};
use tracing::{Instrument, debug, info, info_span, warn};

const HTTP_MAX_BUFFER_SIZE: usize = 16 * 1024;
const LOCAL_LISTENER_ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeProfile {
    Service,
    #[default]
    Foreground,
}

impl RuntimeProfile {
    pub const fn config_scope(self) -> ConfigScope {
        match self {
            Self::Service => ConfigScope::System,
            Self::Foreground => ConfigScope::User,
        }
    }
}

#[derive(Debug)]
pub struct Engine {
    config: AppConfig,
    profile: RuntimeProfile,
    reload_handle: Option<ReloadHandle>,
}

impl Engine {
    pub fn new(config: AppConfig) -> Result<Self, ConfigError> {
        Self::with_profile(config, RuntimeProfile::Foreground)
    }

    pub fn with_profile(config: AppConfig, profile: RuntimeProfile) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            profile,
            reload_handle: None,
        })
    }

    pub(crate) fn with_profile_and_reload(
        config: AppConfig,
        profile: RuntimeProfile,
        reload_handle: ReloadHandle,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            profile,
            reload_handle: Some(reload_handle),
        })
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub async fn run_until_shutdown<F>(self, shutdown: F) -> anyhow::Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        self.run_until_shutdown_with_ready(shutdown, None).await
    }

    pub(crate) async fn run_until_shutdown_with_ready<F>(
        self,
        shutdown: F,
        ready: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let profile = self.profile;
        let reload_handle = self.reload_handle;
        let AppConfig {
            control,
            env: _,
            launchers: _,
            override_default,
            hosts,
        } = self.config;
        let mut tasks = JoinSet::new();
        let connectivity_monitor = ConnectivityMonitor::start().await;
        let connectivity = connectivity_monitor.handle();
        if let Some(handle) = reload_handle {
            let endpoint = ControlEndpoint::from_config(&control, profile.config_scope())?;
            let listener = ControlListener::bind(endpoint).await?;
            tasks.spawn(serve_control(listener, handle));
        }
        let mut retained_dialers = Vec::new();
        let mut host_count = 0_usize;
        let mut local_forward_count = 0_usize;
        let mut remote_forward_count = 0_usize;
        let runtime = stats::RuntimeGuard::start(0, 0, 0);
        tasks.spawn(stats::run_traffic_sampler());
        for (host_name, host) in hosts {
            let mut host = host.resolve(&override_default);
            if !host.auto {
                continue;
            }
            inherit_ssh_config_forwards(&mut host).with_context(|| {
                format!("failed to inherit SSH config forwards for host {host_name}")
            })?;
            host_count += 1;
            remote_forward_count += host
                .remote_proxies
                .iter()
                .filter(|proxy| proxy.auto)
                .count()
                + host
                    .remote_forwards
                    .iter()
                    .filter(|forward| forward.auto)
                    .count();
            register_host_tunnels(&host_name, &host);
            if !host.has_automatic_tunnels() {
                register_idle_ssh_host(&host_name, &host.runtime_pool(&host_name))?;
                info!(
                    host_name,
                    "SSH host is idle because it has no enabled tunnels"
                );
                continue;
            }
            let dialer = build_ssh_host(&host_name, &host, connectivity.clone())?;
            retained_dialers.push(Arc::clone(&dialer));
            let listener_retry = ListenerRetryPolicy {
                initial: Duration::from_millis(host.restart_initial_millis),
                max: Duration::from_secs(host.restart_max_secs),
            };
            for proxy in host.local_proxies.into_iter().filter(|proxy| proxy.auto) {
                local_forward_count += 1;
                let name = proxy.runtime_name("local-proxy");
                let tunnel_id = stats::tunnel_id(&host_name, stats::TunnelKind::LocalProxy, &name);
                tasks.spawn(run_proxy_listener(
                    ProxyListenerRuntime {
                        name,
                        tunnel_id,
                        listen: proxy.listen,
                        protocol: proxy.resolved_protocol(),
                        host_name: host_name.clone(),
                        retry: listener_retry,
                    },
                    Arc::clone(&dialer),
                ));
            }
            for forward in host
                .local_forwards
                .into_iter()
                .filter(|forward| forward.auto)
            {
                local_forward_count += 1;
                let name = forward.runtime_name("local-forward");
                let tunnel_id =
                    stats::tunnel_id(&host_name, stats::TunnelKind::LocalForward, &name);
                tasks.spawn(run_tcp_listener(
                    TcpListenerRuntime {
                        name,
                        tunnel_id,
                        listen: forward.listen,
                        target_host: forward.target.host,
                        target_port: forward.target.port,
                        host_name: host_name.clone(),
                        retry: listener_retry,
                    },
                    Arc::clone(&dialer),
                ));
            }
        }
        runtime.update_configured_counts(host_count, local_forward_count, remote_forward_count);
        info!(
            ?profile,
            local_forward_count, remote_forward_count, host_count, "stk engine started"
        );
        if let Some(ready) = ready {
            let _ = ready.send(());
        }

        let result = if tasks.is_empty() {
            shutdown.await;
            info!("shutdown signal received");
            Ok(())
        } else {
            tokio::select! {
                _ = shutdown => {
                    info!("shutdown signal received");
                    Ok(())
                }
                listener = tasks.join_next() => listener_exit_result(listener),
            }
        };

        tasks.abort_all();
        while let Some(joined) = tasks.join_next().await {
            if let Err(error) = joined
                && !error.is_cancelled()
            {
                warn!(%error, "local forward task failed during shutdown");
            }
        }

        result
    }
}

fn register_host_tunnels(host_name: &str, host: &ResolvedHostConfig) {
    for proxy in host.local_proxies.iter().filter(|proxy| proxy.auto) {
        let name = proxy.runtime_name("local-proxy");
        stats::register_tunnel(stats::TunnelRegistration {
            id: stats::tunnel_id(host_name, stats::TunnelKind::LocalProxy, &name),
            host_name: host_name.to_string(),
            name,
            kind: stats::TunnelKind::LocalProxy,
            listen: proxy.listen.to_string(),
            target: None,
            protocol: Some(proxy_protocol_name(proxy.resolved_protocol()).to_string()),
        });
    }
    for forward in host.local_forwards.iter().filter(|forward| forward.auto) {
        let name = forward.runtime_name("local-forward");
        stats::register_tunnel(stats::TunnelRegistration {
            id: stats::tunnel_id(host_name, stats::TunnelKind::LocalForward, &name),
            host_name: host_name.to_string(),
            name,
            kind: stats::TunnelKind::LocalForward,
            listen: forward.listen.to_string(),
            target: Some(forward.target.to_string()),
            protocol: None,
        });
    }
    for proxy in host.remote_proxies.iter().filter(|proxy| proxy.auto) {
        let name = proxy.runtime_name("remote-proxy");
        stats::register_tunnel(stats::TunnelRegistration {
            id: stats::tunnel_id(host_name, stats::TunnelKind::RemoteProxy, &name),
            host_name: host_name.to_string(),
            name,
            kind: stats::TunnelKind::RemoteProxy,
            listen: proxy.listen.to_string(),
            target: None,
            protocol: Some(proxy_protocol_name(proxy.resolved_protocol()).to_string()),
        });
    }
    for forward in host.remote_forwards.iter().filter(|forward| forward.auto) {
        let name = forward.runtime_name("remote-forward");
        stats::register_tunnel(stats::TunnelRegistration {
            id: stats::tunnel_id(host_name, stats::TunnelKind::RemoteForward, &name),
            host_name: host_name.to_string(),
            name,
            kind: stats::TunnelKind::RemoteForward,
            listen: forward.listen.to_string(),
            target: Some(forward.target.to_string()),
            protocol: None,
        });
    }
}

pub(crate) fn proxy_protocol_name(protocol: ProxyProtocol) -> &'static str {
    match protocol {
        ProxyProtocol::Socks5h => "SOCKS5H",
        ProxyProtocol::Http => "HTTP",
        ProxyProtocol::Mixed => "Mixed",
    }
}

fn listener_exit_result(
    result: Option<Result<anyhow::Result<()>, tokio::task::JoinError>>,
) -> anyhow::Result<()> {
    match result {
        Some(Ok(Ok(()))) => bail!("local forward exited unexpectedly"),
        Some(Ok(Err(error))) => Err(error),
        Some(Err(error)) => Err(error.into()),
        None => bail!("all local forwards exited unexpectedly"),
    }
}

fn build_ssh_host(
    host_name: &str,
    host: &ResolvedHostConfig,
    connectivity: ConnectivityHandle,
) -> anyhow::Result<Arc<dyn OutboundDialer>> {
    if !host.auto {
        bail!(
            "SSH host {} is not configured for automatic startup",
            host_name
        );
    }

    Ok(Arc::new(SshPoolDialer::start_with_connectivity(
        host_name.to_string(),
        host.runtime_pool(host_name),
        host.probe,
        connectivity,
    )?))
}

#[derive(Debug, Clone, Copy)]
struct ListenerRetryPolicy {
    initial: Duration,
    max: Duration,
}

#[derive(Clone)]
struct ProxyListenerRuntime {
    name: String,
    tunnel_id: String,
    listen: SocketAddr,
    protocol: ProxyProtocol,
    host_name: String,
    retry: ListenerRetryPolicy,
}

async fn run_proxy_listener(
    runtime: ProxyListenerRuntime,
    dialer: Arc<dyn OutboundDialer>,
) -> anyhow::Result<()> {
    let mut retry_delay = runtime.retry.initial;
    loop {
        match TcpListener::bind(runtime.listen).await {
            Ok(listener) => {
                stats::update_tunnel_status(
                    &runtime.tunnel_id,
                    stats::TunnelRuntimeStatus::Listening,
                    None,
                    None,
                );
                retry_delay = runtime.retry.initial;
                let error = match serve_proxy_listener(
                    listener,
                    runtime.clone(),
                    Arc::clone(&dialer),
                )
                .await
                {
                    Ok(()) => anyhow!("proxy listener exited unexpectedly"),
                    Err(error) => error,
                };
                record_local_listener_error(
                    &runtime.host_name,
                    &runtime.tunnel_id,
                    &runtime.name,
                    runtime.listen,
                    &error,
                );
            }
            Err(error) => record_local_listener_error(
                &runtime.host_name,
                &runtime.tunnel_id,
                &runtime.name,
                runtime.listen,
                &error,
            ),
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = next_listener_retry(retry_delay, runtime.retry.max);
    }
}

async fn serve_proxy_listener(
    listener: TcpListener,
    runtime: ProxyListenerRuntime,
    dialer: Arc<dyn OutboundDialer>,
) -> anyhow::Result<()> {
    info!(
        local_forward = %runtime.name,
        host_name = %runtime.host_name,
        listen = %runtime.listen,
        protocol = ?runtime.protocol,
        "proxy local forward bound"
    );

    loop {
        let (stream, peer_addr) = accept_local_connection(
            &listener,
            &runtime.host_name,
            &runtime.tunnel_id,
            &runtime.name,
            runtime.listen,
        )
        .await;
        let connection_id = next_connection_id();
        let local_forward_name = runtime.name.clone();
        let connection_tunnel_id = runtime.tunnel_id.clone();
        let host_name = runtime.host_name.clone();
        let dialer = Arc::clone(&dialer);
        let protocol = runtime.protocol;

        let connection_span = info_span!("proxy_connection", connection_id);
        tokio::spawn(
            async move {
                let connection = Arc::new(stats::LocalConnectionGuard::start(
                    stats::ConnectionRegistration {
                        id: connection_id,
                        host_name: host_name.clone(),
                        tunnel_id: connection_tunnel_id.clone(),
                        peer_address: peer_addr.to_string(),
                        target: None,
                        protocol: Some(proxy_protocol_name(protocol).to_string()),
                        session_id: None,
                    },
                ));
                if let Err(error) = handle_proxy_session(
                    stream,
                    protocol,
                    ProxySessionContext {
                        local_forward_name: local_forward_name.clone(),
                        peer_addr,
                        host_name: host_name.clone(),
                        stats_host_name: host_name.clone(),
                        tunnel_id: connection_tunnel_id.clone(),
                        connection_id,
                        connection_started: Instant::now(),
                        _connection_lifetime: Some(Arc::clone(&connection)),
                    },
                    dialer,
                )
                .await
                {
                    stats::record_connection_error(connection_id, &format!("{error:#}"), true);
                    stats::record_tunnel_error(
                        &host_name,
                        &connection_tunnel_id,
                        &format!("{error:#}"),
                    );
                    warn!(
                        local_forward = %local_forward_name,
                        %peer_addr,
                        %error,
                        "proxy session closed with an error"
                    );
                }
            }
            .instrument(connection_span),
        );
    }
}

#[derive(Clone)]
struct TcpListenerRuntime {
    name: String,
    tunnel_id: String,
    listen: SocketAddr,
    target_host: String,
    target_port: u16,
    host_name: String,
    retry: ListenerRetryPolicy,
}

async fn run_tcp_listener(
    runtime: TcpListenerRuntime,
    dialer: Arc<dyn OutboundDialer>,
) -> anyhow::Result<()> {
    let mut retry_delay = runtime.retry.initial;
    loop {
        match TcpListener::bind(runtime.listen).await {
            Ok(listener) => {
                stats::update_tunnel_status(
                    &runtime.tunnel_id,
                    stats::TunnelRuntimeStatus::Listening,
                    None,
                    None,
                );
                retry_delay = runtime.retry.initial;
                let error = match serve_tcp_listener(listener, runtime.clone(), Arc::clone(&dialer))
                    .await
                {
                    Ok(()) => anyhow!("TCP listener exited unexpectedly"),
                    Err(error) => error,
                };
                record_local_listener_error(
                    &runtime.host_name,
                    &runtime.tunnel_id,
                    &runtime.name,
                    runtime.listen,
                    &error,
                );
            }
            Err(error) => record_local_listener_error(
                &runtime.host_name,
                &runtime.tunnel_id,
                &runtime.name,
                runtime.listen,
                &error,
            ),
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = next_listener_retry(retry_delay, runtime.retry.max);
    }
}

async fn serve_tcp_listener(
    listener: TcpListener,
    forward: TcpListenerRuntime,
    dialer: Arc<dyn OutboundDialer>,
) -> anyhow::Result<()> {
    let target = TargetAddr::from_host_port(forward.target_host, forward.target_port);
    info!(
        local_forward = %forward.name,
        host_name = %forward.host_name,
        listen = %forward.listen,
        %target,
        "TCP local forward bound"
    );

    loop {
        let (mut local_stream, peer_addr) = accept_local_connection(
            &listener,
            &forward.host_name,
            &forward.tunnel_id,
            &forward.name,
            forward.listen,
        )
        .await;
        let connection_id = next_connection_id();
        let connection_name = forward.name.clone();
        let connection_tunnel_id = forward.tunnel_id.clone();
        let connection_host = forward.host_name.clone();
        let connection_target = target.clone();
        let dialer = Arc::clone(&dialer);
        let span = info_span!(
            "proxy_connection",
            connection_id,
            ssh_forward = "local",
            local_forward = %connection_name,
            host_name = %connection_host,
            %peer_addr,
            target = %connection_target
        );
        tokio::spawn(
            async move {
                let _connection =
                    stats::LocalConnectionGuard::start(stats::ConnectionRegistration {
                        id: connection_id,
                        host_name: connection_host.clone(),
                        tunnel_id: connection_tunnel_id.clone(),
                        peer_address: peer_addr.to_string(),
                        target: Some(connection_target.to_string()),
                        protocol: Some("TCP".to_string()),
                        session_id: None,
                    });
                let started = Instant::now();
                let ssh_stream = dialer
                    .dial(DialContext {
                        host_name: connection_host.clone(),
                        target: connection_target.clone(),
                        connection_id: Some(connection_id),
                    })
                    .await;
                let mut ssh_stream = match ssh_stream {
                    Ok(stream) => stream,
                    Err(error) => {
                        stats::record_connection_error(connection_id, &format!("{error:#}"), true);
                        stats::record_tunnel_error(
                            &connection_host,
                            &connection_tunnel_id,
                            &format!("{error:#}"),
                        );
                        warn!(
                            local_forward = %connection_name,
                            host_name = %connection_host,
                            target = %connection_target,
                            %peer_addr,
                            %error,
                            "SSH local TCP forward channel failed"
                        );
                        return Err(error);
                    }
                };
                stats::mark_connection_active(connection_id);
                let ssh_dial_ms = elapsed_ms(started);
                let relay_started = Instant::now();
                let recorder = stats::tunnel_transfer_recorder(
                    &connection_host,
                    &connection_tunnel_id,
                    connection_id,
                );
                let mut timed_ssh =
                    TimedIo::with_transfer_recorder(&mut ssh_stream, relay_started, recorder);
                let relay_result = copy_bidirectional(&mut local_stream, &mut timed_ssh).await;
                let timing = timed_ssh.timing();
                if let Err(error) = &relay_result {
                    stats::record_connection_error(connection_id, &error.to_string(), true);
                    stats::record_tunnel_error(
                        &connection_host,
                        &connection_tunnel_id,
                        &error.to_string(),
                    );
                    warn!(
                        local_forward = %connection_name,
                        host_name = %connection_host,
                        target = %connection_target,
                        %peer_addr,
                        %error,
                        "SSH local TCP forward relay failed"
                    );
                }
                let (client_to_ssh_bytes, ssh_to_client_bytes) = relay_result?;
                debug!(
                    target = %connection_target,
                    ssh_dial_ms,
                    first_client_data_ms = timing.first_write_ms.unwrap_or(-1.0),
                    first_upstream_byte_ms = timing.first_read_ms.unwrap_or(-1.0),
                    relay_duration_ms = timing.total_ms,
                    client_to_ssh_bytes,
                    ssh_to_client_bytes,
                    session_total_ms = elapsed_ms(started),
                    "SSH local TCP forward session finished"
                );
                anyhow::Ok(())
            }
            .instrument(span),
        );
    }
}

async fn accept_local_connection(
    listener: &TcpListener,
    host_name: &str,
    tunnel_id: &str,
    name: &str,
    listen: SocketAddr,
) -> (TcpStream, SocketAddr) {
    loop {
        match listener.accept().await {
            Ok(connection) => return connection,
            Err(error) => {
                let error = format!("listener at {listen} failed to accept a connection: {error}");
                stats::record_tunnel_error(host_name, tunnel_id, &error);
                warn!(
                    local_forward = %name,
                    %host_name,
                    %listen,
                    %error,
                    "local listener accept failed; retaining listener and retrying"
                );
                tokio::time::sleep(LOCAL_LISTENER_ACCEPT_RETRY_DELAY).await;
            }
        }
    }
}

fn record_local_listener_error(
    host_name: &str,
    tunnel_id: &str,
    name: &str,
    listen: SocketAddr,
    error: &impl std::fmt::Display,
) {
    let error = format!("failed to listen at {listen}: {error}");
    stats::update_tunnel_status(
        tunnel_id,
        stats::TunnelRuntimeStatus::Error,
        None,
        Some(error.clone()),
    );
    stats::record_tunnel_error(host_name, tunnel_id, &error);
    warn!(local_forward = %name, %host_name, %listen, %error, "local listener failed; retrying");
}

fn next_listener_retry(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

#[derive(Clone)]
pub(crate) struct ProxySessionContext {
    pub(crate) local_forward_name: String,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) host_name: String,
    pub(crate) stats_host_name: String,
    pub(crate) tunnel_id: String,
    pub(crate) connection_id: u64,
    pub(crate) connection_started: Instant,
    pub(crate) _connection_lifetime: Option<Arc<stats::LocalConnectionGuard>>,
}

pub(crate) async fn handle_proxy_session<S>(
    stream: S,
    protocol: ProxyProtocol,
    context: ProxySessionContext,
    dialer: Arc<dyn OutboundDialer>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let session_started = context.connection_started;
    let mut stream = BufReader::new(stream);
    let prefix = stream.fill_buf().await?;
    if prefix.is_empty() {
        return Ok(());
    }

    let detected = detect_protocol(protocol, prefix);
    stats::update_connection_route(
        context.connection_id,
        None,
        Some(
            match detected {
                DetectedProtocol::Socks5 => "SOCKS5H",
                DetectedProtocol::Http => "HTTP",
                DetectedProtocol::Unknown => "Unknown",
            }
            .to_string(),
        ),
    );
    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        ?detected,
        protocol_detect_ms = elapsed_ms(session_started),
        "accepted proxy session"
    );

    match detected {
        DetectedProtocol::Socks5 => {
            handle_socks5_session(&mut stream, &context, dialer.as_ref()).await
        }
        DetectedProtocol::Http => handle_http_connection(stream, context, dialer).await,
        DetectedProtocol::Unknown => bail!("unknown proxy protocol"),
    }
}

struct HttpBodyLogContext {
    direction: &'static str,
    session: ProxySessionContext,
    method: Method,
    uri: Uri,
    target: TargetAddr,
    status: Option<StatusCode>,
    body_start_offset_ms: f64,
    request_started: Instant,
}

impl HttpBodyLogContext {
    fn log(self, timing: BodyTiming) {
        debug!(
            local_forward = %self.session.local_forward_name,
            peer_addr = %self.session.peer_addr,
            host_name = %self.session.host_name,
            method = %self.method,
            uri = %self.uri,
            target = %self.target,
            body_direction = self.direction,
            status = self.status.map(|status| status.as_u16()).unwrap_or(0),
            body_start_offset_ms = self.body_start_offset_ms,
            body_first_data_ms = timing.first_data_ms.unwrap_or(-1.0),
            body_first_data_total_ms = timing
                .first_data_ms
                .map(|first_data_ms| self.body_start_offset_ms + first_data_ms)
                .unwrap_or(-1.0),
            body_duration_ms = timing.total_ms,
            body_bytes = timing.bytes,
            request_total_ms = elapsed_ms(self.request_started),
            outcome = timing.outcome,
            "HTTP body transfer finished"
        );
    }
}

struct TunnelLogContext<'a> {
    local_forward_name: &'a str,
    peer_addr: SocketAddr,
    host_name: &'a str,
    stats_host_name: &'a str,
    tunnel_id: &'a str,
    connection_id: u64,
    target: &'a TargetAddr,
    protocol: &'static str,
    session_started: Instant,
}

struct HttpProxyFailure {
    status: StatusCode,
    error: anyhow::Error,
}

impl HttpProxyFailure {
    fn bad_request(error: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: error.into(),
        }
    }

    fn bad_gateway(error: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            error: error.into(),
        }
    }
}

struct ForwardDestination {
    authority: Authority,
    target: TargetAddr,
    origin_form: Uri,
}

async fn handle_http_connection<S>(
    stream: S,
    context: ProxySessionContext,
    dialer: Arc<dyn OutboundDialer>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service_context = context.clone();
    let service = service_fn(move |request| {
        proxy_http_request(request, service_context.clone(), Arc::clone(&dialer))
    });
    let mut builder = server_http1::Builder::new();
    builder.max_buf_size(HTTP_MAX_BUFFER_SIZE);

    builder
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await
        .with_context(|| {
            format!(
                "HTTP proxy connection from {} on local forward {} failed",
                context.peer_addr, context.local_forward_name
            )
        })
}

async fn proxy_http_request(
    mut request: Request<Incoming>,
    context: ProxySessionContext,
    dialer: Arc<dyn OutboundDialer>,
) -> Result<Response<ProxyBody>, Infallible> {
    let request_started = Instant::now();
    let method = request.method().clone();
    let uri = request.uri().clone();
    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        host_name = %context.host_name,
        %method,
        %uri,
        connection_age_ms = elapsed_ms(context.connection_started),
        "HTTP proxy request received"
    );
    let result = if method == Method::CONNECT {
        proxy_connect(&mut request, &context, dialer, request_started).await
    } else {
        proxy_forward(request, &context, dialer, request_started).await
    };

    match result {
        Ok(response) => Ok(response),
        Err(failure) => {
            stats::record_tunnel_error(
                &context.stats_host_name,
                &context.tunnel_id,
                &format!("{:#}", failure.error),
            );
            stats::record_connection_error(
                context.connection_id,
                &format!("{:#}", failure.error),
                false,
            );
            warn!(
                local_forward = %context.local_forward_name,
                peer_addr = %context.peer_addr,
                host_name = %context.host_name,
                %method,
                %uri,
                status = failure.status.as_u16(),
                request_total_ms = elapsed_ms(request_started),
                error = %failure.error,
                "HTTP proxy request failed"
            );
            Ok(error_response(failure.status))
        }
    }
}

async fn proxy_connect(
    request: &mut Request<Incoming>,
    context: &ProxySessionContext,
    dialer: Arc<dyn OutboundDialer>,
    request_started: Instant,
) -> Result<Response<ProxyBody>, HttpProxyFailure> {
    let target = connect_target(request.uri())?;
    let ssh_dial_started = Instant::now();
    let upstream = match dialer
        .dial(DialContext {
            host_name: context.host_name.clone(),
            target: target.clone(),
            connection_id: Some(context.connection_id),
        })
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => {
            warn!(
                local_forward = %context.local_forward_name,
                peer_addr = %context.peer_addr,
                host_name = %context.host_name,
                %target,
                ssh_dial_ms = elapsed_ms(ssh_dial_started),
                outcome = "error",
                %error,
                "HTTP CONNECT SSH host dial finished"
            );
            return Err(HttpProxyFailure::bad_gateway(error));
        }
    };
    let ssh_dial_ms = elapsed_ms(ssh_dial_started);
    stats::update_connection_route(
        context.connection_id,
        Some(target.to_string()),
        Some("HTTP CONNECT".to_string()),
    );
    stats::mark_connection_active(context.connection_id);
    let on_upgrade = hyper::upgrade::on(request);
    let tunnel_context = context.clone();

    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        host_name = %context.host_name,
        %target,
        ssh_dial_ms,
        connect_setup_ms = elapsed_ms(request_started),
        "HTTP CONNECT upstream established"
    );

    tokio::spawn(
        async move {
            if let Err(error) = run_connect_tunnel(
                on_upgrade,
                upstream,
                &tunnel_context,
                &target,
                request_started,
            )
            .await
            {
                debug!(
                    local_forward = %tunnel_context.local_forward_name,
                    peer_addr = %tunnel_context.peer_addr,
                    host_name = %tunnel_context.host_name,
                    %target,
                    %error,
                    "HTTP CONNECT tunnel closed with an error"
                );
            }
        }
        .in_current_span(),
    );

    let mut response = Response::new(empty_body());
    *response.status_mut() = StatusCode::OK;
    Ok(response)
}

async fn run_connect_tunnel(
    on_upgrade: hyper::upgrade::OnUpgrade,
    mut upstream: BoxedProxyStream,
    context: &ProxySessionContext,
    target: &TargetAddr,
    request_started: Instant,
) -> anyhow::Result<()> {
    let upgrade_started = Instant::now();
    let upgraded = on_upgrade.await.context("HTTP CONNECT upgrade failed")?;
    let upgrade_wait_ms = elapsed_ms(upgrade_started);
    let mut client = TokioIo::new(upgraded);
    let relay_started = Instant::now();
    let recorder = stats::tunnel_transfer_recorder(
        &context.stats_host_name,
        &context.tunnel_id,
        context.connection_id,
    );
    let mut timed_upstream =
        TimedIo::with_transfer_recorder(&mut upstream, relay_started, recorder);
    let relay_result = copy_bidirectional(&mut client, &mut timed_upstream).await;
    let timing = timed_upstream.timing();
    let outcome = match &relay_result {
        Ok(_) => "completed",
        Err(error) => {
            stats::record_connection_error(context.connection_id, &error.to_string(), true);
            stats::record_tunnel_error(
                &context.stats_host_name,
                &context.tunnel_id,
                &error.to_string(),
            );
            "error"
        }
    };
    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        host_name = %context.host_name,
        %target,
        upgrade_wait_ms,
        first_client_data_ms = timing.first_write_ms.unwrap_or(-1.0),
        first_upstream_byte_ms = timing.first_read_ms.unwrap_or(-1.0),
        uploaded_bytes = timing.bytes_written,
        downloaded_bytes = timing.bytes_read,
        tunnel_duration_ms = timing.total_ms,
        request_total_ms = elapsed_ms(request_started),
        outcome,
        "HTTP CONNECT tunnel finished"
    );
    relay_result.map(|_| ()).map_err(Into::into)
}

async fn proxy_forward(
    mut request: Request<Incoming>,
    context: &ProxySessionContext,
    dialer: Arc<dyn OutboundDialer>,
    request_started: Instant,
) -> Result<Response<ProxyBody>, HttpProxyFailure> {
    let destination = forward_destination(&request)?;
    let target_parse_ms = elapsed_ms(request_started);
    let method = request.method().clone();
    let original_uri = request.uri().clone();

    *request.uri_mut() = destination.origin_form;
    remove_hop_by_hop_headers(request.headers_mut());
    append_via(request.headers_mut());
    request.headers_mut().insert(
        HOST,
        HeaderValue::from_str(destination.authority.as_str())
            .map_err(HttpProxyFailure::bad_request)?,
    );

    let request_body_log = HttpBodyLogContext {
        direction: "request",
        session: context.clone(),
        method: method.clone(),
        uri: original_uri.clone(),
        target: destination.target.clone(),
        status: None,
        body_start_offset_ms: 0.0,
        request_started,
    };
    let (request_parts, request_body) = request.into_parts();
    let request = Request::from_parts(
        request_parts,
        TimedBody::new(request_body, request_started, move |timing| {
            request_body_log.log(timing);
        }),
    );

    let ssh_dial_started = Instant::now();
    let upstream = match dialer
        .dial(DialContext {
            host_name: context.host_name.clone(),
            target: destination.target.clone(),
            connection_id: Some(context.connection_id),
        })
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => {
            warn!(
                local_forward = %context.local_forward_name,
                peer_addr = %context.peer_addr,
                host_name = %context.host_name,
                %method,
                uri = %original_uri,
                target = %destination.target,
                ssh_dial_ms = elapsed_ms(ssh_dial_started),
                outcome = "error",
                %error,
                "HTTP forward SSH host dial finished"
            );
            return Err(HttpProxyFailure::bad_gateway(error));
        }
    };
    let ssh_dial_ms = elapsed_ms(ssh_dial_started);
    stats::update_connection_route(
        context.connection_id,
        Some(destination.target.to_string()),
        Some("HTTP".to_string()),
    );
    let upstream_handshake_started = Instant::now();
    let recorder = stats::tunnel_transfer_recorder(
        &context.stats_host_name,
        &context.tunnel_id,
        context.connection_id,
    );
    let upstream = TimedIo::with_transfer_recorder(upstream, request_started, recorder);
    let (mut sender, connection) =
        match client_http1::handshake::<_, TimedBody<Incoming>>(TokioIo::new(upstream)).await {
            Ok(connection) => connection,
            Err(error) => {
                debug!(
                    local_forward = %context.local_forward_name,
                    peer_addr = %context.peer_addr,
                    host_name = %context.host_name,
                    %method,
                    uri = %original_uri,
                    target = %destination.target,
                    upstream_handshake_ms = elapsed_ms(upstream_handshake_started),
                    outcome = "error",
                    %error,
                    "HTTP upstream handshake finished"
                );
                return Err(HttpProxyFailure::bad_gateway(error));
            }
        };
    let upstream_handshake_ms = elapsed_ms(upstream_handshake_started);
    stats::mark_connection_active(context.connection_id);
    let connection_context = context.clone();
    let target = destination.target.clone();
    tokio::spawn(
        async move {
            if let Err(error) = connection.await {
                debug!(
                    local_forward = %connection_context.local_forward_name,
                    peer_addr = %connection_context.peer_addr,
                    host_name = %connection_context.host_name,
                    %target,
                    %error,
                    "HTTP upstream connection closed with an error"
                );
            }
        }
        .in_current_span(),
    );

    let response_wait_started = Instant::now();
    let mut response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(error) => {
            warn!(
                local_forward = %context.local_forward_name,
                peer_addr = %context.peer_addr,
                host_name = %context.host_name,
                %method,
                uri = %original_uri,
                target = %destination.target,
                upstream_response_wait_ms = elapsed_ms(response_wait_started),
                request_total_ms = elapsed_ms(request_started),
                outcome = "error",
                %error,
                "HTTP upstream response wait finished"
            );
            return Err(HttpProxyFailure::bad_gateway(error));
        }
    };
    let upstream_response_wait_ms = elapsed_ms(response_wait_started);
    let response_headers_ms = elapsed_ms(request_started);
    remove_hop_by_hop_headers(response.headers_mut());
    append_via(response.headers_mut());
    let status = response.status();

    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        host_name = %context.host_name,
        %method,
        uri = %original_uri,
        target = %destination.target,
        status = status.as_u16(),
        target_parse_ms,
        ssh_dial_ms,
        upstream_handshake_ms,
        upstream_response_wait_ms,
        response_headers_ms,
        "HTTP forward response headers received"
    );

    let response_body_started = Instant::now();
    let response_body_log = HttpBodyLogContext {
        direction: "response",
        session: context.clone(),
        method,
        uri: original_uri,
        target: destination.target,
        status: Some(status),
        body_start_offset_ms: response_headers_ms,
        request_started,
    };
    let (response_parts, response_body) = response.into_parts();
    let response_body = TimedBody::new(response_body, response_body_started, move |timing| {
        response_body_log.log(timing);
    });

    Ok(Response::from_parts(
        response_parts,
        response_body.boxed_unsync(),
    ))
}

fn connect_target(uri: &Uri) -> Result<TargetAddr, HttpProxyFailure> {
    let authority = uri
        .authority()
        .cloned()
        .or_else(|| Authority::from_str(&uri.to_string()).ok())
        .ok_or_else(|| {
            HttpProxyFailure::bad_request(anyhow!("CONNECT request is missing an authority"))
        })?;
    target_from_authority(&authority, 443)
}

fn forward_destination(
    request: &Request<Incoming>,
) -> Result<ForwardDestination, HttpProxyFailure> {
    if let Some(scheme) = request.uri().scheme_str()
        && !scheme.eq_ignore_ascii_case("http")
    {
        return Err(HttpProxyFailure::bad_request(anyhow!(
            "forward proxy URI scheme must be http; use CONNECT for {scheme}"
        )));
    }

    let authority = match request.uri().authority() {
        Some(authority) => authority.clone(),
        None => request
            .headers()
            .get(HOST)
            .ok_or_else(|| HttpProxyFailure::bad_request(anyhow!("HTTP request is missing Host")))?
            .to_str()
            .map_err(HttpProxyFailure::bad_request)?
            .parse::<Authority>()
            .map_err(HttpProxyFailure::bad_request)?,
    };
    let target = target_from_authority(&authority, 80)?;
    let origin_form = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/")
        .parse::<Uri>()
        .map_err(HttpProxyFailure::bad_request)?;

    Ok(ForwardDestination {
        authority,
        target,
        origin_form,
    })
}

fn target_from_authority(
    authority: &Authority,
    default_port: u16,
) -> Result<TargetAddr, HttpProxyFailure> {
    let port = authority.port_u16().unwrap_or(default_port);
    if port == 0 {
        return Err(HttpProxyFailure::bad_request(anyhow!(
            "target port must be greater than zero"
        )));
    }

    let host = authority.host().trim_matches(['[', ']']);
    if host.is_empty() {
        return Err(HttpProxyFailure::bad_request(anyhow!(
            "target host must not be empty"
        )));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(TargetAddr::Socket(SocketAddr::new(ip, port)));
    }

    Ok(TargetAddr::DomainPort {
        domain: host.to_string(),
        port,
    })
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let mut connection_headers = Vec::new();
    for value in headers.get_all(CONNECTION) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for name in value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                connection_headers.push(name);
            }
        }
    }
    for name in connection_headers {
        headers.remove(name);
    }

    headers.remove(CONNECTION);
    headers.remove(PROXY_AUTHENTICATE);
    headers.remove(PROXY_AUTHORIZATION);
    headers.remove(TE);
    headers.remove(TRAILER);
    headers.remove(TRANSFER_ENCODING);
    headers.remove(UPGRADE);
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
}

fn append_via(headers: &mut HeaderMap) {
    headers.append(VIA, HeaderValue::from_static("1.1 stk"));
}

fn empty_body() -> ProxyBody {
    Full::new(Bytes::new())
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

fn error_response(status: StatusCode) -> Response<ProxyBody> {
    let reason = status.canonical_reason().unwrap_or("Proxy Error");
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from(format!("{reason}\n")))
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        )
        .expect("static proxy error response must be valid")
}

async fn handle_socks5_session<S>(
    stream: &mut S,
    context: &ProxySessionContext,
    dialer: &dyn OutboundDialer,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let handshake_started = Instant::now();
    let target = match accept_socks5(stream).await {
        Ok(target) => target,
        Err(error) => {
            debug!(
                local_forward = %context.local_forward_name,
                peer_addr = %context.peer_addr,
                socks_handshake_ms = elapsed_ms(handshake_started),
                outcome = "error",
                %error,
                "SOCKS5 handshake finished"
            );
            if let Some(reply) = socks5_handshake_reply(&error) {
                let _ = write_socks5_reply(stream, reply).await;
            }
            return Err(error.into());
        }
    };
    let socks_handshake_ms = elapsed_ms(handshake_started);

    let ssh_dial_started = Instant::now();
    let mut upstream = match dialer
        .dial(DialContext {
            host_name: context.host_name.clone(),
            target: target.clone(),
            connection_id: Some(context.connection_id),
        })
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            warn!(
                local_forward = %context.local_forward_name,
                peer_addr = %context.peer_addr,
                host_name = %context.host_name,
                %target,
                socks_handshake_ms,
                ssh_dial_ms = elapsed_ms(ssh_dial_started),
                setup_total_ms = elapsed_ms(context.connection_started),
                outcome = "error",
                %error,
                "SOCKS5 tunnel setup finished"
            );
            let _ = write_socks5_reply(stream, socks5_dial_reply(&error)).await;
            return Err(error);
        }
    };
    let ssh_dial_ms = elapsed_ms(ssh_dial_started);
    stats::update_connection_route(
        context.connection_id,
        Some(target.to_string()),
        Some("SOCKS5H".to_string()),
    );
    stats::mark_connection_active(context.connection_id);

    write_socks5_reply(stream, SOCKS5_REPLY_SUCCEEDED).await?;
    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        host_name = %context.host_name,
        %target,
        socks_handshake_ms,
        ssh_dial_ms,
        setup_total_ms = elapsed_ms(context.connection_started),
        outcome = "connected",
        "SOCKS5 tunnel setup finished"
    );
    relay(
        stream,
        &mut upstream,
        TunnelLogContext {
            local_forward_name: &context.local_forward_name,
            peer_addr: context.peer_addr,
            host_name: &context.host_name,
            stats_host_name: &context.stats_host_name,
            tunnel_id: &context.tunnel_id,
            connection_id: context.connection_id,
            target: &target,
            protocol: "socks5",
            session_started: context.connection_started,
        },
    )
    .await
}

async fn relay<S>(
    client: &mut S,
    ssh_stream: &mut BoxedProxyStream,
    context: TunnelLogContext<'_>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let relay_started = Instant::now();
    let recorder = stats::tunnel_transfer_recorder(
        context.stats_host_name,
        context.tunnel_id,
        context.connection_id,
    );
    let mut timed_ssh_stream = TimedIo::with_transfer_recorder(ssh_stream, relay_started, recorder);
    let relay_result = copy_bidirectional(client, &mut timed_ssh_stream).await;
    let timing = timed_ssh_stream.timing();
    let outcome = match &relay_result {
        Ok(_) => "completed",
        Err(error) => {
            stats::record_connection_error(context.connection_id, &error.to_string(), true);
            stats::record_tunnel_error(
                context.stats_host_name,
                context.tunnel_id,
                &error.to_string(),
            );
            "error"
        }
    };
    debug!(
        local_forward = %context.local_forward_name,
        peer_addr = %context.peer_addr,
        host_name = %context.host_name,
        target = %context.target,
        protocol = context.protocol,
        first_client_data_ms = timing.first_write_ms.unwrap_or(-1.0),
        first_upstream_byte_ms = timing.first_read_ms.unwrap_or(-1.0),
        uploaded_bytes = timing.bytes_written,
        downloaded_bytes = timing.bytes_read,
        relay_duration_ms = timing.total_ms,
        session_total_ms = elapsed_ms(context.session_started),
        outcome,
        "proxy tunnel finished"
    );
    relay_result.map(|_| ()).map_err(Into::into)
}

fn socks5_handshake_reply(error: &InboundError) -> Option<u8> {
    match error {
        InboundError::UnsupportedSocksCommand(_) => Some(SOCKS5_REPLY_COMMAND_NOT_SUPPORTED),
        InboundError::UnsupportedSocksAddressType(_)
        | InboundError::InvalidSocksDomain
        | InboundError::InvalidTargetPort => Some(SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED),
        InboundError::MalformedSocksRequest(_) => Some(SOCKS5_REPLY_GENERAL_FAILURE),
        _ => None,
    }
}

fn socks5_dial_reply(error: &anyhow::Error) -> u8 {
    for cause in error.chain() {
        let Some(io_error) = cause.downcast_ref::<std::io::Error>() else {
            continue;
        };
        return match io_error.kind() {
            ErrorKind::PermissionDenied => 0x02,
            ErrorKind::TimedOut => 0x04,
            ErrorKind::ConnectionRefused => 0x05,
            _ => SOCKS5_REPLY_GENERAL_FAILURE,
        };
    }

    SOCKS5_REPLY_GENERAL_FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyProtocol;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        task::JoinHandle,
        time::{Duration, timeout},
    };

    #[test]
    fn connection_ids_are_monotonically_unique() {
        let first = next_connection_id();
        let second = next_connection_id();
        assert!(second > first);
    }

    async fn spawn_echo_server() -> (std::net::SocketAddr, JoinHandle<anyhow::Result<()>>) {
        let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo_listener.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = echo_listener.accept().await?;
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                stream.write_all(&buffer[..read]).await?;
            }
            anyhow::Ok(())
        });
        (echo_addr, echo_task)
    }

    async fn spawn_proxy(
        protocol: ProxyProtocol,
    ) -> (std::net::SocketAddr, JoinHandle<anyhow::Result<()>>) {
        let local_forward = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = local_forward.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let (stream, peer_addr) = local_forward.accept().await?;
            let dialer: Arc<dyn OutboundDialer> = Arc::new(crate::outbound::LocalTcpDialer);
            handle_proxy_session(
                stream,
                protocol,
                ProxySessionContext {
                    local_forward_name: "test-mixed".to_string(),
                    peer_addr,
                    host_name: "test-local-tcp".to_string(),
                    stats_host_name: "test-host".to_string(),
                    tunnel_id: "test-tunnel".to_string(),
                    connection_id: next_connection_id(),
                    connection_started: Instant::now(),
                    _connection_lifetime: None,
                },
                dialer,
            )
            .await
        });
        (proxy_addr, proxy_task)
    }

    async fn wait_for_session_tasks(
        proxy_task: JoinHandle<anyhow::Result<()>>,
        echo_task: JoinHandle<anyhow::Result<()>>,
    ) {
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .expect("proxy task timed out")
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), echo_task)
            .await
            .expect("echo task timed out")
            .unwrap()
            .unwrap();
    }

    async fn read_http_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        loop {
            head.push(stream.read_u8().await.unwrap());
            if head.ends_with(b"\r\n\r\n") {
                return head;
            }
        }
    }

    async fn spawn_http_origin(
        response_body: &'static [u8],
    ) -> (SocketAddr, JoinHandle<anyhow::Result<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            loop {
                if let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                {
                    let headers = std::str::from_utf8(&request[..header_end])?;
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }

                let mut buffer = [0_u8; 1024];
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    anyhow::bail!("origin client closed before request completed");
                }
                request.extend_from_slice(&buffer[..read]);
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close, X-Origin-Hop\r\nX-Origin-Hop: remove-me\r\nProxy-Authenticate: Basic realm=origin\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(response_body).await?;
            Ok(request)
        });
        (address, task)
    }

    async fn wait_for_proxy_task(proxy_task: JoinHandle<anyhow::Result<()>>) {
        timeout(Duration::from_secs(2), proxy_task)
            .await
            .expect("proxy task timed out")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn tcp_local_forward_uses_host_dialer() {
        let (echo_addr, echo_task) = spawn_echo_server().await;
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let forward_addr = reserved.local_addr().unwrap();
        let dialer: Arc<dyn OutboundDialer> = Arc::new(crate::outbound::LocalTcpDialer);
        let forward_task = tokio::spawn(serve_tcp_listener(
            reserved,
            TcpListenerRuntime {
                name: "test-tcp".to_string(),
                tunnel_id: "test-group/local-forward/test-tcp".to_string(),
                listen: forward_addr,
                target_host: echo_addr.ip().to_string(),
                target_port: echo_addr.port(),
                host_name: "test-group".to_string(),
                retry: ListenerRetryPolicy {
                    initial: Duration::from_millis(10),
                    max: Duration::from_millis(20),
                },
            },
            dialer,
        ));

        let mut client = timeout(Duration::from_secs(2), async {
            loop {
                match TcpStream::connect(forward_addr).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("TCP local forward did not bind");
        client.write_all(b"host-forward").await.unwrap();
        let mut echoed = [0_u8; 12];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"host-forward");
        drop(client);

        timeout(Duration::from_secs(2), echo_task)
            .await
            .expect("echo task timed out")
            .unwrap()
            .unwrap();
        forward_task.abort();
    }

    #[tokio::test]
    async fn local_listener_reports_bind_failure_and_recovers() {
        let blocker = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = blocker.local_addr().unwrap();
        let listener_task = tokio::spawn(run_tcp_listener(
            TcpListenerRuntime {
                name: "recovering-listener".to_string(),
                tunnel_id: "listener-recovery/local-forward/test".to_string(),
                listen,
                target_host: "127.0.0.1".to_string(),
                target_port: 1,
                host_name: "listener-recovery".to_string(),
                retry: ListenerRetryPolicy {
                    initial: Duration::from_millis(10),
                    max: Duration::from_millis(40),
                },
            },
            Arc::new(crate::outbound::LocalTcpDialer),
        ));

        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(blocker);
        let recovered = timeout(Duration::from_secs(2), async {
            loop {
                match TcpStream::connect(listen).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
        .await;
        assert!(
            recovered.is_ok(),
            "listener did not recover after port release"
        );
        listener_task.abort();
    }

    #[tokio::test]
    async fn mixed_socks5_forwards_over_test_tcp_dialer() {
        let (echo_addr, echo_task) = spawn_echo_server().await;
        let (proxy_addr, proxy_task) = spawn_proxy(ProxyProtocol::Mixed).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0_u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        let mut connect_request = vec![0x05, 0x01, 0x00, 0x01];
        connect_request.extend_from_slice(&[127, 0, 0, 1]);
        connect_request.extend_from_slice(&echo_addr.port().to_be_bytes());
        client.write_all(&connect_request).await.unwrap();

        let mut connect_reply = [0_u8; 10];
        client.read_exact(&mut connect_reply).await.unwrap();
        assert_eq!(connect_reply[1], SOCKS5_REPLY_SUCCEEDED);

        const PAYLOAD: &[u8] = b"stk-echo";
        client.write_all(PAYLOAD).await.unwrap();
        let mut echoed = vec![0_u8; PAYLOAD.len()];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, PAYLOAD);
        drop(client);

        wait_for_session_tasks(proxy_task, echo_task).await;
    }

    #[tokio::test]
    async fn mixed_http_connect_forwards_over_test_tcp_dialer() {
        let (echo_addr, echo_task) = spawn_echo_server().await;
        let (proxy_addr, proxy_task) = spawn_proxy(ProxyProtocol::Mixed).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                format!(
                    "CONNECT {echo_addr} HTTP/1.1\r\nHost: {echo_addr}\r\nProxy-Connection: keep-alive\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let response = read_http_head(&mut client).await;
        assert!(response.starts_with(b"HTTP/1.1 200 "));

        client.write_all(b"http-tunnel").await.unwrap();
        let mut echoed = [0_u8; 11];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"http-tunnel");
        drop(client);

        wait_for_session_tasks(proxy_task, echo_task).await;
    }

    #[tokio::test]
    async fn mixed_http_forward_get_rewrites_uri_and_filters_proxy_headers() {
        let (origin_addr, origin_task) = spawn_http_origin(b"forward-get").await;
        let (proxy_addr, proxy_task) = spawn_proxy(ProxyProtocol::Mixed).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                format!(
                    "GET http://{origin_addr}/hello?name=stk HTTP/1.1\r\nHost: wrong.example\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic secret\r\nConnection: close, X-Remove-Me\r\nX-Remove-Me: secret\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response_text = String::from_utf8_lossy(&response);
        assert!(response_text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response_text.ends_with("forward-get"));
        assert!(response_text.to_ascii_lowercase().contains("via: 1.1 stk"));
        assert!(!response_text.to_ascii_lowercase().contains("x-origin-hop"));
        assert!(
            !response_text
                .to_ascii_lowercase()
                .contains("proxy-authenticate")
        );

        let request = timeout(Duration::from_secs(2), origin_task)
            .await
            .expect("origin task timed out")
            .unwrap()
            .unwrap();
        let request_text = String::from_utf8(request).unwrap();
        let lower_request = request_text.to_ascii_lowercase();
        assert!(request_text.starts_with("GET /hello?name=stk HTTP/1.1\r\n"));
        assert!(lower_request.contains(&format!("host: {origin_addr}").to_ascii_lowercase()));
        assert!(lower_request.contains("via: 1.1 stk"));
        assert!(!lower_request.contains("proxy-connection"));
        assert!(!lower_request.contains("proxy-authorization"));
        assert!(!lower_request.contains("x-remove-me"));

        wait_for_proxy_task(proxy_task).await;
    }

    #[tokio::test]
    async fn mixed_http_forward_post_streams_request_body() {
        let (origin_addr, origin_task) = spawn_http_origin(b"forward-post").await;
        let (proxy_addr, proxy_task) = spawn_proxy(ProxyProtocol::Mixed).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                format!(
                    "POST http://{origin_addr}/submit HTTP/1.1\r\nHost: {origin_addr}\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello-proxy"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).ends_with("forward-post"));

        let request = timeout(Duration::from_secs(2), origin_task)
            .await
            .expect("origin task timed out")
            .unwrap()
            .unwrap();
        let request_text = String::from_utf8(request).unwrap();
        assert!(request_text.starts_with("POST /submit HTTP/1.1\r\n"));
        assert!(request_text.ends_with("hello-proxy"));

        wait_for_proxy_task(proxy_task).await;
    }
}
