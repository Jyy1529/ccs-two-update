//! Explicit, bounded capability diagnostics. Never changes provider routing or live files.
//!
//! Only the public DTOs below may enter IPC/history. Provider snapshots and credentials
//! stay in the private prepared plan and are dropped after a run or plan expiry.

mod discovery;
mod probes;
mod protocol;
mod runtime;
mod target;
mod transport;

mod types;

pub use discovery::fetch_models as fetch_validation_models;
pub use runtime::ModelValidationService;
pub use types::*;

fn input_error(message: &str) -> crate::error::AppError {
    crate::error::AppError::InvalidInput(format!("[model_validation] {message}"))
}

#[cfg(test)]
mod tests;
