use crate::{
    ConfigScope,
    config::ControlConfig,
    default_config_directory,
    reload::ReloadHandle,
    stats::{self, RuntimeSnapshot, TrafficHistorySnapshot},
};
use anyhow::{Context as _, bail};
use http_body_util::{BodyExt as _, Full, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Bytes, Frame, Incoming, SizeHint},
    client::conn::http1 as client_http1,
    header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue},
    server::conn::http1 as server_http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(not(any(unix, windows)))]
const DEFAULT_TCP_PORT: u16 = 19090;
const STATUS_STREAM_HEARTBEAT: Duration = Duration::from_secs(1);

type ControlBody = UnsyncBoxBody<Bytes, Infallible>;
type StatusChangeFuture = Pin<Box<dyn Future<Output = watch::Receiver<u64>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEndpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
    NamedPipe(String),
}

impl ControlEndpoint {
    pub fn from_config(config: &ControlConfig, scope: ConfigScope) -> anyhow::Result<Self> {
        config
            .endpoint
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(anyhow::Error::msg)
            .map(|endpoint| endpoint.unwrap_or_else(|| default_control_endpoint(scope)))
    }

    fn tcp_connect_address(address: SocketAddr) -> SocketAddr {
        if !address.ip().is_unspecified() {
            return address;
        }
        match address.ip() {
            IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port()),
            IpAddr::V6(_) => {
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), address.port())
            }
        }
    }
}

impl fmt::Display for ControlEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(address) => write!(formatter, "tcp:{address}"),
            Self::Unix(path) => write!(formatter, "unix:{}", path.display()),
            Self::NamedPipe(name) => write!(formatter, "pipe:{name}"),
        }
    }
}

impl FromStr for ControlEndpoint {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if let Some(address) = value
            .strip_prefix("tcp://")
            .or_else(|| value.strip_prefix("tcp:"))
        {
            let address = if address.chars().all(|character| character.is_ascii_digit()) {
                let port = address
                    .parse::<u16>()
                    .map_err(|error| format!("invalid TCP port {address}: {error}"))?;
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
            } else {
                address
                    .parse::<SocketAddr>()
                    .map_err(|error| format!("invalid TCP address {address}: {error}"))?
            };
            if address.port() == 0 {
                return Err("control TCP port must not be zero".to_string());
            }
            return Ok(Self::Tcp(address));
        }
        if let Some(path) = value
            .strip_prefix("unix://")
            .or_else(|| value.strip_prefix("unix:"))
        {
            if path.trim().is_empty() {
                return Err("Unix socket path must not be empty".to_string());
            }
            return Ok(Self::Unix(expand_tilde(Path::new(path))));
        }
        if let Some(name) = value
            .strip_prefix("pipe://")
            .or_else(|| value.strip_prefix("pipe:"))
        {
            if name.trim().is_empty() {
                return Err("named pipe name must not be empty".to_string());
            }
            return Ok(Self::NamedPipe(name.to_string()));
        }
        Err("expected tcp:<port-or-address>, unix:<path>, or pipe:<name>".to_string())
    }
}

pub fn default_control_endpoint(scope: ConfigScope) -> ControlEndpoint {
    #[cfg(unix)]
    {
        let path = match scope {
            ConfigScope::User => default_config_directory(scope).join("control.sock"),
            ConfigScope::System => system_runtime_directory().join("control.sock"),
        };
        ControlEndpoint::Unix(path)
    }

    #[cfg(windows)]
    {
        let name = match scope {
            ConfigScope::User => {
                let username = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
                format!("stk-{username}")
            }
            ConfigScope::System => "stk-system".to_string(),
        };
        ControlEndpoint::NamedPipe(name)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = scope;
        ControlEndpoint::Tcp(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            DEFAULT_TCP_PORT,
        ))
    }
}

#[cfg(target_os = "linux")]
fn system_runtime_directory() -> PathBuf {
    PathBuf::from("/run/stk")
}

#[cfg(all(unix, not(target_os = "linux")))]
fn system_runtime_directory() -> PathBuf {
    PathBuf::from("/var/run/stk")
}

fn expand_tilde(path: &Path) -> PathBuf {
    let path_text = path.to_string_lossy();
    if path_text == "~" {
        return home_directory();
    }
    if let Some(suffix) = path_text
        .strip_prefix("~/")
        .or_else(|| path_text.strip_prefix("~\\"))
    {
        return home_directory().join(suffix);
    }
    path.to_path_buf()
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}

trait ControlStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> ControlStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}
type BoxedControlStream = Box<dyn ControlStream>;

pub(crate) struct ControlListener {
    endpoint: ControlEndpoint,
    inner: ControlListenerInner,
}

enum ControlListenerInner {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
    #[cfg(windows)]
    NamedPipe(NamedPipeServer),
}

impl ControlListener {
    pub(crate) async fn bind(endpoint: ControlEndpoint) -> anyhow::Result<Self> {
        let inner = match &endpoint {
            ControlEndpoint::Tcp(address) => {
                if !address.ip().is_loopback() {
                    warn!(%address, "control API is listening on a non-loopback TCP address without authentication");
                }
                ControlListenerInner::Tcp(
                    TcpListener::bind(address)
                        .await
                        .with_context(|| format!("failed to bind control API at {endpoint}"))?,
                )
            }
            ControlEndpoint::Unix(path) => {
                #[cfg(unix)]
                {
                    bind_unix_listener(path).await?
                }
                #[cfg(not(unix))]
                {
                    bail!("Unix domain sockets are not supported on this platform");
                }
            }
            ControlEndpoint::NamedPipe(name) => {
                #[cfg(windows)]
                {
                    let path = named_pipe_path(name);
                    let server = ServerOptions::new()
                        .first_pipe_instance(true)
                        .create(&path)
                        .with_context(|| format!("failed to create control named pipe {path}"))?;
                    ControlListenerInner::NamedPipe(server)
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    bail!("Windows named pipes are not supported on this platform");
                }
            }
        };
        info!(%endpoint, "runtime control API bound");
        Ok(Self { endpoint, inner })
    }

    async fn accept(&mut self) -> anyhow::Result<(BoxedControlStream, String)> {
        match &mut self.inner {
            ControlListenerInner::Tcp(listener) => {
                let (stream, peer) = listener.accept().await?;
                Ok((Box::new(stream), peer.to_string()))
            }
            #[cfg(unix)]
            ControlListenerInner::Unix(listener) => {
                let (stream, peer) = listener.accept().await?;
                Ok((Box::new(stream), format!("{peer:?}")))
            }
            #[cfg(windows)]
            ControlListenerInner::NamedPipe(server) => {
                server.connect().await?;
                let pipe_name = match &self.endpoint {
                    ControlEndpoint::NamedPipe(name) => named_pipe_path(name),
                    _ => unreachable!("named pipe listener must have a named pipe endpoint"),
                };
                let connected = std::mem::replace(
                    server,
                    ServerOptions::new().create(&pipe_name).with_context(|| {
                        format!("failed to create the next control named pipe {pipe_name}")
                    })?,
                );
                Ok((Box::new(connected), pipe_name))
            }
        }
    }
}

#[cfg(unix)]
async fn bind_unix_listener(path: &Path) -> anyhow::Result<ControlListenerInner> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create control socket directory {}",
                parent.display()
            )
        })?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to secure control socket {}", path.display()))?;
            Ok(ControlListenerInner::Unix(listener))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path).await {
                Ok(_) => bail!("runtime control endpoint {path:?} is already in use"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path).with_context(|| {
                        format!("failed to remove stale control socket {}", path.display())
                    })?;
                    let listener = UnixListener::bind(path).with_context(|| {
                        format!("failed to bind control API at unix:{}", path.display())
                    })?;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                        .with_context(|| {
                            format!("failed to secure control socket {}", path.display())
                        })?;
                    Ok(ControlListenerInner::Unix(listener))
                }
                Err(error) => Err(error).with_context(|| {
                    format!(
                        "control socket {} exists but cannot be reached",
                        path.display()
                    )
                }),
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to bind control API at unix:{}", path.display())),
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let ControlEndpoint::Unix(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) async fn serve_control(
    mut listener: ControlListener,
    reload_handle: ReloadHandle,
) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let reload_handle = reload_handle.clone();
        tokio::spawn(async move {
            let service =
                service_fn(move |request| control_response(request, reload_handle.clone()));
            if let Err(error) = server_http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                debug!(%peer, %error, "runtime control API connection closed with an error");
            }
        });
    }
}

struct RuntimeStatusBody {
    interval: tokio::time::Interval,
    status_change: StatusChangeFuture,
    finished: bool,
}

impl RuntimeStatusBody {
    fn new() -> Self {
        let mut interval = tokio::time::interval(STATUS_STREAM_HEARTBEAT);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            interval,
            status_change: wait_for_status_change(stats::subscribe_runtime_status_changes()),
            finished: false,
        }
    }
}

impl Body for RuntimeStatusBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if let Poll::Ready(receiver) = self.status_change.as_mut().poll(context) {
            self.status_change = wait_for_status_change(receiver);
            self.interval.reset();
            return self.snapshot_frame();
        }
        match self.interval.poll_tick(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => self.snapshot_frame(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

impl RuntimeStatusBody {
    fn snapshot_frame(&mut self) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match serde_json::to_vec(&stats::runtime_snapshot()) {
            Ok(mut snapshot) => {
                snapshot.push(b'\n');
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(snapshot)))))
            }
            Err(_) => {
                stats::record_error();
                self.finished = true;
                Poll::Ready(None)
            }
        }
    }
}

fn wait_for_status_change(mut receiver: watch::Receiver<u64>) -> StatusChangeFuture {
    Box::pin(async move {
        let _ = receiver.changed().await;
        receiver
    })
}

async fn control_response(
    request: Request<Incoming>,
    reload_handle: ReloadHandle,
) -> Result<Response<ControlBody>, Infallible> {
    let method = request.method();
    let path = request.uri().path();
    if method == Method::POST && path == "/v1/connections/capture/start" {
        stats::set_connection_capture_recording(true);
        return Ok(text_response(
            StatusCode::OK,
            "connection capture started\n",
        ));
    }
    if method == Method::POST && path == "/v1/connections/capture/stop" {
        stats::set_connection_capture_recording(false);
        return Ok(text_response(
            StatusCode::OK,
            "connection capture stopped\n",
        ));
    }
    if method == Method::DELETE && path == "/v1/connections" {
        stats::clear_captured_connections();
        return Ok(text_response(
            StatusCode::OK,
            "captured connections cleared\n",
        ));
    }
    if method == Method::POST && path == "/v1/connections/auto-clear/enable" {
        stats::set_connection_capture_auto_clear_closed(true);
        return Ok(text_response(
            StatusCode::OK,
            "connection auto-clear enabled\n",
        ));
    }
    if method == Method::POST && path == "/v1/connections/auto-clear/disable" {
        stats::set_connection_capture_auto_clear_closed(false);
        return Ok(text_response(
            StatusCode::OK,
            "connection auto-clear disabled\n",
        ));
    }
    if request.method() == Method::POST && request.uri().path() == "/v1/reload" {
        let (status, body) = if reload_handle.request_reload() {
            (StatusCode::ACCEPTED, "reload requested\n")
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "reload control unavailable\n",
            )
        };
        let mut response = Response::new(full_body(Bytes::from_static(body.as_bytes())));
        *response.status_mut() = status;
        return Ok(response);
    }
    if request.method() == Method::GET && request.uri().path() == "/v1/traffic-history" {
        return Ok(json_response(&stats::traffic_history_snapshot()));
    }
    if request.method() == Method::GET && request.uri().path() == "/v1/status/stream" {
        let mut response = Response::new(RuntimeStatusBody::new().boxed_unsync());
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
        );
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return Ok(response);
    }
    if request.method() != Method::GET || request.uri().path() != "/v1/status" {
        let mut response = Response::new(full_body(Bytes::from_static(b"not found\n")));
        *response.status_mut() = StatusCode::NOT_FOUND;
        return Ok(response);
    }

    Ok(json_response(&stats::runtime_snapshot()))
}

fn text_response(status: StatusCode, body: &'static str) -> Response<ControlBody> {
    let mut response = Response::new(full_body(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    response
}

fn json_response<T: serde::Serialize>(value: &T) -> Response<ControlBody> {
    match serde_json::to_vec_pretty(value) {
        Ok(status) => {
            let mut response = Response::new(full_body(Bytes::from(status)));
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            response
        }
        Err(error) => {
            stats::record_error();
            let mut response = Response::new(full_body(Bytes::from(format!(
                "failed to encode runtime status: {error}\n"
            ))));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        }
    }
}

fn full_body(bytes: Bytes) -> ControlBody {
    Full::new(bytes).boxed_unsync()
}

pub async fn fetch_runtime_snapshot(endpoint: &ControlEndpoint) -> anyhow::Result<RuntimeSnapshot> {
    let response = send_request(
        endpoint,
        b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
    )
    .await?;
    let (status, body) = split_http_response(&response)?;
    if !status.contains(" 200 ") {
        bail!("control API returned {status}");
    }
    serde_json::from_str(body).context("control API returned invalid status JSON")
}

pub struct RuntimeSnapshotSubscription {
    body: Incoming,
    buffer: Vec<u8>,
    connection: JoinHandle<()>,
}

impl RuntimeSnapshotSubscription {
    pub async fn recv(&mut self) -> anyhow::Result<Option<RuntimeSnapshot>> {
        loop {
            if let Some(snapshot) = self.take_buffered_snapshot()? {
                return Ok(Some(snapshot));
            }
            let Some(frame) = self.body.frame().await else {
                if self.buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    let snapshot = serde_json::from_slice(&self.buffer)
                        .context("control status stream ended with invalid JSON")?;
                    self.buffer.clear();
                    return Ok(Some(snapshot));
                }
                return Ok(None);
            };
            let frame = frame.context("control status stream failed")?;
            if let Ok(data) = frame.into_data() {
                self.buffer.extend_from_slice(&data);
            }
        }
    }

    fn take_buffered_snapshot(&mut self) -> anyhow::Result<Option<RuntimeSnapshot>> {
        let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        let line = self.buffer.drain(..=newline).collect::<Vec<_>>();
        let line = line[..line.len().saturating_sub(1)]
            .strip_suffix(b"\r")
            .unwrap_or(&line[..line.len().saturating_sub(1)]);
        if line.iter().all(u8::is_ascii_whitespace) {
            return self.take_buffered_snapshot();
        }
        serde_json::from_slice(line)
            .context("control status stream returned invalid JSON")
            .map(Some)
    }
}

impl Drop for RuntimeSnapshotSubscription {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

pub async fn subscribe_runtime_snapshots(
    endpoint: &ControlEndpoint,
) -> anyhow::Result<RuntimeSnapshotSubscription> {
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
        .await
        .with_context(|| format!("failed to start control status stream at {endpoint}"))?;
    let endpoint_for_task = endpoint.clone();
    let connection = tokio::spawn(async move {
        if let Err(error) = connection.await {
            debug!(endpoint = %endpoint_for_task, %error, "control status stream connection closed");
        }
    });
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/status/stream")
        .header("host", "localhost")
        .header("accept", "application/x-ndjson")
        .body(Full::new(Bytes::new()))
        .context("failed to build control status stream request")?;
    let response = sender
        .send_request(request)
        .await
        .with_context(|| format!("failed to subscribe to control status stream at {endpoint}"))?;
    if response.status() != StatusCode::OK {
        bail!(
            "runtime control status stream returned HTTP {}",
            response.status()
        );
    }
    Ok(RuntimeSnapshotSubscription {
        body: response.into_body(),
        buffer: Vec::new(),
        connection,
    })
}

pub async fn request_runtime_reload(endpoint: &ControlEndpoint) -> anyhow::Result<()> {
    let response = send_request(
        endpoint,
        b"POST /v1/reload HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    )
    .await?;
    let (status, _) = split_http_response(&response)?;
    if !status.contains(" 202 ") {
        bail!("runtime control API returned {status}");
    }
    Ok(())
}

pub async fn request_connection_capture_recording(
    endpoint: &ControlEndpoint,
    recording: bool,
) -> anyhow::Result<()> {
    let path = if recording {
        "/v1/connections/capture/start"
    } else {
        "/v1/connections/capture/stop"
    };
    request_connection_control(endpoint, "POST", path).await
}

pub async fn request_clear_captured_connections(endpoint: &ControlEndpoint) -> anyhow::Result<()> {
    request_connection_control(endpoint, "DELETE", "/v1/connections").await
}

pub async fn request_connection_capture_auto_clear_closed(
    endpoint: &ControlEndpoint,
    enabled: bool,
) -> anyhow::Result<()> {
    let path = if enabled {
        "/v1/connections/auto-clear/enable"
    } else {
        "/v1/connections/auto-clear/disable"
    };
    request_connection_control(endpoint, "POST", path).await
}

async fn request_connection_control(
    endpoint: &ControlEndpoint,
    method: &str,
    path: &str,
) -> anyhow::Result<()> {
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    let response = send_request(endpoint, request.as_bytes()).await?;
    let (status, _) = split_http_response(&response)?;
    if !status.contains(" 200 ") {
        bail!("runtime control API returned {status}");
    }
    Ok(())
}

pub async fn fetch_traffic_history(
    endpoint: &ControlEndpoint,
) -> anyhow::Result<TrafficHistorySnapshot> {
    let response = send_request(
        endpoint,
        b"GET /v1/traffic-history HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
    )
    .await?;
    let (status, body) = split_http_response(&response)?;
    if !status.contains(" 200 ") {
        bail!("control API returned {status}");
    }
    serde_json::from_str(body).context("control API returned invalid traffic history JSON")
}

async fn send_request(endpoint: &ControlEndpoint, request: &[u8]) -> anyhow::Result<String> {
    let mut stream = connect(endpoint).await?;
    stream.write_all(request).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    String::from_utf8(response).context("control API response was not UTF-8")
}

async fn connect(endpoint: &ControlEndpoint) -> anyhow::Result<BoxedControlStream> {
    match endpoint {
        ControlEndpoint::Tcp(address) => {
            let connect_address = ControlEndpoint::tcp_connect_address(*address);
            Ok(Box::new(
                TcpStream::connect(connect_address).await.with_context(|| {
                    format!("failed to connect to runtime control API at {endpoint}")
                })?,
            ))
        }
        ControlEndpoint::Unix(path) => {
            #[cfg(unix)]
            {
                Ok(Box::new(UnixStream::connect(path).await.with_context(
                    || format!("failed to connect to runtime control API at {endpoint}"),
                )?))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                bail!("Unix domain sockets are not supported on this platform");
            }
        }
        ControlEndpoint::NamedPipe(name) => {
            #[cfg(windows)]
            {
                let path = named_pipe_path(name);
                for attempt in 0..40 {
                    match ClientOptions::new().open(&path) {
                        Ok(client) => return Ok(Box::new(client)),
                        Err(error) if error.raw_os_error() == Some(231) && attempt < 39 => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("failed to connect to runtime control named pipe {path}")
                            });
                        }
                    }
                }
                unreachable!("named pipe retry loop must return")
            }
            #[cfg(not(windows))]
            {
                let _ = name;
                bail!("Windows named pipes are not supported on this platform");
            }
        }
    }
}

fn split_http_response(response: &str) -> anyhow::Result<(&str, &str)> {
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .context("control API returned an incomplete HTTP response")?;
    Ok((headers.lines().next().unwrap_or_default(), body))
}

#[cfg(windows)]
fn named_pipe_path(name: &str) -> String {
    if name.starts_with(r"\\.\pipe\") {
        name.to_string()
    } else {
        format!(r"\\.\pipe\{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reload::ReloadControl;

    #[test]
    fn parses_all_endpoint_kinds() {
        assert_eq!(
            "tcp:19090".parse::<ControlEndpoint>().unwrap(),
            ControlEndpoint::Tcp("127.0.0.1:19090".parse().unwrap())
        );
        assert_eq!(
            "tcp:[::]:19091".parse::<ControlEndpoint>().unwrap(),
            ControlEndpoint::Tcp("[::]:19091".parse().unwrap())
        );
        assert_eq!(
            "unix:/tmp/stk.sock".parse::<ControlEndpoint>().unwrap(),
            ControlEndpoint::Unix(PathBuf::from("/tmp/stk.sock"))
        );
        assert_eq!(
            "pipe:stk-test".parse::<ControlEndpoint>().unwrap(),
            ControlEndpoint::NamedPipe("stk-test".to_string())
        );
    }

    #[tokio::test]
    async fn tcp_status_and_reload_are_available() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let control = ReloadControl::new();
        let server = tokio::spawn(serve_control(
            ControlListener {
                endpoint: ControlEndpoint::Tcp(address),
                inner: ControlListenerInner::Tcp(listener),
            },
            control.handle(),
        ));
        let runtime = stats::RuntimeGuard::start(2, 3, 4);
        let endpoint = ControlEndpoint::Tcp(address);

        let output = fetch_runtime_snapshot(&endpoint).await.unwrap();
        assert!(output.running);
        let mut subscription = subscribe_runtime_snapshots(&endpoint).await.unwrap();
        let _pushed = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("status stream did not push a snapshot")
            .unwrap()
            .expect("status stream ended unexpectedly");
        request_connection_capture_recording(&endpoint, true)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), subscription.recv())
            .await
            .expect("status change did not trigger an immediate pushed snapshot")
            .unwrap()
            .expect("status stream ended unexpectedly");
        let history = fetch_traffic_history(&endpoint).await.unwrap();
        assert_eq!(history.retention_hours, 24);
        assert_eq!(history.bucket_seconds, 60);
        request_connection_capture_auto_clear_closed(&endpoint, true)
            .await
            .unwrap();
        let output = fetch_runtime_snapshot(&endpoint).await.unwrap();
        assert!(output.connection_capture.recording);
        assert!(output.connection_capture.auto_clear_closed);
        request_clear_captured_connections(&endpoint).await.unwrap();
        request_connection_capture_recording(&endpoint, false)
            .await
            .unwrap();
        request_connection_capture_auto_clear_closed(&endpoint, false)
            .await
            .unwrap();
        let output = fetch_runtime_snapshot(&endpoint).await.unwrap();
        assert!(!output.connection_capture.recording);
        assert!(!output.connection_capture.auto_clear_closed);
        request_runtime_reload(&endpoint).await.unwrap();

        drop(runtime);
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_status_endpoint_is_available() {
        let directory =
            std::env::temp_dir().join(format!("stk-control-test-{}", std::process::id()));
        let path = directory.join("control.sock");
        let endpoint = ControlEndpoint::Unix(path.clone());
        let listener = ControlListener::bind(endpoint.clone()).await.unwrap();
        let control = ReloadControl::new();
        let server = tokio::spawn(serve_control(listener, control.handle()));
        let runtime = stats::RuntimeGuard::start(1, 0, 0);

        let output = fetch_runtime_snapshot(&endpoint).await.unwrap();
        assert!(output.running);

        drop(runtime);
        server.abort();
        let _ = std::fs::remove_dir_all(directory);
    }
}
