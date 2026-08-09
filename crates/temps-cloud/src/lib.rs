//! Optional managed-control-plane integration for a self-hosted Temps instance.

#![forbid(unsafe_code)]

mod backup_mirror;
mod handler;
mod plugin;
mod service;

pub use handler::{cloud_routes, CloudApiDoc};
pub use plugin::CloudPlugin;
pub use service::{CloudCapability, CloudService, CloudServiceError, CloudStatus};
