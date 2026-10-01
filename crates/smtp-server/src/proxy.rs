//! Reading a PROXY protocol (HAProxy v1 and v2) header from a freshly accepted connection.
//!
//! Only use this on a listener that is reachable by trusted proxies exclusively: the header is
//! plain data and anyone who may send it can claim any client address.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use haproxy_protocol::{AsyncReadError, ProxyHdrV1, ProxyHdrV2, RemoteAddress};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The addresses announced by a PROXY header. Both are `None` when the proxy did not announce
/// any (v2 `LOCAL`, v1 `UNKNOWN`, v2 `UNSPEC`/`UNIX`): keep using the TCP peer address then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyHeader {
    /// The original client.
    pub source: Option<SocketAddr>,
    /// The address the client connected to on the proxy.
    pub destination: Option<SocketAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("i/o error reading PROXY header: {0}")]
    Io(#[from] io::Error),
    #[error("invalid PROXY header: {0}")]
    Invalid(&'static str),
    #[error("timed out waiting for PROXY header")]
    Timeout,
}

/// The client address of a connection from a proxy: reads the PROXY header within `timeout` and
/// returns its source address, or `tcp_addr` (the proxy itself) if it announced none. IPv4-mapped
/// IPv6 addresses are returned as plain IPv4.
pub async fn read_proxy_peer<S>(
    stream: &mut S,
    tcp_addr: SocketAddr,
    timeout: Duration,
) -> Result<SocketAddr, ProxyError>
where
    S: AsyncRead + Unpin,
{
    let header = tokio::time::timeout(timeout, read_proxy_header(stream))
        .await
        .map_err(|_| ProxyError::Timeout)??;
    let peer = header.source.unwrap_or(tcp_addr);
    Ok(SocketAddr::new(peer.ip().to_canonical(), peer.port()))
}

/// Reads exactly one PROXY header (v1 or v2) from the start of `stream`; nothing after it is
/// consumed. The caller should bound this with a timeout.
///
/// Known limitation of the underlying parser: a bare v1 `PROXY UNKNOWN\r\n` is shorter than the
/// 32 bytes it reads up front, so it only completes if more data follows (and then fails).
pub async fn read_proxy_header<S>(stream: &mut S) -> Result<ProxyHeader, ProxyError>
where
    S: AsyncRead + Unpin,
{
    let first = [stream.read_u8().await?];
    let chained = (&first[..]).chain(stream);
    let remote = match first[0] {
        0x0D => ProxyHdrV2::parse_from_read(chained)
            .await
            .map(|(_, hdr)| hdr.to_remote_addr()),
        b'P' => ProxyHdrV1::parse_from_read(chained)
            .await
            .map(|(_, hdr)| hdr.to_remote_addr()),
        _ => return Err(ProxyError::Invalid("not a PROXY header")),
    }
    .map_err(|e| match e {
        AsyncReadError::Io(e) => ProxyError::Io(e),
        AsyncReadError::Invalid => ProxyError::Invalid("malformed header"),
        AsyncReadError::UnableToComplete => ProxyError::Invalid("incomplete header"),
        AsyncReadError::RequestTooLarge => ProxyError::Invalid("header too large"),
        AsyncReadError::InconsistentRead => ProxyError::Invalid("inconsistent header length"),
    })?;

    let (source, destination) = match remote {
        RemoteAddress::Local | RemoteAddress::Invalid => (None, None),
        RemoteAddress::TcpV4 { src, dst } => (Some(src.into()), Some(dst.into())),
        RemoteAddress::TcpV6 { src, dst } => (Some(src.into()), Some(dst.into())),
        RemoteAddress::UdpV4 { .. } | RemoteAddress::UdpV6 { .. } => {
            return Err(ProxyError::Invalid("unsupported transport"));
        }
    };
    Ok(ProxyHeader {
        source,
        destination,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIG: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
    const EHLO: &[u8] = b"EHLO x\r\n";

    fn v2(cmd: u8, fam: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = SIG.to_vec();
        b.extend([0x20 | cmd, fam]);
        b.extend((payload.len() as u16).to_be_bytes());
        b.extend(payload);
        b
    }

    /// Parses `header` followed by an `EHLO` and checks that the `EHLO` was left unread.
    async fn parse(header: &[u8]) -> Result<ProxyHeader, ProxyError> {
        let data = [header, EHLO].concat();
        let mut s = &data[..];
        let res = read_proxy_header(&mut s).await;
        if res.is_ok() {
            assert_eq!(s, EHLO, "bytes after the header must stay unread");
        }
        res
    }

    fn addr(s: &str) -> Option<SocketAddr> {
        Some(s.parse().unwrap())
    }

    #[tokio::test]
    async fn v1_tcp4() {
        let h = parse(b"PROXY TCP4 192.0.2.1 198.51.100.7 56324 25\r\n")
            .await
            .unwrap();
        assert_eq!(h.source, addr("192.0.2.1:56324"));
        assert_eq!(h.destination, addr("198.51.100.7:25"));
    }

    #[tokio::test]
    async fn v1_tcp6() {
        let h = parse(b"PROXY TCP6 2001:db8::1 2001:db8::2 56324 25\r\n")
            .await
            .unwrap();
        assert_eq!(h.source, addr("[2001:db8::1]:56324"));
        assert_eq!(h.destination, addr("[2001:db8::2]:25"));
    }

    #[tokio::test]
    async fn v2_inet() {
        let mut p = vec![192, 0, 2, 1, 198, 51, 100, 7];
        p.extend(56324u16.to_be_bytes());
        p.extend(25u16.to_be_bytes());
        let h = parse(&v2(1, 0x11, &p)).await.unwrap();
        assert_eq!(h.source, addr("192.0.2.1:56324"));
        assert_eq!(h.destination, addr("198.51.100.7:25"));
    }

    #[tokio::test]
    async fn v2_inet6_with_tlvs() {
        let src: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
        let dst: std::net::Ipv6Addr = "2001:db8::2".parse().unwrap();
        let mut p = src.octets().to_vec();
        p.extend(dst.octets());
        p.extend(56324u16.to_be_bytes());
        p.extend(25u16.to_be_bytes());
        // a NOOP TLV and an unknown one
        p.extend([0x04, 0x00, 0x02, 0xAA, 0xBB, 0xE0, 0x00, 0x01, 0xCC]);
        let h = parse(&v2(1, 0x21, &p)).await.unwrap();
        assert_eq!(h.source, addr("[2001:db8::1]:56324"));
        assert_eq!(h.destination, addr("[2001:db8::2]:25"));
    }

    #[tokio::test]
    async fn v2_local_and_unspec_have_no_addresses() {
        let none = ProxyHeader {
            source: None,
            destination: None,
        };
        assert_eq!(parse(&v2(0, 0x00, &[])).await.unwrap(), none);
        assert_eq!(parse(&v2(1, 0x00, &[])).await.unwrap(), none);
        assert_eq!(parse(&v2(1, 0x31, &[0; 216])).await.unwrap(), none);
    }

    #[tokio::test]
    async fn rejects_garbage_and_wrong_transport() {
        for bad in [
            &b"EHLO x\r\n"[..],
            b"PROXY BOGUS 192.0.2.1 198.51.100.7 56324 25\r\n",
            b"PROXY TCP4 not-an-ip 198.51.100.7 56324 25\r\n",
            b"\r\n\r\n\0\r\nQUIT\n\x10\x11\x00\x0c",
        ] {
            assert!(
                matches!(parse(bad).await, Err(ProxyError::Invalid(_))),
                "{bad:?}"
            );
        }
        let udp = parse(&v2(1, 0x12, &[0; 12])).await;
        assert!(matches!(
            udp,
            Err(ProxyError::Invalid("unsupported transport"))
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_v2() {
        let res = parse(&v2(1, 0x11, &[0; 600])).await;
        assert!(matches!(res, Err(ProxyError::Invalid("header too large"))));
    }

    #[tokio::test]
    async fn truncated_is_an_io_error() {
        for cut in [0, 1, 8, 20] {
            let full = b"PROXY TCP4 192.0.2.1 198.51.100.7 56324 25\r\n";
            let mut s = &full[..cut];
            let res = read_proxy_header(&mut s).await;
            assert!(matches!(res, Err(ProxyError::Io(_))), "cut {cut}: {res:?}");
        }
    }

    #[tokio::test]
    async fn v1_unknown_stalls_without_trailing_data() {
        let mut s = &b"PROXY UNKNOWN\r\n"[..];
        assert!(matches!(
            read_proxy_header(&mut s).await,
            Err(ProxyError::Io(_))
        ));
    }

    #[tokio::test]
    async fn peer_falls_back_to_tcp_addr_and_canonicalizes() {
        let tcp: SocketAddr = "10.1.2.3:4000".parse().unwrap();
        let t = Duration::from_secs(1);
        let mut local = &v2(0, 0x00, &[])[..];
        assert_eq!(read_proxy_peer(&mut local, tcp, t).await.unwrap(), tcp);
        let mut mapped = &b"PROXY TCP6 ::ffff:192.0.2.1 2001:db8::2 56324 25\r\n"[..];
        assert_eq!(
            read_proxy_peer(&mut mapped, tcp, t).await.unwrap(),
            "192.0.2.1:56324".parse().unwrap()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn peer_times_out() {
        let (mut idle, _keep_open) = tokio::io::duplex(64);
        let tcp = "10.1.2.3:4000".parse().unwrap();
        let res = read_proxy_peer(&mut idle, tcp, Duration::from_secs(5)).await;
        assert!(matches!(res, Err(ProxyError::Timeout)), "{res:?}");
    }
}
