//! OxiGate library surface (for tests and embedding).
pub mod middleware;
pub mod config;
pub mod dashboard;
pub mod health;
pub mod lb;
pub mod metrics;
pub mod pool;
pub mod proxy;
pub mod proxy_protocol;
pub mod ratelimit;
pub mod router;
pub mod security;
pub mod state;
pub mod tls;

pub use proxy::ProxyContext;
pub use state::AppState;
pub use middleware::load_shed::DynResponseBody;
pub use pool::BufferPool;

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
