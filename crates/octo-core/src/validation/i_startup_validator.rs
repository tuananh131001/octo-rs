//! Port of `Services/Validation/IStartupValidator.cs`.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::validation_result::ValidationResult;

/// Interface for service startup validators
#[async_trait]
pub trait IStartupValidator: Send + Sync {
    /// Gets the name of the service being validated
    fn service_name(&self) -> &str;

    /// Validates the service configuration and connectivity
    async fn validate(&self, cancellation_token: &CancellationToken) -> ValidationResult;
}
