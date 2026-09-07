use oxigate::config::Config;
use oxigate::dashboard::{handle_admin, DashboardState};
use oxigate::health::{run_health_checks, HealthRuntime};
use oxigate::metrics::Metrics;
use oxigate::proxy::client::{build_client, ClientOptions};
use oxigate::proxy::handler::ProxyContext;
use oxigate::proxy::proxy_request;
use oxigate::proxy_protocol;
use oxigate::ratelimit::{RateLimitConfig, RateLimiter};
use oxigate::router::Router;
use oxigate::security::Acl;
use oxigate::state::AppState;
use oxigate::tls;

use bytes::Bytes;
use clap::Parser;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn, Level};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "oxigate", about = "Ultra-fast L7 reverse proxy & load balancer")]
struct Args {
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

struct Runtime {
    config: Config,
    router: Arc<Router>,
    client: oxigate::proxy::client::HttpClient,
    rate_limiter: Option<Arc<RateLimiter>>,
    tls: Option<TlsAcceptor>,
    request_limit: Option<Arc<Semaphore>>,
    proxy_trusted: Option<Acl>,
}

impl Runtime {
    fn from_config(config: Config) -> anyhow::Result<Self> {
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
        let tls = if let Some(ref t) = config.tls {
            Some(TlsAcceptor::from(tls::load_server_config(&t.cert, &t.key)?))
        } else {
            None
        };
        let request_limit = if config.max_connections > 0 {
            Some(Arc::new(Semaphore::new(config.max_connections)))
        } else {
            None
        };
        let proxy_trusted =
            if config.proxy_protocol && !config.proxy_protocol_trusted_cidrs.is_empty() {
                Some(Acl::from_config(
                    &config.proxy_protocol_trusted_cidrs,
                    &[],
                )?)
            } else {
                None
            };
        let router = Arc::new(Router::from_config(&config));
        Ok(Self {
            config,
            router,
            client,
            rate_limiter,
            tls,
            request_limit,
            proxy_trusted,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install the Rustls ring crypto provider"))?;

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
    let initial = Config::from_file(&args.config)?;
    info!(
        listen = %initial.listen,
        admin = %initial.metrics_listen,
        admin_auth = initial.admin_token.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
        "starting OxiGate"
    );

    let metrics = Arc::new(Metrics::new()?);
    let state = AppState::new(initial.clone());
    let runtime = Arc::new(RwLock::new(Runtime::from_config(initial)?));

    let admin_token = Arc::new(RwLock::new(
        runtime.read().unwrap().config.admin_token.clone(),
    ));
    let shared_rl: Arc<RwLock<Option<Arc<RateLimiter>>>> = Arc::new(RwLock::new(
        runtime.read().unwrap().rate_limiter.clone(),
    ));

    // Health runtime (client + config + lbs fully refreshable)
    let health_rt = {
        let runtime = runtime.clone();
        let (client, config) = {
            let rt = runtime.read().unwrap();
            (rt.client.clone(), rt.config.health_check.clone())
        };
        let provider_runtime = runtime.clone();
        Arc::new(RwLock::new(HealthRuntime {
            client,
            config,
            lbs: Box::new(move || provider_runtime.read().unwrap().router.all_lbs()),
        }))
    };
    {
        let health_rt = health_rt.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            run_health_checks(health_rt, metrics).await;
        });
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Admin server
    {
        let dash = Arc::new(DashboardState {
            metrics: metrics.clone(),
            rate_limiter: shared_rl.clone(),
            started: Instant::now(),
            version: env!("CARGO_PKG_VERSION"),
            admin_token: admin_token.clone(),
        });
        let addr = runtime.read().unwrap().config.metrics_listen;
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_admin_server(addr, dash, &mut shutdown_rx).await {
                error!("admin server error: {e}");
            }
        });
    }

    // SIGHUP full rebuild + refresh health/admin shared state
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let health_rt = health_rt.clone();
        let admin_token = admin_token.clone();
        let shared_rl = shared_rl.clone();
        let config_path = args.config.clone();
        tokio::spawn(async move {
            let mut sighup = signal(SignalKind::hangup()).expect("SIGHUP");
            loop {
                sighup.recv().await;
                info!("SIGHUP: rebuilding runtime");
                match Config::from_file(&config_path) {
                    Ok(cfg) => match Runtime::from_config(cfg.clone()) {
                        Ok(new_rt) => {
                            *admin_token.write().unwrap() = new_rt.config.admin_token.clone();
                            *shared_rl.write().unwrap() = new_rt.rate_limiter.clone();
                            {
                                let mut h = health_rt.write().unwrap();
                                h.client = new_rt.client.clone();
                                h.config = new_rt.config.health_check.clone();
                            }
                            state.swap_config(cfg);
                            *runtime.write().unwrap() = new_rt;
                            info!("runtime reloaded (router/client/TLS/rate-limit/health/admin)");
                        }
                        Err(e) => error!("runtime rebuild failed: {e}"),
                    },
                    Err(e) => error!("config reload failed: {e}"),
                }
            }
        });
    }

    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM");
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT");
            tokio::select! {
                _ = sigterm.recv() => info!("SIGTERM"),
                _ = sigint.recv() => info!("SIGINT"),
            }
            let _ = shutdown_tx.send(true);
        });
    }

    let listen_addr = runtime.read().unwrap().config.listen;
    let listener = TcpListener::bind(listen_addr).await?;
    info!(%listen_addr, "listening");

    let mut shutdown_rx = shutdown_rx.clone();
    let mut joins = JoinSet::new();
    let metrics_accept = metrics.clone();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("draining {} connection task(s)", joins.len());
                    let deadline = tokio::time::sleep(Duration::from_secs(15));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => {
                                warn!("drain timeout – aborting remaining tasks");
                                joins.abort_all();
                                break;
                            }
                            r = joins.join_next() => {
                                if r.is_none() { break; }
                            }
                        }
                    }
                    info!("shutdown complete");
                    break;
                }
            }
            accept = listener.accept() => {
                let (stream, remote_addr) = match accept {
                    Ok(v) => v,
                    Err(e) => { warn!("accept: {e}"); continue; }
                };
                let _ = stream.set_nodelay(true);

                let (router, client, rate_limiter, tls_acc, request_limit, proxy_trusted, cfg) = {
                    let rt = runtime.read().unwrap();
                    (
                        rt.router.clone(),
                        rt.client.clone(),
                        rt.rate_limiter.clone(),
                        rt.tls.clone(),
                        rt.request_limit.clone(),
                        rt.proxy_trusted.clone(),
                        rt.config.clone(),
                    )
                };

                let state = state.clone();
                let metrics = metrics_accept.clone();
                metrics.active_connections.inc();
                let idle = Duration::from_secs(cfg.timeouts.idle_secs.max(1));
                let req_timeout = cfg.request_timeout();

                joins.spawn(async move {

                    let (client_addr, stream) = if cfg.proxy_protocol {
                        let allowed = match &proxy_trusted {
                            Some(acl) => acl.is_allowed(remote_addr.ip()),
                            None => true,
                        };
                        if !allowed {
                            warn!(%remote_addr, "PROXY from untrusted source rejected");
                            metrics.active_connections.dec();
                            return;
                        }
                        match proxy_protocol::maybe_read_proxy_header(stream).await {
                            Ok((Some(h), s)) => (h.src, s),
                            Ok((None, s)) => (remote_addr, s),
                            Err(e) => {
                                warn!(%remote_addr, error=%e, "PROXY parse failed");
                                metrics.active_connections.dec();
                                return;
                            }
                        }
                    } else {
                        (remote_addr, proxy_protocol::PrefixedStream::new(stream, Vec::new()))
                    };

                    let acl = cfg
                        .acl
                        .as_ref()
                        .and_then(|a| Acl::from_config(&a.allow, &a.deny).ok());

                    let make_ctx = |is_tls: bool| {
                        Arc::new(ProxyContext {
                            router: router.clone(),
                            client: client.clone(),
                            metrics: metrics.clone(),
                            retries: cfg.retries,
                            request_timeout: req_timeout,
                            sticky: cfg.sticky.clone(),
                            headers: cfg.headers.clone(),
                            access_log: cfg.access_log,
                            rate_limiter: rate_limiter.clone(),
                            max_body_bytes: cfg.max_body_bytes,
                            retry_idempotent_only: cfg.retry_idempotent_only,
                            force_retry_with_body: cfg.force_retry_with_body,
                            acl: acl.clone(),
                            security_headers: cfg.security_headers,
                            client_is_tls: is_tls,
                            request_limit: request_limit.clone(),
                            idle_timeout: idle,
                        })
                    };

                    // Safety net for abandoned TCP connections (primary idle is on body frames).
                    let max_conn_life = idle.saturating_mul(10).max(req_timeout.saturating_mul(2));
                    let result = tokio::time::timeout(max_conn_life, async {
                        if let Some(acceptor) = tls_acc {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    let io = TokioIo::new(tls_stream);
                                    serve_connection(io, state, make_ctx(true), client_addr).await;
                                }
                                Err(e) => tracing::debug!(%client_addr, "TLS failed: {e}"),
                            }
                        } else {
                            let io = TokioIo::new(stream);
                            serve_connection(io, state, make_ctx(false), client_addr).await;
                        }
                    })
                    .await;

                    if result.is_err() {
                        warn!(%client_addr, "connection idle/lifetime timeout");
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
            let span = tracing::info_span!(
                "proxy_request",
                otel.kind = "server",
                http.method = %req.method(),
                http.target = %req.uri(),
                client.address = %remote_addr,
            );
            let _g = span.enter();
            proxy_request(req, state, ctx, remote_addr).await
        }
    });
    let builder = AutoBuilder::new(TokioExecutor::new());
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!(%remote_addr, "conn closed: {e}");
    }
}

async fn run_admin_server(
    addr: SocketAddr,
    state: Arc<DashboardState>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "admin/metrics listening");
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
