//! Port of `Services/Subsonic/RecentScrobbles.cs`.

use std::collections::{HashSet, VecDeque};

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::dotnet;
use parking_lot::Mutex;

/// The completed plays each listener reported in the last few minutes, so a play a client sends
/// twice is learned from once. Some players post the same completed scrobble again (a retry after
/// a slow answer, a second scrobble at the end of the song), and each copy used to reach Last.fm,
/// ListenBrainz and the radio profile as another play.
///
/// A play is the song and the time the client gave for it, or, without one, the minute it
/// arrived in. Kept per listener, newest first, and bounded both ways, so a flood from one
/// listener pushes out only that listener's oldest plays.
#[derive(Default)]
pub struct RecentScrobbles {
    /// Keyed by `StringComparer.OrdinalIgnoreCase`.
    listeners: Mutex<IndexMap<String, Plays>>,
}

impl RecentScrobbles {
    /// How long a play is remembered. A repeat later than this counts again.
    pub const WINDOW: TimeDelta = TimeDelta::minutes(10);

    /// Plays remembered per listener. Far more than anyone finishes in ten minutes.
    pub const PER_LISTENER: usize = 256;

    /// Listeners remembered at once.
    pub const LISTENERS: usize = 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// True the first time this listener reports this play inside the window, false for a repeat.
    ///
    /// `time`: the client's own time for the play (Subsonic's `time`), or None.
    pub fn first_report(
        &self,
        username: &str,
        song_id: &str,
        time: Option<&str>,
        now_utc: DateTime<Utc>,
    ) -> bool {
        let play = play(song_id, time, now_utc);
        let key = dotnet::ordinal_ignore_case_key(username);
        let mut listeners = self.listeners.lock();
        if !listeners.contains_key(&key) {
            if listeners.len() >= Self::LISTENERS {
                forget_quietest_listener(&mut listeners);
            }
            listeners.insert(key.clone(), Plays::default());
        }
        let plays = listeners.get_mut(&key).expect("the listener was just added");
        plays.last_report_utc = now_utc;
        plays.add(play, now_utc)
    }

    /// Takes back a play [`first_report`](Self::first_report) let through, because nothing was
    /// learned from it: the song could not be looked up this time. The client's retry then counts
    /// as the play it is. Called with the same arguments first_report had.
    pub fn withdraw(&self, username: &str, song_id: &str, time: Option<&str>, now_utc: DateTime<Utc>) {
        let play = play(song_id, time, now_utc);
        if let Some(plays) = self
            .listeners
            .lock()
            .get_mut(&dotnet::ordinal_ignore_case_key(username))
        {
            plays.remove(&play);
        }
    }
}

/// `nowUtc.Ticks / TimeSpan.TicksPerMinute`: minutes since 0001-01-01.
fn minute(now_utc: DateTime<Utc>) -> i64 {
    const UNIX_EPOCH_SECONDS: i64 = 62_135_596_800;
    (now_utc.timestamp() + UNIX_EPOCH_SECONDS).div_euclid(60)
}

fn play(song_id: &str, time: Option<&str>, now_utc: DateTime<Utc>) -> String {
    match time {
        Some(time) if !time.is_empty() => format!("{song_id}\n{time}"),
        _ => format!("{song_id}\nminute {}", minute(now_utc)),
    }
}

/// Makes room for a new listener by forgetting the one heard from longest ago.
fn forget_quietest_listener(listeners: &mut IndexMap<String, Plays>) {
    // MinBy: the first of the quietest.
    let mut quietest: Option<(&String, DateTime<Utc>)> = None;
    for (key, plays) in listeners.iter() {
        if quietest.is_none_or(|(_, at)| plays.last_report_utc < at) {
            quietest = Some((key, plays.last_report_utc));
        }
    }
    if let Some(key) = quietest.map(|(key, _)| key.clone()) {
        listeners.shift_remove(&key);
    }
}

struct Plays {
    /// Newest first.
    order: VecDeque<(String, DateTime<Utc>)>,
    index: HashSet<String>,
    last_report_utc: DateTime<Utc>,
}

impl Default for Plays {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            index: HashSet::new(),
            last_report_utc: DateTime::<Utc>::MIN_UTC,
        }
    }
}

impl Plays {
    fn add(&mut self, play: String, now_utc: DateTime<Utc>) -> bool {
        // Oldest last: drop whatever has aged out of the window before looking.
        while let Some((oldest, at)) = self.order.back()
            && now_utc - *at >= RecentScrobbles::WINDOW
        {
            self.index.remove(oldest);
            self.order.pop_back();
        }
        if self.index.contains(&play) {
            return false;
        }
        self.index.insert(play.clone());
        self.order.push_front((play, now_utc));
        if self.order.len() > RecentScrobbles::PER_LISTENER
            && let Some((oldest, _)) = self.order.pop_back()
        {
            self.index.remove(&oldest);
        }
        true
    }

    fn remove(&mut self, play: &str) {
        if !self.index.remove(play) {
            return;
        }
        if let Some(position) = self.order.iter().position(|(queued, _)| queued == play) {
            self.order.remove(position);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0)
            .single()
            .expect("a time")
            + TimeDelta::minutes(minutes)
    }

    /// `ScrobbleRetryTests.APlayNotTakenTheFirstTime_CountsWhenTheClientSendsItAgain`, at the
    /// store: the play withdrawn because the song could not be looked up counts on the retry,
    /// once.
    #[test]
    fn a_play_not_taken_the_first_time_counts_when_the_client_sends_it_again() {
        let recent = RecentScrobbles::new();
        let time = Some("1759579200000");

        assert!(recent.first_report("bob", "local-song", time, at(0)));
        // Navidrome could not say what the library song was: nothing learned.
        recent.withdraw("bob", "local-song", time, at(0));

        assert!(recent.first_report("bob", "local-song", time, at(0)));
        assert!(!recent.first_report("bob", "local-song", time, at(0)));
    }

    /// Rust-only: a repeat is one play inside the window and per listener (ignoring case); a
    /// play without a time is its minute.
    #[test]
    fn repeats_are_one_play_inside_the_window() {
        let recent = RecentScrobbles::new();
        assert!(recent.first_report("Bob", "a", None, at(0)));
        assert!(!recent.first_report("bob", "a", None, at(0) + TimeDelta::seconds(30)));
        assert!(recent.first_report("bob", "a", None, at(1)));
        assert!(recent.first_report("alice", "a", None, at(0)));
        assert!(recent.first_report("bob", "a", Some("t1"), at(2)));
        assert!(!recent.first_report("bob", "a", Some("t1"), at(11)));
        // Ten minutes after it was first heard, it counts again.
        assert!(recent.first_report("bob", "a", Some("t1"), at(12)));
        recent.withdraw("nobody", "a", None, at(0));
        recent.withdraw("bob", "never", None, at(0));
    }

    /// Rust-only: both bounds. A listener's 257th play pushes out their oldest, and the
    /// 1025th listener pushes out the one heard from longest ago.
    #[test]
    fn plays_and_listeners_are_bounded() {
        let recent = RecentScrobbles::new();
        for i in 0..=RecentScrobbles::PER_LISTENER {
            assert!(recent.first_report("bob", &format!("s{i}"), None, at(0)));
        }
        assert!(
            recent.first_report("bob", "s0", None, at(0)),
            "the oldest was pushed out"
        );
        assert!(!recent.first_report("bob", "s2", None, at(0)));

        let many = RecentScrobbles::new();
        let t = Some("t");
        assert!(many.first_report("quiet", "a", t, at(0)));
        for i in 1..RecentScrobbles::LISTENERS {
            assert!(many.first_report(&format!("u{i}"), "a", t, at(1)));
        }
        assert!(many.first_report("newcomer", "a", t, at(2)));
        assert!(many.first_report("quiet", "a", t, at(2)), "quiet was forgotten");
        // Coming back, quiet pushed out u1, the first of the next quietest.
        assert!(!many.first_report("u2", "a", t, at(2)));
        assert!(many.first_report("u1", "a", t, at(2)), "u1 was forgotten");
    }

    #[test]
    fn minutes_count_from_year_one() {
        // DateTime(2026, 10, 4, 12, 0, 0).Ticks / TicksPerMinute.
        assert_eq!(minute(at(0)), 639_267_120_000_000_000 / 600_000_000);
    }
}
