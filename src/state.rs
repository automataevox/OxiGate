use crate::config::Config;
use arc_swap::ArcSwap;
use std::sync::Arc;

/// Global shared state that can be atomically swapped on hot-reload.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ArcSwap<Config>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(ArcSwap::from_pointee(config)),
        }
    }

    pub fn load_config(&self) -> arc_swap::Guard<Arc<Config>> {
        self.config.load()
    }

    pub fn swap_config(&self, new_config: Config) {
        self.config.store(Arc::new(new_config));
    }
}
