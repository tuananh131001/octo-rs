//! `Services/Validation` in the C#: the startup checks printed to the console. The result type,
//! the validator interface and the base helpers are `octo_core::validation`.

pub mod startup_validation_orchestrator;
pub mod subsonic_startup_validator;

pub use octo_core::validation::{ConsoleColor, IStartupValidator, ValidationResult};
pub use startup_validation_orchestrator::StartupValidationOrchestrator;
pub use subsonic_startup_validator::SubsonicStartupValidator;
