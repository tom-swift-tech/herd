pub mod pool;
pub mod health;
pub mod discovery;

pub use pool::{BackendPool, BackendState, GpuMetrics};
pub use health::HealthChecker;
pub use discovery::ModelDiscovery;