use oxigate::config::Config;
use oxigate::dashboard::{handle_admin, DashboardState};
use oxigate::health::run_health_checks;
use oxigate::metrics::Metrics;
use oxigate::proxy::client::{build_client, ClientOptions};
use oxigate::proxy::handler::ProxyContext;
use oxigate::proxy::proxy_request;
use oxigate::ratelimit::{RateLimitConfig, RateLimiter};
use oxigate::router::Router;
use oxigate::state::AppState;
use oxigate::tls;
use oxigate::proxy_protocol;
use oxigate::security;

use bytes::Bytes;
use clap::Parser;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn, Level};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "oxigate", about = "Ultra-fast L7 reverse proxy & load balancer")]
struct Args {
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured JSON logging – compatible with OpenTelemetry log pipelines
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(Level::INFO.into())
                .add_directive("hyper=warn".parse().unwrap())
                .add_directive("hyper_util=warn".parse().unwrap())
                .add_directive("rustls=warn".parse().unwrap()),
        )
        .json()
        .with_target(true)
        .with_current_span(true)
        .init();

    let args = Args::parse();
    let config = Config::from_file(&args.config)?;
    info!(
        listen = %config.listen,
        metrics = %config.metrics_listen,
        routes = config.routes.len(),
        retries = config.retries,
        max_connections = config.max_connections,
        rate_limit = config.rate_limit.is_some(),
        proxy_protocol = config.proxy_protocol,
        upstream_http2 = config.upstream_http2,
        tls = config.tls.is_some(),
        "loaded configuration"
    );

    let state = AppState::new(config.clone());
    let metrics = Arc::new(Metrics::new()?);
    let router = Arc::new(std::sync::RwLock::new(Arc::new(Router::from_config(&config))));

    let client = build_client(ClientOptions {
        connect_timeout: config.connect_timeout(),
        pool_max_idle_per_host: config.pool_max_idle_per_host,
        http2: config.upstream_http2,
    });

    let rate_limiter = config.rate_limit.as_ref().map(|rl| {
        RateLimiter::new(RateLimitConfig {
            rate: rl.rps,
            window: Duration::from_secs(1),
            burst: rl.burst,
        })
    });

    // Periodic rate-limit bucket cleanup
    if let Some(ref rl) = rate_limiter {
        let rl = rl.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                rl.cleanup(Duration::from_secs(120));
            }
        });
    }

    let conn_limit = if config.max_connections > 0 {
        Some(Arc::new(Semaphore::new(config.max_connections)))
    } else {
        None
    };

    let tls_holder: Arc<std::sync::RwLock<Option<TlsAcceptor>>> = Arc::new(std::sync::RwLock::new(
        if let Some(ref tls_cfg) = config.tls {
            let server_config = tls::load_server_config(&tls_cfg.cert, &tls_cfg.key)?;
            info!("TLS termination enabled (ALPN: h2, http/1.1)");
            Some(TlsAcceptor::from(server_config))
        } else {
            None
        }
    ));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Health checks
    {
        let router = router.clone();
        let client = client.clone();
        let metrics = metrics.clone();
        let interval = config.health_interval();
        let path = config.health_check.path.clone();
        tokio::spawn(async move {
            let lbs = router.read().unwrap().all_lbs();
            run_health_checks(lbs, client, metrics, interval, path).await;
        });
    }

    // Admin / dashboard / metrics server
    {
        let dash = Arc::new(DashboardState {
            metrics: metrics.clone(),
            rate_limiter: rate_limiter.clone(),
            started: Instant::now(),
            version: env!("CARGO_PKG_VERSION"),
        });
        let addr = config.metrics_listen;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_admin_server(addr, dash, &mut shutdown_rx).await {
                error!("admin server error: {e}");
            }
        });
    }

    // SIGHUP reload
    {
        let state = state.clone();
        let router = router.clone();
        let tls_holder = tls_holder.clone();
        let config_path = args.config.clone();
        tokio::spawn(async move {
            let mut sighup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    error!("failed to register SIGHUP: {e}");
                    return;
                }
            };
            loop {
                sighup.recv().await;
                info!("received SIGHUP – reloading configuration");
                match Config::from_file(&config_path) {
                    Ok(new_cfg) => {
                        if let Some(ref tls_cfg) = new_cfg.tls {
                            match tls::load_server_config(&tls_cfg.cert, &tls_cfg.key) {
                                Ok(sc) => {
                                    *tls_holder.write().unwrap() = Some(TlsAcceptor::from(sc));
                                    info!("TLS certificates reloaded");
                                }
                                Err(e) => error!("TLS reload failed: {e}"),
                            }
                        }
                        state.swap_config(new_cfg.clone());
                        *router.write().unwrap() = Arc::new(Router::from_config(&new_cfg));
                        info!("configuration reloaded successfully");
                    }
                    Err(e) => error!("failed to reload configuration: {e}"),
                }
            }
        });
    }

    // Graceful shutdown
    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM");
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT");
            tokio::select! {
                _ = sigterm.recv() => info!("received SIGTERM"),
                _ = sigint.recv() => info!("received SIGINT"),
            }
            let _ = shutdown_tx.send(true);
        });
    }

    let listener = TcpListener::bind(config.listen).await?;
    info!(
        addr = %config.listen,
        admin = %config.metrics_listen,
        "OxiGate listening · dashboard http://{}/dashboard",
        config.metrics_listen
    );

    let proxy_protocol = config.proxy_protocol;
    let mut shutdown_rx = shutdown_rx.clone();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("shutting down – draining connections");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    info!("shutdown complete");
                    break;
                }
            }
            accept = listener.accept() => {
                let (mut stream, remote_addr) = match accept {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("accept error: {e}");
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);

                let permit = if let Some(ref sem) = conn_limit {
                    match sem.clone().try_acquire_owned() {
                        Ok(p) => Some(p),
                        Err(_) => {
                            metrics.active_connections.set(0); // best-effort
                            let io = TokioIo::new(stream);
                            tokio::spawn(async move {
                                let service = service_fn(|_req: Request<Incoming>| async {
                                    Ok::<_, std::convert::Infallible>(
                                        Response::builder()
                                            .status(StatusCode::SERVICE_UNAVAILABLE)
                                            .header("Connection", "close")
                                            .body(Full::new(Bytes::from("Service Unavailable")))
                                            .unwrap(),
                                    )
                                });
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                            continue;
                        }
                    }
                } else {
                    None
                };

                let state = state.clone();
                let router = router.clone();
                let client = client.clone();
                let metrics = metrics.clone();
                let tls_holder = tls_holder.clone();
                let rate_limiter = rate_limiter.clone();

                metrics.active_connections.inc();

                tokio::spawn(async move {
                    let _permit = permit;

                    // PROXY protocol with buffered peek (safe if header absent)
                    let (client_addr, stream) = if proxy_protocol {
                        match oxigate::proxy_protocol::maybe_read_proxy_header(stream).await {
                            Ok((Some(h), s)) => {
                                tracing::debug!(src = %h.src, "PROXY protocol");
                                (h.src, s)
                            }
                            Ok((None, s)) => (remote_addr, s),
                            Err(e) => {
                                warn!(%remote_addr, error = %e, "PROXY protocol parse failed");
                                metrics.active_connections.dec();
                                return;
                            }
                        }
                    } else {
                        (remote_addr, oxigate::proxy_protocol::PrefixedStream::new(stream, Vec::new()))
                    };

                    let cfg = state.load_config();
                    let acl = cfg.acl.as_ref().and_then(|a| {
                        oxigate::security::Acl::from_config(&a.allow, &a.deny).ok()
                    });
                    let ctx = Arc::new(ProxyContext {
                        router: router.read().unwrap().clone(),
                        client: client.clone(),
                        metrics: metrics.clone(),
                        retries: cfg.retries,
                        request_timeout: cfg.request_timeout(),
                        sticky: cfg.sticky.clone(),
                        headers: cfg.headers.clone(),
                        access_log: cfg.access_log,
                        rate_limiter,
                        max_body_bytes: cfg.max_body_bytes,
                        retry_idempotent_only: cfg.retry_idempotent_only,
                        force_retry_with_body: cfg.force_retry_with_body,
                        acl,
                        security_headers: cfg.security_headers,
                    });

                    // TLS acceptor may be reloaded via ArcSwap-like holder
                    let acceptor = tls_holder.read().unwrap().clone();
                    if let Some(acceptor) = acceptor {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = TokioIo::new(tls_stream);
                                serve_connection(io, state, ctx, client_addr).await;
                            }
                            Err(e) => tracing::debug!(%client_addr, "TLS handshake failed: {e}"),
                        }
                    } else {
                        let io = TokioIo::new(stream);
                        serve_connection(io, state, ctx, client_addr).await;
                    }

                    metrics.active_connections.dec();
                });
            }
        }
    }

    Ok(())
}

async fn serve_connection<I>(
    io: TokioIo<I>,
    state: AppState,
    ctx: Arc<ProxyContext>,
    remote_addr: SocketAddr,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        let ctx = ctx.clone();
        async move {
            // OpenTelemetry-friendly span fields via tracing
            let span = tracing::info_span!(
                "proxy_request",
                otel.kind = "server",
                http.method = %req.method(),
                http.target = %req.uri(),
                client.address = %remote_addr,
            );
            let _guard = span.enter();
            proxy_request(req, state, ctx, remote_addr).await
        }
    });

    let builder = AutoBuilder::new(TokioExecutor::new());
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!(%remote_addr, "connection closed: {e}");
    }
}

async fn run_admin_server(
    addr: SocketAddr,
    state: Arc<DashboardState>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "admin/dashboard/metrics listening");

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let io = TokioIo::new(stream);
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let state = state.clone();
                        async move { handle_admin(req, state).await }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        }
    }
    Ok(())
}
