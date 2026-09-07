use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

/// Streaming-capable body type used by the upstream client.
pub type ClientBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

pub type HttpClient = Client<HttpConnector, ClientBody>;

#[derive(Clone)]
pub struct ClientOptions {
    pub connect_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub http2: bool,
}

/// Build a production-tuned HTTP client with connection pooling.
/// `http2` enables HTTP/2 prior knowledge / ALPN where the connector supports it.
pub fn build_client(opts: ClientOptions) -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(opts.connect_timeout));
    connector.set_keepalive(Some(Duration::from_secs(30)));
    connector.enforce_http(false);
    connector.set_reuse_address(true);

    let mut builder = Client::builder(TokioExecutor::new());
    builder = builder
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(opts.pool_max_idle_per_host.max(4))
        .retry_canceled_requests(true)
        .set_host(true);

    if opts.http2 {
        // Prefer HTTP/2 when upstream supports it (h2c prior knowledge for cleartext).
        builder = builder.http2_only(false);
        builder = builder.http2_adaptive_window(true);
        builder = builder.http2_keep_alive_interval(Duration::from_secs(30));
        builder = builder.http2_keep_alive_timeout(Duration::from_secs(10));
    }

    builder.build(connector)
}

/// Helper to box any body into ClientBody.
pub fn boxed_body<B>(body: B) -> ClientBody
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    body.map_err(Into::into).boxed()
}
