//! Startup validation (`Services/Validation`): the result, the validator interface and the
//! helpers the C# base class gave every validator. The Subsonic and Soulseek validators and
//! the orchestrator come with their services.

pub mod base_startup_validator;
pub mod i_startup_validator;
pub mod validation_result;

pub use base_startup_validator::{BaseStartupValidator, ExceptionKind};
pub use i_startup_validator::IStartupValidator;
pub use validation_result::{ConsoleColor, ValidationResult};
