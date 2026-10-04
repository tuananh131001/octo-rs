//! `Octo.Models.Settings.UpdateSettings` (the `Updates` section).

use serde::{Deserialize, Serialize};

/// Whether Octo looks for its own new releases, and where. Read through IOptionsMonitor at every
/// check, so a change applies without a restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UpdateSettings {
    /// Ask GitHub every 6 hours whether a newer Octo release is out, and say so on the dashboard
    /// (default: true). Off means Octo never contacts GitHub for this.
    /// Environment variable: UPDATES__CHECK
    pub check: bool,

    /// The GitHub repository whose releases count, as "owner/name" (default: winters27/octo).
    /// Only a fork that publishes its own dated releases needs to change it.
    /// Environment variable: UPDATES__REPO
    pub repo: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check: true,
            repo: "winters27/octo".to_string(),
        }
    }
}
