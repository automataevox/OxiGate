//! PROXY protocol v1 / v2 with buffered peek (does not consume non-PROXY traffic).

use std::io::{self, IoSlice};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

const V2_SIG: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

#[derive(Debug, Clone)]
pub struct ProxyHeader {
    pub src: SocketAddr,
    pub dst: SocketAddr,
}

/// Stream wrapper that can prefix leftover bytes after PROXY parsing.
pub struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    prefix_pos: usize,
}

impl<S> PrefixedStream<S> {
    pub fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self {
            inner,
            prefix,
            prefix_pos: 0,
        }
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix_pos < self.prefix.len() {
            let rest = &self.prefix[self.prefix_pos..];
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            self.prefix_pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Read PROXY header if present. On non-PROXY traffic returns bytes in the prefix buffer.
pub async fn maybe_read_proxy_header<S>(
    mut stream: S,
) -> anyhow::Result<(Option<ProxyHeader>, PrefixedStream<S>)>
where
    S: AsyncRead + Unpin,
{
    let mut first = [0u8; 16];
    let mut filled = 0usize;

    while filled < 6 {
        let n = stream.read(&mut first[filled..]).await?;
        if n == 0 {
            return Ok((None, PrefixedStream::new(stream, first[..filled].to_vec())));
        }
        filled += n;
    }

    // v1
    if &first[..6] == b"PROXY " {
        let (header, leftover) = parse_v1(&mut stream, &first[..filled]).await?;
        return Ok((Some(header), PrefixedStream::new(stream, leftover)));
    }

    // need 12 bytes for v2 sig
    while filled < 12 {
        let n = stream.read(&mut first[filled..]).await?;
        if n == 0 {
            return Ok((None, PrefixedStream::new(stream, first[..filled].to_vec())));
        }
        filled += n;
    }

    if first[..12] == V2_SIG {
        while filled < 16 {
            let n = stream.read(&mut first[filled..]).await?;
            if n == 0 {
                anyhow::bail!("EOF in PROXY v2 header");
            }
            filled += n;
        }
        let header = parse_v2(&mut stream, &first[..16]).await?;
        return Ok((Some(header), PrefixedStream::new(stream, Vec::new())));
    }

    // Not PROXY – put bytes back
    Ok((None, PrefixedStream::new(stream, first[..filled].to_vec())))
}

async fn parse_v1<S>(stream: &mut S, first: &[u8]) -> anyhow::Result<(ProxyHeader, Vec<u8>)>
where
    S: AsyncRead + Unpin,
{
    let mut buf = first.to_vec();
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = std::str::from_utf8(&buf[..pos])
                .map_err(|_| anyhow::anyhow!("invalid utf8 in PROXY v1"))?;
            let leftover = buf[pos + 2..].to_vec();
            return Ok((parse_v1_line(line)?, leftover));
        }
        if buf.len() > 108 {
            anyhow::bail!("PROXY v1 header too long");
        }
        let mut b = [0u8; 1];
        let n = stream.read(&mut b).await?;
        if n == 0 {
            anyhow::bail!("EOF in PROXY v1 header");
        }
        buf.push(b[0]);
    }
}

fn parse_v1_line(line: &str) -> anyhow::Result<ProxyHeader> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "PROXY" {
        anyhow::bail!("malformed PROXY v1: {line}");
    }
    if parts[1] == "UNKNOWN" {
        return Ok(ProxyHeader {
            src: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            dst: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        });
    }
    if parts.len() < 6 {
        anyhow::bail!("malformed PROXY v1: {line}");
    }
    Ok(ProxyHeader {
        src: SocketAddr::new(parts[2].parse()?, parts[4].parse()?),
        dst: SocketAddr::new(parts[3].parse()?, parts[5].parse()?),
    })
}

async fn parse_v2<S>(stream: &mut S, hdr: &[u8]) -> anyhow::Result<ProxyHeader>
where
    S: AsyncRead + Unpin,
{
    let ver_cmd = hdr[12];
    let family = hdr[13];
    let len = u16::from_be_bytes([hdr[14], hdr[15]]) as usize;
    let mut rest = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut rest).await?;
    }

    if ver_cmd & 0x0F == 0x00 {
        return Ok(ProxyHeader {
            src: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            dst: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        });
    }

    match family {
        0x11 if rest.len() >= 12 => Ok(ProxyHeader {
            src: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(rest[0], rest[1], rest[2], rest[3])),
                u16::from_be_bytes([rest[8], rest[9]]),
            ),
            dst: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(rest[4], rest[5], rest[6], rest[7])),
                u16::from_be_bytes([rest[10], rest[11]]),
            ),
        }),
        0x21 if rest.len() >= 36 => {
            let mut s = [0u8; 16];
            let mut d = [0u8; 16];
            s.copy_from_slice(&rest[0..16]);
            d.copy_from_slice(&rest[16..32]);
            Ok(ProxyHeader {
                src: SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(s)),
                    u16::from_be_bytes([rest[32], rest[33]]),
                ),
                dst: SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(d)),
                    u16::from_be_bytes([rest[34], rest[35]]),
                ),
            })
        }
        _ => anyhow::bail!("unsupported PROXY v2 family {family:#x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn v1_parse() {
        let data = b"PROXY TCP4 1.2.3.4 5.6.7.8 12345 80\r\nGET / HTTP/1.1\r\n";
        let (hdr, mut stream) = maybe_read_proxy_header(Cursor::new(data.to_vec()))
            .await
            .unwrap();
        let hdr = hdr.unwrap();
        assert_eq!(hdr.src.ip().to_string(), "1.2.3.4");
        assert_eq!(hdr.src.port(), 12345);
        let mut rest = String::new();
        stream.read_to_string(&mut rest).await.unwrap();
        assert!(rest.starts_with("GET /"));
    }

    #[tokio::test]
    async fn non_proxy_bytes_preserved() {
        let data = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let (hdr, mut stream) = maybe_read_proxy_header(Cursor::new(data.to_vec()))
            .await
            .unwrap();
        assert!(hdr.is_none());
        let mut rest = String::new();
        stream.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest.as_bytes(), data.as_slice());
    }
}
