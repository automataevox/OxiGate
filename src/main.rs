#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use oxigate::config::Config;
use oxigate::dashboard::{handle_admin, handle_dashboard_ws, DashboardState};
use oxigate::health::{run_health_checks, HealthRuntime};
use oxigate::metrics::Metrics;
use oxigate::pool::BufferPool;
use oxigate::proxy::client::{build_client, ClientOptions};
use oxigate::proxy::handler::ProxyContext;
use oxigate::proxy_protocol;
use oxigate::ratelimit::{RateLimitConfig, RateLimiter};
use oxigate::router::Router;
use oxigate::security::Acl;
use oxigate::state::AppState;
use oxigate::tls;
use oxigate::BoxError;

use anyhow::Result;
use clap::Parser;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio_rustls::TlsAcceptor;
use tower::service_fn;
use tracing::{error, info, warn, Level};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "oxigate",
    about = "Ultra-fast L7 reverse proxy & load balancer"
)]
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
                Some(Acl::from_config(&config.proxy_protocol_trusted_cidrs, &[])?)
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

/// Owned packet handed from accept loop → dispatcher.
struct AcceptedConn {
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
}

/// Concrete owned service – avoids higher-rank lifetime issues with closures.
#[derive(Clone)]
struct ProxySvc {
    ctx: Arc<ProxyContext>,
    state: Arc<AppState>,
    client_addr: SocketAddr,
}

impl tower::Service<Request<Incoming>> for ProxySvc {
    type Response = hyper::Response<oxigate::DynResponseBody>;
    type Error = BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        let ctx = self.ctx.clone();
        let state = self.state.clone();
        let client_addr = self.client_addr;
        Box::pin(async move {
            oxigate::proxy::proxy_request(req, state, ctx, client_addr)
                .await
                .map_err(|e| e as BoxError)
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
    let state = Arc::new(AppState::new(initial.clone()));
    let runtime = Arc::new(RwLock::new(Runtime::from_config(initial)?));

    let shared_inflight = Arc::new(AtomicUsize::new(0));

    let admin_token = Arc::new(RwLock::new(
        runtime.read().unwrap().config.admin_token.clone(),
    ));
    let shared_rl: Arc<RwLock<Option<Arc<RateLimiter>>>> =
        Arc::new(RwLock::new(runtime.read().unwrap().rate_limiter.clone()));

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

    // SIGHUP reload
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
                            info!("runtime reloaded");
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

    // ---- Bounded task channel + pre-allocated buffer pool ----
    let max_conn = {
        let rt = runtime.read().unwrap();
        if rt.config.max_connections > 0 {
            rt.config.max_connections
        } else {
            rt.config.max_concurrency.unwrap_or(10_000)
        }
    };
    let channel_cap = max_conn.max(256);
    let conn_limit = Arc::new(Semaphore::new(max_conn.max(64)));

    let buffer_pool = BufferPool::new(
        (max_conn * 2).clamp(256, 16_384),
        16 * 1024,
    );
    info!(
        channel_cap,
        max_conn,
        pool_size = buffer_pool.capacity(),
        "bounded task channel + buffer pool ready"
    );

    let (conn_tx, mut conn_rx) = mpsc::channel::<AcceptedConn>(channel_cap);

    // Dispatcher: pulls from bounded channel, spawns connection task only when
    // a Semaphore permit is free. Caps concurrent connection tasks at max_conn.
    {
        let runtime = runtime.clone();
        let state = state.clone();
        let metrics = metrics.clone();
        let shared_inflight = shared_inflight.clone();
        let buffer_pool = buffer_pool.clone();
        let conn_limit = conn_limit.clone();
        let mut shutdown_rx = shutdown_rx.clone();

        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                        continue;
                    }
                    item = conn_rx.recv() => item,
                };
                let Some(AcceptedConn { stream, remote_addr }) = accepted else {
                    break;
                };

                let permit = match conn_limit.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        metrics
                            .requests_total
                            .with_label_values(&["ACCEPT", "503", "none"])
                            .inc();
                        tracing::debug!(%remote_addr, "connection limit – shedding");
                        continue;
                    }
                };

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
                let metrics = metrics.clone();
                let shared_inflight = shared_inflight.clone();
                let buffer_pool = buffer_pool.clone();
                metrics.active_connections.inc();

                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = handle_one_connection(
                        stream,
                        remote_addr,
                        router,
                        client,
                        rate_limiter,
                        tls_acc,
                        request_limit,
                        proxy_trusted,
                        cfg,
                        state,
                        metrics.clone(),
                        shared_inflight,
                        buffer_pool,
                    )
                    .await;
                    metrics.active_connections.dec();
                });
            }
            tracing::debug!("connection dispatcher exited");
        });
    }

    let mut shutdown_rx = shutdown_rx.clone();
    let metrics_accept = metrics.clone();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("shutdown: closing accept channel");
                    drop(conn_tx);
                    tokio::time::sleep(Duration::from_secs(5)).await;
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

                match conn_tx.try_send(AcceptedConn { stream, remote_addr }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        metrics_accept
                            .requests_total
                            .with_label_values(&["ACCEPT", "503", "none"])
                            .inc();
                        tracing::debug!(%remote_addr, "accept queue full – shedding");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
    }

    Ok(())
}

async fn handle_one_connection(
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    router: Arc<Router>,
    client: oxigate::proxy::client::HttpClient,
    rate_limiter: Option<Arc<RateLimiter>>,
    tls_acc: Option<TlsAcceptor>,
    request_limit: Option<Arc<Semaphore>>,
    proxy_trusted: Option<Acl>,
    cfg: Config,
    state: Arc<AppState>,
    metrics: Arc<Metrics>,
    _shared_inflight: Arc<AtomicUsize>,
    _buffer_pool: BufferPool,
) -> Result<(), BoxError> {
    let (client_addr, stream) = if cfg.proxy_protocol {
        let allowed = match &proxy_trusted {
            Some(acl) => acl.is_allowed(remote_addr.ip()),
            None => true,
        };
        if !allowed {
            warn!(%remote_addr, "PROXY from untrusted source rejected");
            return Ok(());
        }
        match proxy_protocol::maybe_read_proxy_header(stream).await {
            Ok((Some(h), s)) => (h.src, s),
            Ok((None, s)) => (remote_addr, s),
            Err(e) => {
                warn!(%remote_addr, error=%e, "PROXY parse failed");
                return Ok(());
            }
        }
    } else {
        (remote_addr, proxy_protocol::PrefixedStream::new(stream, Vec::new()))
    };

    let acl = cfg
        .acl
        .as_ref()
        .and_then(|a| Acl::from_config(&a.allow, &a.deny).ok());

    let idle = Duration::from_secs(cfg.timeouts.idle_secs.max(1));
    let req_timeout = cfg.request_timeout();

    let ctx = Arc::new(ProxyContext {
        router,
        client,
        metrics: metrics.clone(),
        retries: cfg.retries,
        request_timeout: req_timeout,
        sticky: cfg.sticky.clone(),
        headers: cfg.headers.clone(),
        access_log: cfg.access_log,
        rate_limiter,
        max_body_bytes: cfg.max_body_bytes,
        retry_idempotent_only: cfg.retry_idempotent_only,
        force_retry_with_body: cfg.force_retry_with_body,
        acl,
        security_headers: cfg.security_headers,
        client_is_tls: tls_acc.is_some(),
        request_limit,
        idle_timeout: idle,
    });

    let max_conn_life = idle.saturating_mul(10).max(req_timeout.saturating_mul(2));
    let result = tokio::time::timeout(max_conn_life, async {
        if let Some(acceptor) = tls_acc {
            match acceptor.accept(stream).await {
                Ok(tls_stream) => {
                    let io = TokioIo::new(tls_stream);
                    let mut ctx = (*ctx).clone();
                    ctx.client_is_tls = true;
                    let svc = ProxySvc {
                        ctx: Arc::new(ctx),
                        state: state.clone(),
                        client_addr,
                    };
                    let service = TowerToHyperService::new(svc);
                    let builder = auto::Builder::new(TokioExecutor::new());
                    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
                        tracing::debug!(%client_addr, "TLS conn closed: {e}");
                    }
                }
                Err(e) => tracing::debug!(%client_addr, "TLS failed: {e}"),
            }
        } else {
            let io = TokioIo::new(stream);
            let svc = ProxySvc {
                ctx,
                state,
                client_addr,
            };
            let service = TowerToHyperService::new(svc);
            let builder = auto::Builder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
                tracing::debug!(%client_addr, "HTTP conn closed: {e}");
            }
        }
    })
    .await;

    if result.is_err() {
        warn!(%client_addr, "connection idle/lifetime timeout");
    }
    Ok(())
}

async fn run_admin_server(
    addr: SocketAddr,
    state: Arc<DashboardState>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "admin/metrics listening");

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let io = TokioIo::new(stream);
                let state = state.clone();

                tokio::spawn(async move {
                    use std::convert::Infallible;

                    let service = service_fn(move |req: Request<Incoming>| {
                        let state = state.clone();
                        async move {
                            if req.uri().path() == "/ws"
                                && req.headers().contains_key(hyper::header::UPGRADE)
                            {
                                let res = handle_dashboard_ws(req, state);
                                Ok::<_, Infallible>(res.map(|b| {
                                    b.map_err(|e: std::convert::Infallible| -> BoxError { match e {} }).boxed()
                                }))
                            } else {
                                match handle_admin(req, state).await {
                                    Ok(res) => {
                                        Ok(res.map(|b| {
                                            b.map_err(|e: std::convert::Infallible| -> BoxError { match e {} }).boxed()
                                        }))
                                    }
                                    Err(_e) => unreachable!(),
                                }
                            }
                        }
                    });
                    let hyper_service = TowerToHyperService::new(service);

                    let builder = auto::Builder::new(TokioExecutor::new());
                    if let Err(e) = builder.serve_connection_with_upgrades(io, hyper_service).await {
                        tracing::debug!("admin conn closed: {e}");
                    }
                });
            }
        }
    }

    Ok(())
}
