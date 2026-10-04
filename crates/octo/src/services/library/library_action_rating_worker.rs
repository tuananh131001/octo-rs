//! Port of `Services/Library/LibraryActionRatingWorker.cs`: `RatingActionRequest` and the worker
//! (singleton AND hosted: the controller enqueues into the loop the host is running).

use std::sync::Arc;

use octo_core::settings::{LibraryAction, SettingsStore};
use octo_subsonic::SubsonicCredential;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use super::library_action_executor::{LibraryActionExecutor, LibraryActionRequest};
use super::library_action_journal::action_name;
use crate::services::subsonic::SubsonicProxyService;

/// A rating that asked for an action, with the credentials needed to clear it again.
///
/// The triplet is carried rather than looked up because Subsonic ratings are PER USER: clearing
/// with Octo's admin identity would clear the admin's rating and leave the user's in place.
/// Subsonic token auth is md5(password + salt) and is replayable with the same salt, which is
/// the same mechanism NavidromeIdentityService already relies on for startScan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RatingActionRequest {
    pub action: LibraryAction,
    pub navidrome_id: String,
    pub username: String,
    pub auth_user: String,
    pub auth_token: String,
    pub auth_salt: String,
}

/// Applies star-rating actions, off the request thread.
///
/// Separate from the playlist worker because the two triggers are independently configurable:
/// ratings can be on with playlists off, and a single worker with two early-return gates would
/// switch both off together.
pub struct LibraryActionRatingWorker {
    sender: mpsc::Sender<RatingActionRequest>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<RatingActionRequest>>,
    executor: Arc<LibraryActionExecutor>,
    /// A background `SubsonicProxyService`, as the scope the C# made for each clear gave.
    proxy: SubsonicProxyService,
    /// `IOptionsMonitor<LibraryActionSettings>`: read at every enqueue.
    settings: Arc<SettingsStore>,
}

impl LibraryActionRatingWorker {
    const CAPACITY: usize = 256;

    pub fn new(
        executor: Arc<LibraryActionExecutor>,
        proxy: SubsonicProxyService,
        settings: Arc<SettingsStore>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(Self::CAPACITY);
        LibraryActionRatingWorker {
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            executor,
            proxy,
            settings,
        }
    }

    /// Queue a rating for action. The cheap gates run inline so a disabled feature costs one
    /// property read on the request path, and nothing that removes a file runs in the request.
    ///
    /// The C# channel was bounded with `DropWrite`: a full queue drops the new rating and still
    /// answers true.
    pub fn try_enqueue(&self, request: RatingActionRequest) -> bool {
        let settings = self.settings.current();
        let settings = &settings.library_actions;
        if !settings.enabled || !settings.ratings_enabled {
            return false;
        }
        if !settings.is_allowed(Some(&request.username)) {
            return false;
        }
        match self.sender.try_send(request) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Ratings waiting to be applied.
    pub fn pending(&self) -> usize {
        self.sender.max_capacity() - self.sender.capacity()
    }

    /// `ExecuteAsync`: applies queued ratings one at a time until shutdown.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut receiver = self.receiver.lock().await;
        loop {
            let request = tokio::select! {
                request = receiver.recv() => match request {
                    Some(request) => request,
                    None => break,
                },
                _ = stopping.cancelled() => break,
            };
            // Per-item handling is mandatory: one failed action must not stop the worker.
            match self.executor.apply(Self::to_action_request(&request)).await {
                Ok(outcome) => {
                    info!(
                        "Library action {} from a rating by {}: {} - {}",
                        action_name(request.action),
                        request.username,
                        outcome.state.name(),
                        outcome.detail.as_deref().unwrap_or("")
                    );

                    // Only clear a rating the action actually consumed. Leaving it set on a
                    // failure means the user can see it did not take, rather than the rating
                    // vanishing and nothing having happened. Keep never clears it: five stars is
                    // also what someone who loves a track gives it, and Keep changed nothing that
                    // needs undoing.
                    if outcome.consumed() && request.action != LibraryAction::Keep {
                        self.clear_rating(&request).await;
                    }
                }
                Err(e) => error!(
                    "Library action from a rating failed for {}: {e:#}",
                    request.navidrome_id
                ),
            }
        }
        Ok(())
    }

    /// The executor's request, with the rater's own sign-in, so a replaced song keeps the
    /// favorite they gave it.
    pub fn to_action_request(request: &RatingActionRequest) -> LibraryActionRequest {
        let sign_in: [(String, String); 3] = [
            ("u".into(), request.auth_user.clone()),
            ("t".into(), request.auth_token.clone()),
            ("s".into(), request.auth_salt.clone()),
        ];
        LibraryActionRequest::new(
            request.action,
            request.navidrome_id.clone(),
            request.username.clone(),
        )
        .with_credential(SubsonicCredential::from(sign_in.iter().map(|(k, v)| (k, v))))
    }

    /// Put the rating back to 0.
    ///
    /// Three things stop this looping. rating=0 is inert by construction, because the handler
    /// only acts on 1 to 5. This call goes out through the relay, which builds an outbound
    /// request and never re-enters Octo's own routing table. And the journal would make a
    /// re-entry for the same action, id and file content a no-op anyway.
    async fn clear_rating(&self, request: &RatingActionRequest) {
        let cleared = self
            .proxy
            .relay(
                "rest/setRating",
                [
                    ("id", request.navidrome_id.as_str()),
                    ("rating", "0"),
                    ("u", request.auth_user.as_str()),
                    ("t", request.auth_token.as_str()),
                    ("s", request.auth_salt.as_str()),
                    ("v", "1.16.1"),
                    ("c", "octo"),
                    ("f", "json"),
                ],
            )
            .await;
        if let Err(e) = cleared {
            // A rating left set is cosmetic, and the journal stops it triggering the action a
            // second time.
            info!("Could not clear the rating on {}: {e}", request.navidrome_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use octo_core::settings::{AppSettings, LibraryActionSettings, SubsonicSettings};
    use parking_lot::Mutex;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::any};

    use super::*;
    use crate::services::common::test_fakes::until;
    use crate::services::library::library_action_test_support::{Extras, asked_about, executor, store};

    fn rating(action: LibraryAction, id: &str) -> RatingActionRequest {
        RatingActionRequest {
            action,
            navidrome_id: id.into(),
            username: "alice".into(),
            auth_user: "alice".into(),
            auth_token: "t".into(),
            auth_salt: "s".into(),
        }
    }

    /// Records the ids whose rating was put back to 0.
    #[derive(Clone, Default)]
    struct RatingNavidrome {
        cleared: Arc<Mutex<Vec<String>>>,
    }

    impl Respond for RatingNavidrome {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let query: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
            let value = |key: &str| query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            if request.url.path().ends_with("/rest/setRating") && value("rating").as_deref() == Some("0") {
                self.cleared.lock().push(value("id").unwrap_or_default());
            }
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json")
        }
    }

    /// Five stars is also what someone who loves a track gives it, and Keep changed nothing
    /// that needs undoing, so its rating stays. Any other consumed rating is cleared.
    #[tokio::test]
    async fn rating_worker_never_clears_a_keep_but_clears_another_consumed_rating() {
        let navidrome = MockServer::start().await;
        let responder = RatingNavidrome::default();
        Mock::given(any())
            .respond_with(responder.clone())
            .mount(&navidrome)
            .await;
        let settings = store(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                ratings_enabled: true,
                dry_run: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        });
        let worker = Arc::new(LibraryActionRatingWorker::new(
            executor(
                &settings,
                Extras {
                    notices: Some(asked_about("alice", "nd-keep")),
                    ..Default::default()
                },
            ),
            SubsonicProxyService::new(settings.clone()),
            settings,
        ));
        let stopping = CancellationToken::new();
        let running = tokio::spawn(worker.clone().run(stopping.clone()));

        assert!(worker.try_enqueue(rating(LibraryAction::Keep, "nd-keep")));
        // Delete is not switched on, so this one is Skipped: consumed, and its rating cleared.
        assert!(worker.try_enqueue(rating(LibraryAction::Delete, "nd-delete")));
        until(|| !responder.cleared.lock().is_empty()).await;
        stopping.cancel();
        let _ = running.await;

        assert_eq!(*responder.cleared.lock(), ["nd-delete"]);
    }

    /// Port of `LibraryActionStarTests.RatingCarriesTheRatersSignIn`.
    #[test]
    fn rating_carries_the_raters_sign_in() {
        let request = LibraryActionRatingWorker::to_action_request(&RatingActionRequest {
            action: LibraryAction::BetterQuality,
            navidrome_id: "nd-1".into(),
            username: "alice".into(),
            auth_user: "alice".into(),
            auth_token: "token".into(),
            auth_salt: "salt".into(),
        });

        assert_eq!(request.action, LibraryAction::BetterQuality);
        assert_eq!(request.navidrome_id, "nd-1");
        assert_eq!(request.username, "alice");
        let credential = request.credential.expect("a sign-in");
        assert_eq!(credential.user(), Some("alice"));
        let parameters = credential.parameters(&[]);
        assert_eq!(parameters["t"], "token");
        assert_eq!(parameters["s"], "salt");
    }

    /// Rust-only: the cheap gates, and the full queue that drops the rating and still says yes.
    #[tokio::test]
    async fn the_gates_run_inline_and_a_full_queue_drops_the_new_rating() {
        let settings = store(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                ratings_enabled: false,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        let worker = LibraryActionRatingWorker::new(
            executor(&settings, Extras::default()),
            SubsonicProxyService::new(settings.clone()),
            settings.clone(),
        );
        assert!(!worker.try_enqueue(rating(LibraryAction::Delete, "nd-1")));

        let mut on = (*settings.current()).clone();
        on.library_actions.ratings_enabled = true;
        settings.set(on);
        assert!(!worker.try_enqueue(RatingActionRequest {
            username: "mallory".into(),
            ..rating(LibraryAction::Delete, "nd-1")
        }));
        for _ in 0..300 {
            assert!(worker.try_enqueue(rating(LibraryAction::Delete, "nd-1")));
        }
        assert_eq!(worker.pending(), 256);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
