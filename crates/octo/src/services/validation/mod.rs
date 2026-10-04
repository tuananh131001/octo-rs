//! `Services/Validation` in the C#: the startup checks printed to the console.

pub mod base_startup_validator;
pub mod i_startup_validator;
pub mod startup_validation_orchestrator;
pub mod subsonic_startup_validator;
pub mod validation_result;

pub use i_startup_validator::IStartupValidator;
pub use startup_validation_orchestrator::StartupValidationOrchestrator;
pub use subsonic_startup_validator::SubsonicStartupValidator;
pub use validation_result::{ConsoleColor, ValidationResult};
