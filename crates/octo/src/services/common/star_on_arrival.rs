//! Port of `Services/Common/StarOnArrival.cs`.
//!
//! The C# subscribed to the tracker's `Ended` event in its constructor and unsubscribed in
//! `Dispose`. The tracker holds the listener and this holds the tracker, so the listener holds
//! this only weakly ([`Weak`]); dropping the last `Arc` unsubscribes, as `Dispose` did.

use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use octo_core::common::Clock;
use octo_core::common::dotnet::{self, eq_ignore_case};
use octo_core::settings::SettingsStore;
use octo_subsonic::subsonic_credential::SubsonicCredential;
use parking_lot::Mutex;
use serde_json::Value;
use tracing::{debug, info};

use super::acquisition_tracker::{self, AcquisitionEnd, AcquisitionTracker, LibraryLookup};
use crate::services::library::NavidromeSongPathResolver;
use crate::services::subsonic::{CredentialCheck, NavidromeIdentityService, SubsonicProxyService};

/// Calls Navidrome as the user: an endpoint ("rest/star") and its parameters, answering the
/// body.
pub type NavidromeCall = Arc<
    dyn Fn(String, IndexMap<String, String>) -> BoxFuture<'static, anyhow::Result<Vec<u8>>> + Send + Sync,
>;

/// The Navidrome side the C# reached through `IServiceScopeFactory`: the relay that calls as the
/// person, and what finds a placed file's id. Absent in tests, which answer through the seams.
#[derive(Clone)]
pub struct StarNavidrome {
    /// A background `SubsonicProxyService` (no request of its own), as a fresh scope gave.
    pub proxy: SubsonicProxyService,
    pub identity: NavidromeIdentityService,
    pub resolver: Arc<NavidromeSongPathResolver>,
}

#[derive(Clone)]
struct Hold {
    key: String,
    credential: SubsonicCredential,
    who: String,
    held_at: DateTime<Utc>,
}

#[derive(Default)]
struct Holds {
    songs: Vec<Hold>,
    albums: Vec<Hold>,
}

/// The C# internal settable properties: `Call`, `LibraryLookup`, `VisibilityPoll`,
/// `VisibilityAttempts`.
#[derive(Clone)]
pub struct StarSeams {
    /// Calls Navidrome as the user. Tests set it; otherwise the relay.
    pub call: Option<NavidromeCall>,
    /// Finds a placed file's Navidrome id. Tests set it; otherwise the path resolver.
    pub library_lookup: Option<LibraryLookup>,
    pub visibility_poll: Duration,
    pub visibility_attempts: i32,
}

impl Default for StarSeams {
    fn default() -> Self {
        StarSeams {
            call: None,
            library_lookup: None,
            visibility_poll: Duration::from_secs(15),
            visibility_attempts: 40,
        }
    }
}

/// Favorites a starred outside song in Navidrome once its download lands, for the person who
/// starred it (#71). In most clients a star means "I like this", and Octo kept the song but lost
/// the like, because the id that was starred was never Navidrome's.
///
/// Navidrome favorites only as the user who signs the call, so that person's sign-in is held
/// with the download: in memory only, and dropped once the favorite is sent, once the download
/// ends without a song, or after a day. A restart loses what is held. A password is never held:
/// it is turned into a token first.
pub struct StarOnArrival {
    holds: Mutex<Holds>,
    tracker: Arc<AcquisitionTracker>,
    navidrome: Option<StarNavidrome>,
    /// `IOptionsMonitor<SubsonicSettings>`: `StarDownloadsForRequester` is read when a song lands.
    settings: Arc<SettingsStore>,
    clock: Clock,
    seams: Mutex<StarSeams>,
    subscription: u64,
}

impl StarOnArrival {
    /// What Octo's own apps send as c. Their star is the Add button: a copy, not a like.
    pub const OCTO_APP_CLIENT: &'static str = "Octo";

    pub fn new(
        tracker: Arc<AcquisitionTracker>,
        navidrome: Option<StarNavidrome>,
        settings: Arc<SettingsStore>,
        clock: Clock,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<StarOnArrival>| {
            let listener = weak.clone();
            let subscription = tracker.subscribe_ended(Arc::new(move |end| {
                if let Some(stars) = listener.upgrade() {
                    stars.on_ended(end);
                }
            }));
            StarOnArrival {
                holds: Mutex::new(Holds::default()),
                tracker,
                navidrome,
                settings,
                clock,
                seams: Mutex::new(StarSeams::default()),
                subscription,
            }
        })
    }

    /// Sets the test seams.
    pub fn configure(&self, change: impl FnOnce(&mut StarSeams)) {
        change(&mut self.seams.lock());
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    pub fn is_octo_app(client: Option<&str>) -> bool {
        client.is_some_and(|c| eq_ignore_case(c.trim(), Self::OCTO_APP_CLIENT))
    }

    /// How many sign-ins are held.
    pub fn held(&self) -> usize {
        let holds = self.holds.lock();
        holds.songs.len() + holds.albums.len()
    }

    pub fn hold_song(
        &self,
        provider: &str,
        external_id: &str,
        credential: &SubsonicCredential,
        who: Option<&str>,
    ) {
        self.add(
            false,
            acquisition_tracker::key_of(provider, external_id),
            credential,
            who,
        );
    }

    pub fn hold_album(
        &self,
        provider: &str,
        album_id: &str,
        credential: &SubsonicCredential,
        who: Option<&str>,
    ) {
        self.add(
            true,
            acquisition_tracker::key_of(provider, album_id),
            credential,
            who,
        );
    }

    fn add(&self, album: bool, key: String, credential: &SubsonicCredential, who: Option<&str>) {
        // Held for up to a day, so a password is swapped for a token made from it first.
        let credential = credential.without_password();
        // An API key names nobody until Navidrome says, so part of its fingerprint stands in.
        let name = match who.filter(|w| !dotnet::is_blank(w)) {
            Some(who) => who.trim().to_string(),
            None => format!("API key {}", &credential.fingerprint()[..8]),
        };
        let now = self.now();
        let mut holds = self.holds.lock();
        prune(&mut holds, now);
        let list = if album {
            &mut holds.albums
        } else {
            &mut holds.songs
        };
        // A second star from the same person replaces the first: one favorite each.
        list.retain(|hold| !(hold.key == key && eq_ignore_case(&hold.who, &name)));
        list.push(Hold {
            key,
            credential,
            who: name,
            held_at: now,
        });
        let over = (holds.songs.len() + holds.albums.len()).saturating_sub(AcquisitionTracker::CAPACITY);
        if over > 0 {
            let list = if album {
                &mut holds.albums
            } else {
                &mut holds.songs
            };
            let n = over.min(list.len());
            list.drain(..n);
        }
    }

    fn on_ended(self: &Arc<Self>, end: &AcquisitionEnd) {
        let (songs, albums) = {
            let mut holds = self.holds.lock();
            let songs = take(&mut holds.songs, |hold| hold.key == end.key);
            // An album keeps its hold through a track that failed: the next one may land.
            let albums = if end.library_id.is_none() {
                Vec::new()
            } else {
                take(&mut holds.albums, |hold| end.album_keys.contains(&hold.key))
            };
            (songs, albums)
        };
        if songs.len() + albums.len() == 0 {
            return;
        }
        let Some(library_id) = end.library_id.clone() else {
            if end.done {
                for hold in &songs {
                    info!(
                        "'{} - {}' is in the folder but Navidrome did not show it, so it was not favorited for {}",
                        end.artist.as_deref().unwrap_or_default(),
                        end.title.as_deref().unwrap_or_default(),
                        hold.who
                    );
                }
            }
            return;
        };
        if !self.settings.current().subsonic.star_downloads_for_requester {
            return;
        }
        // Off whatever reported the end, and carrying no request's context, so a call made as
        // one person can never pick up another request's parameters.
        let stars = Arc::clone(self);
        spawn(async move { stars.place(library_id, songs, albums).await });
    }

    async fn place(&self, library_id: String, songs: Vec<Hold>, albums: Vec<Hold>) {
        for hold in &songs {
            self.star_song(&hold.credential, &hold.who, &library_id).await;
        }
        for hold in &albums {
            let attempt: anyhow::Result<()> = async {
                let body = self
                    .call(
                        "rest/getSong",
                        hold.credential.parameters(&[("id", library_id.as_str())]),
                    )
                    .await?;
                let Some(album_id) = song_facts(&body)?.and_then(|facts| facts.album_id) else {
                    info!(
                        "Navidrome did not say which album {library_id} is on, so no album was favorited for {}",
                        hold.who
                    );
                    return Ok(());
                };
                let body = self
                    .call("rest/star", hold.credential.parameters(&[("albumId", album_id.as_str())]))
                    .await?;
                report(&body, &hold.who, &format!("album {album_id}"));
                Ok(())
            }
            .await;
            if let Err(e) = attempt {
                info!(
                    "Could not favorite the album of {library_id} for {}: {e}",
                    hold.who
                );
            }
        }
    }

    /// A heart on a song already in the library: favorite the library's copy now for whoever
    /// hearted it, whatever StarDownloadsForRequester says, since that setting is only about
    /// downloads. By its Navidrome id, or found by its path when only that is known. False when
    /// nobody's sign-in was held (Octo's own apps, whose heart means Add).
    pub fn favorite_owned(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        library_id: Option<&str>,
        artist: &str,
        title: &str,
        path: &str,
    ) -> bool {
        let key = acquisition_tracker::key_of(provider, external_id);
        let songs = take(&mut self.holds.lock().songs, |hold| hold.key == key);
        if songs.is_empty() {
            return false;
        }
        for hold in songs {
            match library_id {
                Some(library_id) => {
                    let stars = Arc::clone(self);
                    let library_id = library_id.to_string();
                    spawn(async move { stars.star_song(&hold.credential, &hold.who, &library_id).await });
                }
                None => self.star_when_visible(&hold.credential, &hold.who, artist, title, path),
            }
        }
        true
    }

    /// A heart on an album already whole in the library: favorite the album now, found
    /// through one of its songs, whatever StarDownloadsForRequester says.
    pub fn favorite_owned_album(
        self: &Arc<Self>,
        provider: &str,
        album_id: &str,
        library_id_of_a_song: &str,
    ) -> bool {
        let key = acquisition_tracker::key_of(provider, album_id);
        let albums = take(&mut self.holds.lock().albums, |hold| hold.key == key);
        if albums.is_empty() {
            return false;
        }
        let stars = Arc::clone(self);
        let library_id = library_id_of_a_song.to_string();
        spawn(async move { stars.place(library_id, Vec::new(), albums).await });
        true
    }

    /// Whether this person has favorited a song. False when Navidrome cannot say.
    pub async fn is_starred(&self, credential: &SubsonicCredential, navidrome_id: &str) -> bool {
        let attempt: anyhow::Result<bool> = async {
            let body = self
                .call("rest/getSong", credential.parameters(&[("id", navidrome_id)]))
                .await?;
            Ok(song_facts(&body)?.is_some_and(|facts| facts.starred))
        }
        .await;
        attempt.unwrap_or_else(|e| {
            debug!("Could not read whether {navidrome_id} is a favorite: {e}");
            false
        })
    }

    /// Favorite a file for this person once Navidrome shows it, for a replacement that
    /// arrives as a new song. Polls about ten minutes, then gives up with a log line.
    pub fn star_when_visible(
        self: &Arc<Self>,
        credential: &SubsonicCredential,
        who: &str,
        artist: &str,
        title: &str,
        path: &str,
    ) {
        let stars = Arc::clone(self);
        let (credential, who, artist, title, path) = (
            credential.clone(),
            who.to_string(),
            artist.to_string(),
            title.to_string(),
            path.to_string(),
        );
        spawn(async move {
            let seams = stars.seams.lock().clone();
            let Some(lookup) = seams.library_lookup.or_else(|| stars.resolve_lookup()) else {
                info!(
                    "Octo has no Navidrome sign-in of its own yet, so the replacement of '{title}' was not favorited for {who}"
                );
                return;
            };
            for attempt in 0..seams.visibility_attempts.max(1) {
                if attempt > 0 {
                    tokio::time::sleep(seams.visibility_poll).await;
                }
                let id = lookup(artist.clone(), title.clone(), path.clone())
                    .await
                    .unwrap_or_else(|e| {
                        debug!("Library lookup for {path} failed: {e}");
                        None
                    });
                if let Some(id) = id.filter(|i| !dotnet::is_blank(i)) {
                    stars.star_song(&credential, &who, &id).await;
                    return;
                }
            }
            info!("Navidrome never showed the replacement of '{title}', so it was not favorited for {who}");
        });
    }

    async fn star_song(&self, credential: &SubsonicCredential, who: &str, library_id: &str) {
        let attempt: anyhow::Result<()> = async {
            // Already a favorite: starring again would only move it to the top of the list.
            let body = self
                .call("rest/getSong", credential.parameters(&[("id", library_id)]))
                .await?;
            if song_facts(&body)?.is_some_and(|facts| facts.starred) {
                return Ok(());
            }
            let body = self
                .call("rest/star", credential.parameters(&[("id", library_id)]))
                .await?;
            report(&body, who, library_id);
            Ok(())
        }
        .await;
        if let Err(e) = attempt {
            info!("Could not favorite {library_id} for {who}: {e}");
        }
    }

    async fn call(&self, endpoint: &str, parameters: IndexMap<String, String>) -> anyhow::Result<Vec<u8>> {
        let call = self.seams.lock().call.clone();
        if let Some(call) = call {
            return call(endpoint.to_string(), parameters).await;
        }
        let Some(navidrome) = &self.navidrome else {
            anyhow::bail!("there is no Navidrome to call");
        };
        Ok(navidrome.proxy.relay(endpoint, &parameters).await?.body.to_vec())
    }

    fn resolve_lookup(&self) -> Option<LibraryLookup> {
        let navidrome = self.navidrome.as_ref()?;
        navidrome.identity.get_scan_auth()?;
        let resolver = navidrome.resolver.clone();
        Some(Arc::new(move |artist, title, path| {
            let resolver = resolver.clone();
            async move { Ok(resolver.find_id_by_path(&artist, &title, &path).await) }.boxed()
        }))
    }
}

impl Drop for StarOnArrival {
    fn drop(&mut self) {
        self.tracker.unsubscribe_ended(self.subscription);
        let lost = {
            let mut holds = self.holds.lock();
            let lost = holds.songs.len() + holds.albums.len();
            holds.songs.clear();
            holds.albums.clear();
            lost
        };
        if lost > 0 {
            info!(
                "Octo is stopping with {lost} starred download(s) still to favorite; they will arrive without the favorite"
            );
        }
    }
}

fn report(body: &[u8], who: &str, what: &str) {
    if CredentialCheck::status(body).as_deref() == Some("ok") {
        info!("Favorited {what} for {who}, who starred it");
    } else {
        // Most likely the password changed while the download ran.
        info!("Navidrome would not favorite {what} for {who}");
    }
}

/// What a `getSong` answer says about the song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongFacts {
    pub starred: bool,
    pub album_id: Option<String>,
}

/// `Ok(None)` for a body that is not JSON or has no song; `Err` where `JsonElement` threw (a
/// node that is not an object), as the C# let that out to its caller's catch.
pub fn song_facts(body: &[u8]) -> anyhow::Result<Option<SongFacts>> {
    let Ok(document) = serde_json::from_slice::<Value>(body) else {
        return Ok(None);
    };
    fn property<'a>(element: &'a Value, name: &str) -> anyhow::Result<Option<&'a Value>> {
        match element {
            Value::Object(map) => Ok(map.get(name)),
            _ => anyhow::bail!("the element is not an object"),
        }
    }
    let Some(response) = property(&document, "subsonic-response")? else {
        return Ok(None);
    };
    let Some(song) = property(response, "song")? else {
        return Ok(None);
    };
    Ok(Some(SongFacts {
        starred: matches!(property(song, "starred")?, Some(Value::String(_))),
        album_id: match property(song, "albumId")? {
            Some(Value::String(id)) => Some(id.clone()),
            _ => None,
        },
    }))
}

fn prune(holds: &mut Holds, now: DateTime<Utc>) {
    holds
        .songs
        .retain(|hold| now - hold.held_at < AcquisitionTracker::STALLED_RETENTION);
    holds
        .albums
        .retain(|hold| now - hold.held_at < AcquisitionTracker::STALLED_RETENTION);
}

fn take(holds: &mut Vec<Hold>, matches: impl Fn(&Hold) -> bool) -> Vec<Hold> {
    let (taken, kept): (Vec<Hold>, Vec<Hold>) = std::mem::take(holds).into_iter().partition(|h| matches(h));
    *holds = kept;
    taken
}

/// `Task.Run` off the caller; outside a runtime there is nowhere to run it, and nothing is sent.
fn spawn(work: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(work);
    }
}

#[cfg(test)]
#[path = "star_on_arrival_tests.rs"]
mod tests;
