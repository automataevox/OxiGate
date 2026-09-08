use std::env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 3\r\nconnection: keep-alive\r\n\r\nok\n";
const MAX_REQUEST_BYTES: usize = 16 * 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let port = env::args().nth(1).expect("usage: oxigate-benchmark-backend PORT");
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).await?;

    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tokio::spawn(async move {
            let _ = serve(stream).await;
        });
    }
}

async fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buffer = Vec::with_capacity(4096);
    let mut read_buf = [0u8; 4096];

    loop {
        let header_end = loop {
            if let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos + 4;
            }
            if buffer.len() >= MAX_REQUEST_BYTES {
                return Ok(());
            }
            let read = stream.read(&mut read_buf).await?;
            if read == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&read_buf[..read]);
        };

        buffer.drain(..header_end);
        stream.write_all(RESPONSE).await?;
    }
}
