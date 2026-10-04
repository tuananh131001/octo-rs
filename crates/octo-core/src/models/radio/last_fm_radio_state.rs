//! Port of `Models/Radio/LastFmRadioState.cs`: only the station and its tracks, as far as the
//! Subsonic playlist rows read them.
//!
//! STUB(5-C): replaced when 5-C (Last.fm radio) lands. 5-C ports the whole file (the state
//! document, users, plays, the station kind and the serialisation of `last-fm-radio.json`);
//! the fields below keep their C# names and types so the swap changes no caller.

use chrono::{DateTime, Utc};

/// One station: a read-only playlist of Last.fm suggestions for one listener.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LastFmRadioStation {
    pub id: String,
    pub key: String,
    pub name: String,
    pub owner: String,
    pub personalized: bool,
    pub definition_version: i32,
    pub created_utc: DateTime<Utc>,
    pub changed_utc: DateTime<Utc>,
    pub valid_until_utc: DateTime<Utc>,
    pub seeds: Vec<String>,
    pub tracks: Vec<LastFmRadioTrack>,
}

/// One suggestion on a station.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LastFmRadioTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<i32>,
    pub year: Option<i32>,
    pub score: f64,
    pub source: String,
    pub resolved_id: Option<String>,
    pub is_local: bool,
    pub external_provider: Option<String>,
    pub you_tube_id: Option<String>,
}
