use crate::stats::elapsed_ms;
use anyhow::Context;
use async_trait::async_trait;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    time::Instant,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, lookup_host},
};
use tracing::debug;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TargetAddr {
    DomainPort { domain: String, port: u16 },
    Socket(SocketAddr),
}

impl TargetAddr {
    pub(crate) fn from_host_port(host: impl Into<String>, port: u16) -> Self {
        let host = host.into();
        match host.trim_matches(['[', ']']).parse::<IpAddr>() {
            Ok(ip) => Self::Socket(SocketAddr::new(ip, port)),
            Err(_) => Self::DomainPort { domain: host, port },
        }
    }

    pub(crate) fn port(&self) -> u16 {
        match self {
            Self::DomainPort { port, .. } => *port,
            Self::Socket(addr) => addr.port(),
        }
    }
}

impl fmt::Display for TargetAddr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DomainPort { domain, port } => write!(formatter, "{domain}:{port}"),
            Self::Socket(addr) => addr.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DialContext {
    pub host_name: String,
    pub target: TargetAddr,
    pub connection_id: Option<u64>,
}

pub(crate) trait ProxyStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ProxyStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(crate) type BoxedProxyStream = Box<dyn ProxyStream>;

#[async_trait]
pub(crate) trait OutboundDialer: Send + Sync {
    async fn dial(&self, context: DialContext) -> anyhow::Result<BoxedProxyStream>;
}

// Used only when the SSH client must connect back to a target on the stk host.
#[derive(Debug, Default)]
pub(crate) struct LocalTcpDialer;

#[async_trait]
impl OutboundDialer for LocalTcpDialer {
    async fn dial(&self, context: DialContext) -> anyhow::Result<BoxedProxyStream> {
        let dial_started = Instant::now();
        let stream = match &context.target {
            TargetAddr::DomainPort { domain, port } => {
                let dns_started = Instant::now();
                let addresses = match lookup_host((domain.as_str(), *port)).await {
                    Ok(addresses) => addresses.collect::<Vec<_>>(),
                    Err(error) => {
                        debug!(
                            host_name = %context.host_name,
                            target = %context.target,
                            dns_lookup_ms = elapsed_ms(dns_started),
                            outcome = "error",
                            %error,
                            "local target DNS lookup finished"
                        );
                        return Err(error).with_context(|| {
                            format!(
                                "{} failed to resolve local target {}",
                                context.host_name, context.target
                            )
                        });
                    }
                };
                let dns_lookup_ms = elapsed_ms(dns_started);
                if addresses.is_empty() {
                    debug!(
                        host_name = %context.host_name,
                        target = %context.target,
                        dns_lookup_ms,
                        resolved_address_count = 0,
                        outcome = "no_addresses",
                        "local target DNS lookup finished"
                    );
                    anyhow::bail!(
                        "{} resolved no addresses for local target {}",
                        context.host_name,
                        context.target
                    );
                }
                debug!(
                    host_name = %context.host_name,
                    target = %context.target,
                    dns_lookup_ms,
                    resolved_address_count = addresses.len(),
                    outcome = "resolved",
                    "local target DNS lookup finished"
                );

                let tcp_started = Instant::now();
                let stream = TcpStream::connect(addresses.as_slice()).await;
                debug!(
                    host_name = %context.host_name,
                    target = %context.target,
                    tcp_connect_ms = elapsed_ms(tcp_started),
                    outcome = if stream.is_ok() { "connected" } else { "error" },
                    "local target TCP connect attempt finished"
                );
                stream
            }
            TargetAddr::Socket(addr) => {
                let tcp_started = Instant::now();
                let stream = TcpStream::connect(addr).await;
                debug!(
                    host_name = %context.host_name,
                    target = %context.target,
                    tcp_connect_ms = elapsed_ms(tcp_started),
                    outcome = if stream.is_ok() { "connected" } else { "error" },
                    "local target TCP connect attempt finished"
                );
                stream
            }
        }
        .with_context(|| {
            format!(
                "{} failed to connect to local target {}",
                context.host_name, context.target
            )
        })?;
        stream.set_nodelay(true).with_context(|| {
            format!(
                "{} failed to configure local target connection {}",
                context.host_name, context.target
            )
        })?;
        debug!(
            host_name = %context.host_name,
            target = %context.target,
            peer_addr = ?stream.peer_addr().ok(),
            local_addr = ?stream.local_addr().ok(),
            local_dial_total_ms = elapsed_ms(dial_started),
            "local target connection established"
        );

        Ok(Box::new(stream))
    }
}
