//! Port of `Services/Validation/StartupValidationOrchestrator.cs`.

use std::sync::Arc;

use super::IStartupValidator;

/// Orchestrates startup validation for all configured services. A hosted service in C#: the
/// host awaited its `StartAsync` before the server started listening, so [`crate::host::run`]
/// awaits [`StartupValidationOrchestrator::start`] before it binds.
pub struct StartupValidationOrchestrator {
    validators: Vec<Arc<dyn IStartupValidator>>,
}

impl StartupValidationOrchestrator {
    pub fn new(validators: Vec<Arc<dyn IStartupValidator>>) -> Self {
        StartupValidationOrchestrator { validators }
    }

    pub async fn start(&self) {
        println!();
        println!("========================================");
        println!("       Octo starting up...       ");
        println!("========================================");
        println!();

        // Run all validators
        for validator in &self.validators {
            if let Err(error) = validator.validate().await {
                println!("Error validating {}: {error}", validator.service_name());
            }
        }

        println!();
        println!("========================================");
        println!("       Startup validation complete      ");
        println!("========================================");
        println!();
    }
}
