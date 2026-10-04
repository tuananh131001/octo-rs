//! STUB(2-B): replaced when 2-B lands with its port of `Services/Validation/IStartupValidator.cs`.

use async_trait::async_trait;

use super::ValidationResult;

/// Interface for service startup validators
#[async_trait]
pub trait IStartupValidator: Send + Sync {
    /// Gets the name of the service being validated
    fn service_name(&self) -> &str;

    /// Validates the service configuration and connectivity. `Err` stands for an exception
    /// the validator let escape, which the orchestrator reports and moves past.
    async fn validate(&self) -> anyhow::Result<ValidationResult>;
}
