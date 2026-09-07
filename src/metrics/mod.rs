use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};

#[derive(Clone)]
pub struct Metrics {
    pub registry: Registry,
    pub requests_total: IntCounterVec,
    pub request_duration: HistogramVec,
    pub upstream_connections: IntGaugeVec,
    pub upstream_health: IntGaugeVec,
    pub rate_limit_rejects: IntCounter,
    pub rate_limit_allows: IntCounter,
    pub active_connections: IntGauge,
    pub retries_total: IntCounter,
}

impl Metrics {
    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new("oxigate_requests_total", "Total number of requests processed"),
            &["method", "status", "upstream"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        let request_duration = HistogramVec::new(
            HistogramOpts::new(
                "oxigate_request_duration_seconds",
                "Request duration in seconds",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["upstream"],
        )?;
        registry.register(Box::new(request_duration.clone()))?;

        let upstream_connections = IntGaugeVec::new(
            Opts::new(
                "oxigate_upstream_connections",
                "Current number of active connections to upstream",
            ),
            &["upstream"],
        )?;
        registry.register(Box::new(upstream_connections.clone()))?;

        let upstream_health = IntGaugeVec::new(
            Opts::new(
                "oxigate_upstream_health",
                "Upstream health status (1=healthy, 0=unhealthy)",
            ),
            &["upstream"],
        )?;
        registry.register(Box::new(upstream_health.clone()))?;

        let rate_limit_rejects =
            IntCounter::new("oxigate_rate_limit_rejects_total", "Requests rejected by rate limiter")?;
        registry.register(Box::new(rate_limit_rejects.clone()))?;

        let rate_limit_allows =
            IntCounter::new("oxigate_rate_limit_allows_total", "Requests allowed by rate limiter")?;
        registry.register(Box::new(rate_limit_allows.clone()))?;

        let active_connections =
            IntGauge::new("oxigate_active_connections", "Currently accepted client connections")?;
        registry.register(Box::new(active_connections.clone()))?;

        let retries_total =
            IntCounter::new("oxigate_retries_total", "Upstream retry attempts")?;
        registry.register(Box::new(retries_total.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            request_duration,
            upstream_connections,
            upstream_health,
            rate_limit_rejects,
            rate_limit_allows,
            active_connections,
            retries_total,
        })
    }

    pub fn gather(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new().expect("failed to create metrics")
    }
}
