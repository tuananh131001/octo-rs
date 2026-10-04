//! Port of `Models/Download/DownloadInfo.cs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::download_status::DownloadStatus;
use crate::json::datetime;

/// Information about an ongoing or completed download
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DownloadInfo {
    pub song_id: String,
    pub external_id: String,
    pub external_provider: String,
    pub status: DownloadStatus,
    /// 0.0 to 1.0
    pub progress: f64,
    pub local_path: Option<String>,
    pub error_message: Option<String>,
    #[serde(with = "datetime::utc")]
    pub started_at: DateTime<Utc>,
    #[serde(with = "datetime::utc_option")]
    pub completed_at: Option<DateTime<Utc>>,
}

impl Default for DownloadInfo {
    fn default() -> Self {
        Self {
            song_id: String::new(),
            external_id: String::new(),
            external_provider: String::new(),
            status: DownloadStatus::default(),
            progress: 0.0,
            local_path: None,
            error_message: None,
            // DateTime's default is DateTime.MinValue.
            started_at: datetime::min_value(),
            completed_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_status_as_a_number_and_dates_as_stj_did() {
        let info = DownloadInfo {
            song_id: "ext-deezer-1".into(),
            status: DownloadStatus::Completed,
            progress: 1.0,
            started_at: DateTime::parse_from_rfc3339("2026-10-04T12:34:56.5Z")
                .expect("a date")
                .with_timezone(&Utc),
            ..Default::default()
        };
        assert_eq!(
            crate::json::to_string(&info),
            r#"{"SongId":"ext-deezer-1","ExternalId":"","ExternalProvider":"","Status":2,"Progress":1,"LocalPath":null,"ErrorMessage":null,"StartedAt":"2026-10-04T12:34:56.5Z","CompletedAt":null}"#
        );
        let default = crate::json::to_string(&DownloadInfo::default());
        assert!(
            default.contains(r#""StartedAt":"0001-01-01T00:00:00""#),
            "{default}"
        );
    }
}
