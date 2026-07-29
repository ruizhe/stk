use crate::{config::ProxyProtocol, outbound::TargetAddr};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_AUTH_NONE: u8 = 0x00;
const SOCKS5_AUTH_UNACCEPTABLE: u8 = 0xff;
const SOCKS5_COMMAND_CONNECT: u8 = 0x01;

pub const SOCKS5_REPLY_SUCCEEDED: u8 = 0x00;
pub const SOCKS5_REPLY_GENERAL_FAILURE: u8 = 0x01;
pub const SOCKS5_REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
pub const SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedProtocol {
    Socks5,
    Http,
    Unknown,
}

#[derive(Debug, Error)]
pub enum InboundError {
    #[error("proxy I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported SOCKS version: {0}")]
    UnsupportedSocksVersion(u8),
    #[error("SOCKS5 client offered no supported authentication method")]
    NoAcceptableSocksAuth,
    #[error("malformed SOCKS5 request: {0}")]
    MalformedSocksRequest(&'static str),
    #[error("unsupported SOCKS5 command: {0}")]
    UnsupportedSocksCommand(u8),
    #[error("unsupported SOCKS5 address type: {0}")]
    UnsupportedSocksAddressType(u8),
    #[error("invalid SOCKS5 domain name")]
    InvalidSocksDomain,
    #[error("target port must be greater than zero")]
    InvalidTargetPort,
}

pub fn detect_protocol(configured: ProxyProtocol, bytes: &[u8]) -> DetectedProtocol {
    match configured {
        ProxyProtocol::Socks5h => DetectedProtocol::Socks5,
        ProxyProtocol::Http => DetectedProtocol::Http,
        ProxyProtocol::Mixed => detect_mixed(bytes),
    }
}

fn detect_mixed(bytes: &[u8]) -> DetectedProtocol {
    if bytes.first() == Some(&SOCKS5_VERSION) {
        return DetectedProtocol::Socks5;
    }

    if looks_like_http(bytes) {
        return DetectedProtocol::Http;
    }

    DetectedProtocol::Unknown
}

fn looks_like_http(bytes: &[u8]) -> bool {
    bytes.first().is_some_and(u8::is_ascii_uppercase)
}

pub async fn accept_socks5<S>(stream: &mut S) -> Result<TargetAddr, InboundError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting[0] != SOCKS5_VERSION {
        return Err(InboundError::UnsupportedSocksVersion(greeting[0]));
    }

    let mut methods = vec![0_u8; greeting[1] as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&SOCKS5_AUTH_NONE) {
        stream
            .write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_UNACCEPTABLE])
            .await?;
        return Err(InboundError::NoAcceptableSocksAuth);
    }

    stream
        .write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_NONE])
        .await?;

    let mut request = [0_u8; 4];
    stream.read_exact(&mut request).await?;
    if request[0] != SOCKS5_VERSION {
        return Err(InboundError::UnsupportedSocksVersion(request[0]));
    }
    if request[2] != 0x00 {
        return Err(InboundError::MalformedSocksRequest(
            "reserved byte must be zero",
        ));
    }
    if request[1] != SOCKS5_COMMAND_CONNECT {
        return Err(InboundError::UnsupportedSocksCommand(request[1]));
    }

    read_socks5_target(stream, request[3]).await
}

async fn read_socks5_target<S>(stream: &mut S, address_type: u8) -> Result<TargetAddr, InboundError>
where
    S: AsyncRead + Unpin,
{
    let target = match address_type {
        0x01 => {
            let mut octets = [0_u8; 4];
            stream.read_exact(&mut octets).await?;
            let port = read_port(stream).await?;
            TargetAddr::Socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        0x03 => {
            let domain_len = stream.read_u8().await? as usize;
            if domain_len == 0 {
                return Err(InboundError::InvalidSocksDomain);
            }

            let mut domain = vec![0_u8; domain_len];
            stream.read_exact(&mut domain).await?;
            let domain = String::from_utf8(domain).map_err(|_| InboundError::InvalidSocksDomain)?;
            let port = read_port(stream).await?;
            TargetAddr::DomainPort { domain, port }
        }
        0x04 => {
            let mut octets = [0_u8; 16];
            stream.read_exact(&mut octets).await?;
            let port = read_port(stream).await?;
            TargetAddr::Socket(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        other => return Err(InboundError::UnsupportedSocksAddressType(other)),
    };

    if target.port() == 0 {
        return Err(InboundError::InvalidTargetPort);
    }

    Ok(target)
}

async fn read_port<S>(stream: &mut S) -> Result<u16, InboundError>
where
    S: AsyncRead + Unpin,
{
    let mut port = [0_u8; 2];
    stream.read_exact(&mut port).await?;
    Ok(u16::from_be_bytes(port))
}

pub async fn write_socks5_reply<S>(stream: &mut S, reply: u8) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(&[SOCKS5_VERSION, reply, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[test]
    fn mixed_detects_socks5() {
        assert_eq!(
            detect_protocol(ProxyProtocol::Mixed, &[0x05, 0x01, 0x00]),
            DetectedProtocol::Socks5
        );
    }

    #[test]
    fn mixed_detects_partial_http_method() {
        assert_eq!(
            detect_protocol(ProxyProtocol::Mixed, b"C"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(ProxyProtocol::Mixed, b"CONNE"),
            DetectedProtocol::Http
        );
    }

    #[test]
    fn mixed_detects_http_connect() {
        assert_eq!(
            detect_protocol(
                ProxyProtocol::Mixed,
                b"CONNECT example.com:443 HTTP/1.1\r\n"
            ),
            DetectedProtocol::Http
        );
    }

    #[test]
    fn mixed_detects_other_http_methods() {
        assert_eq!(
            detect_protocol(ProxyProtocol::Mixed, b"TRACE / HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
    }

    #[test]
    fn fixed_protocol_bypasses_detection() {
        assert_eq!(
            detect_protocol(ProxyProtocol::Socks5h, b"anything"),
            DetectedProtocol::Socks5
        );
    }

    #[tokio::test]
    async fn socks5_parses_domain_target_without_resolving_it() {
        let (mut client, mut server) = duplex(1024);
        let mut request = vec![0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x03, 11];
        request.extend_from_slice(b"example.com");
        request.extend_from_slice(&443_u16.to_be_bytes());
        client.write_all(&request).await.unwrap();

        let target = accept_socks5(&mut server).await.unwrap();
        assert_eq!(
            target,
            TargetAddr::DomainPort {
                domain: "example.com".to_string(),
                port: 443,
            }
        );

        let mut method_reply = [0_u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);
    }

    #[tokio::test]
    async fn socks5_parses_ipv4_target() {
        let (mut client, mut server) = duplex(1024);
        client
            .write_all(&[
                0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0x1f, 0x90,
            ])
            .await
            .unwrap();

        let target = accept_socks5(&mut server).await.unwrap();
        assert_eq!(
            target,
            TargetAddr::Socket("127.0.0.1:8080".parse().unwrap())
        );
    }
}
