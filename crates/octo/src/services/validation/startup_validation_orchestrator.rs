//! Port of `Services/Validation/StartupValidationOrchestrator.cs`.

use std::sync::Arc;

use octo_core::validation::IStartupValidator;
use tokio_util::sync::CancellationToken;

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

        // Run all validators. Each one catches its own failures and turns them into a result,
        // so nothing escapes here; the C# catch around each call never fired.
        let token = CancellationToken::new();
        for validator in &self.validators {
            validator.validate(&token).await;
        }

        println!();
        println!("========================================");
        println!("       Startup validation complete      ");
        println!("========================================");
        println!();
    }
}
