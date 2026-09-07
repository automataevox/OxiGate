use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;
use hyper::body::Body;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

pub type ClientBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

type HttpsConnector = hyper_rustls::HttpsConnector<HttpConnector>;
pub type HttpClient = Client<HttpsConnector, ClientBody>;

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub connect_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub http2: bool,
}

/// HTTP/HTTPS client (rustls) with pooling and optional HTTP/2.
pub fn build_client(opts: ClientOptions) -> HttpClient {
    let mut http = HttpConnector::new();
    http.set_nodelay(true);
    http.set_connect_timeout(Some(opts.connect_timeout));
    http.set_keepalive(Some(Duration::from_secs(30)));
    http.enforce_http(false);
    http.set_reuse_address(true);

    // hyper-rustls 0.24: with_native_roots() (feature native-tokio)
    let https = HttpsConnectorBuilder::new()
        .with_native_roots() // native root certs
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);

    // Builder methods take &mut self and return &mut Self — chain without reassignment.
    let mut builder = Client::builder(TokioExecutor::new());
    builder
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(opts.pool_max_idle_per_host.max(4))
        .retry_canceled_requests(true)
        .set_host(true);

    if opts.http2 {
        builder
            .http2_only(false)
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10));
    }

    builder.build(https)
}

pub fn boxed_body<B>(body: B) -> ClientBody
where
    B: Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    BodyExt::map_err(body, Into::into).boxed()
}
