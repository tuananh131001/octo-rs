//! `Services/Soulseek/SoulseekDownloadService.cs`, the parts task 4-A ports: the deny-list seam
//! between the rejected-peer store and the ranking, and the bounded re-poll for a file slskd is
//! still moving. The candidate matching is `octo_core::soulseek::soulseek_download_service`.
//! Task 4-C ports the service itself into this module.

use std::time::{Duration, Instant};

use octo_core::soulseek::SoulseekFileHit;
use tokio_util::sync::CancellationToken;

use super::rejected_peer_registry::RejectedPeerRegistry;

/// Is this candidate still allowed, given what a previous download proved about it?
///
/// Static and separate so the deny-list can be driven in tests without a download
/// service. The failure it guards against is invisible from outside: a filter that denies
/// everything leaves every track unfetchable and looks exactly like Soulseek having no
/// copies.
pub fn candidate_allowed(
    hit: &SoulseekFileHit,
    deny_list: Option<&RejectedPeerRegistry>,
    enabled: bool,
) -> bool {
    match deny_list {
        Some(deny_list) if enabled => !deny_list.is_denied(Some(&hit.username), Some(&hit.filename)),
        _ => true,
    }
}

/// slskd marks a transfer Succeeded before moving the file out of its incomplete
/// directory, and on bind mounts that move is a copy that can take seconds for a FLAC.
/// Without this window the attempt fails on "no file on disk" and the next peer
/// re-downloads the same track. A cancelled caller gets one final check instead of a wait.
pub async fn retry_resolve(
    mut resolve: impl FnMut() -> Option<String>,
    max_wait: Duration,
    poll_interval: Duration,
    ct: &CancellationToken,
) -> Option<String> {
    let deadline = Instant::now() + max_wait;
    loop {
        let path = resolve();
        if path.is_some() || Instant::now() >= deadline {
            return path;
        }
        tokio::select! {
            biased;
            _ = ct.cancelled() => {
                // Caller left: no point waiting out the window, but the file may
                // have just landed, so look once more before giving up.
                return resolve();
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SoulseekDenyListTests -----------------------------------------------------------
    //
    // CandidateAllowed is the seam between the deny-list and the ranking. The failure it guards
    // against is invisible from outside: a filter that denies everything leaves every track
    // unfetchable and looks exactly like Soulseek having no copies.

    fn hit(user: &str, file: &str) -> SoulseekFileHit {
        SoulseekFileHit {
            username: user.into(),
            filename: file.into(),
            extension: "flac".into(),
            size: 40_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn candidate_allowed_rejected_candidate_is_never_offered_again() {
        let registry = RejectedPeerRegistry::in_memory();
        registry.deny(Some("peer1"), Some("a.flac"), "wrong recording", "A - B");

        assert!(!candidate_allowed(&hit("peer1", "a.flac"), Some(&registry), true));
        assert!(candidate_allowed(&hit("peer1", "b.flac"), Some(&registry), true));
    }

    /// Turning the setting off is the fastest recovery from a wrong denial, so it has to work
    /// without touching the file the denials live in.
    #[test]
    fn candidate_allowed_verification_off_the_list_is_inert() {
        let registry = RejectedPeerRegistry::in_memory();
        registry.deny(Some("peer1"), Some("a.flac"), "wrong recording", "A - B");

        assert!(candidate_allowed(&hit("peer1", "a.flac"), Some(&registry), false));
    }

    #[test]
    fn candidate_allowed_no_registry_filters_nothing() {
        assert!(candidate_allowed(&hit("peer1", "a.flac"), None, true));
    }

    // ---- SoulseekResolveRetryTests -------------------------------------------------------
    //
    // slskd marks a transfer Succeeded before moving the file out of its incomplete
    // directory, and on bind mounts that move is a copy that can take seconds. The
    // one-shot disk check used to miss the mid-move file, fail the attempt, and
    // re-download the same track from the next peer. These tests pin the bounded
    // re-poll that closes that window.

    #[tokio::test]
    async fn resolves_immediately_without_waiting() {
        let mut calls = 0;
        let result = retry_resolve(
            || {
                calls += 1;
                Some("/music/song.flac".to_string())
            },
            Duration::from_secs(30),
            Duration::from_millis(10),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(result.as_deref(), Some("/music/song.flac"));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn resolves_when_file_appears_mid_window() {
        let mut calls = 0;
        let result = retry_resolve(
            || {
                calls += 1;
                (calls >= 3).then(|| "/music/song.flac".to_string())
            },
            Duration::from_secs(30),
            Duration::from_millis(10),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(result.as_deref(), Some("/music/song.flac"));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_wait() {
        let result = retry_resolve(
            || None,
            Duration::from_millis(100),
            Duration::from_millis(10),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn cancelled_caller_gets_one_final_check_instead_of_the_window() {
        let cts = CancellationToken::new();
        cts.cancel();

        let mut calls = 0;
        let result = retry_resolve(
            || {
                calls += 1;
                (calls >= 2).then(|| "/music/song.flac".to_string())
            },
            Duration::from_secs(30),
            Duration::from_secs(30),
            &cts,
        )
        .await;

        // First check misses, the delay is cancelled, the final check lands.
        assert_eq!(result.as_deref(), Some("/music/song.flac"));
        assert_eq!(calls, 2);
    }
}
