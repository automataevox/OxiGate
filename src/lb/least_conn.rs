use super::UpstreamRuntime;
use std::sync::Arc;

/// Select the healthy upstream with the fewest active connections.
pub fn select(healthy: &[Arc<UpstreamRuntime>]) -> Option<Arc<UpstreamRuntime>> {
    healthy.iter().min_by_key(|u| u.connections()).cloned()
}
