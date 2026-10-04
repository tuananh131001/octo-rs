//! The pure half of `Services/LastFm/LastFmRadioStateStore.cs`: the station ids and the
//! constants the store prunes by. The store itself is `octo::services::last_fm`.

use chrono::TimeDelta;
use sha2::{Digest, Sha256};

use crate::common::dotnet;

pub const CURRENT_VERSION: i32 = 1;
pub const MAX_PLAYS_PER_USER: usize = 2_000;
pub const MAX_USERS: usize = 100;
pub const MAX_UNAVAILABLE_TRACKS_PER_USER: usize = 500;
pub const UNAVAILABLE_TRACK_COOLDOWN: TimeDelta = TimeDelta::hours(24);
pub const DUPLICATE_WINDOW: TimeDelta = TimeDelta::minutes(5);

/// The key a listener is filed under: trimmed and lower-cased.
pub fn user_key(username: &str) -> String {
    dotnet::to_lower_invariant(username.trim())
}

/// A station's id: "or" and 20 base-62 characters of SHA-256 over the listener and the station
/// key, so the same station keeps its id across refreshes and restarts.
pub fn station_id(username: &str, station_key: &str) -> String {
    let hash = Sha256::digest(
        format!(
            "{}|{}",
            user_key(username),
            dotnet::to_lower_invariant(station_key.trim())
        )
        .as_bytes(),
    );
    format!("or{}", to_base62(&hash, 20))
}

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The first 16 bytes as one unsigned big-endian number, written least significant digit first
/// and padded with '0' to `length`.
pub fn to_base62(bytes: &[u8], length: usize) -> String {
    let mut first = [0u8; 16];
    first.copy_from_slice(&bytes[..16]);
    let mut value = u128::from_be_bytes(first);
    let mut builder = String::with_capacity(length);
    while builder.len() < length {
        let remainder = (value % 62) as usize;
        value /= 62;
        builder.push(ALPHABET[remainder] as char);
        if value == 0 {
            break;
        }
    }
    while builder.len() < length {
        builder.push('0');
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What .NET 9 computed (BigInteger over the hash).
    #[test]
    fn station_ids_are_what_dotnet_computed() {
        assert_eq!(station_id("alice", "mix"), "or5D611eeSONhWnfZPwUAs");
        assert_eq!(station_id(" Brandon ", " Your-Mix "), "or9Pos9xe8DfTyQJGuZqZG");
        assert_eq!(station_id("Björk", "artist-x"), "orwLd3nTWU1MPqQgsP05zv");
    }

    #[test]
    fn small_numbers_are_padded() {
        let mut bytes = [0u8; 32];
        bytes[15] = 63;
        assert_eq!(to_base62(&bytes, 5), "11000");
        assert_eq!(to_base62(&[0u8; 16], 3), "000");
    }
}
