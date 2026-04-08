pub mod admin;
pub mod agent;
pub mod models;
pub mod nodes;
pub mod openai;
pub mod status;

// Re-export handlers for convenience
pub use admin::{add_backend, get_backend, list_backends, remove_backend, update_backend};
