//! Port of `Services/Subsonic/SubsonicCredential.cs`.

use std::fmt;

use indexmap::IndexMap;
use md5::Md5;
use sha2::{Digest, Sha256};

/// Every parameter Navidrome signs a Subsonic request in with.
const SIGN_IN_KEYS: [&str; 6] = ["u", "t", "s", "p", "apiKey", "jwt"];

/// The part of a Subsonic request that signs it in, and nothing else: u with t and s, u with p,
/// or an API key. Copied out so a check, or a call made later as that person, carries none of
/// the ids, queries or other values the client sent.
///
/// In memory only. `Display`/`Debug` name the user and no more, because a record that carries
/// one prints its members, and a cache files it under [`SubsonicCredential::fingerprint`], a
/// SHA-256 of the values.
#[derive(Clone, PartialEq, Eq)]
pub struct SubsonicCredential {
    values: IndexMap<String, String>,
    version: String,
    client: String,
    fingerprint: String,
}

impl SubsonicCredential {
    fn new(values: IndexMap<String, String>, version: String, client: String) -> Self {
        let canonical = SIGN_IN_KEYS
            .iter()
            .map(|key| format!("{key}={}", values.get(*key).map_or("", String::as_str)))
            .collect::<Vec<_>>()
            .join("\n");
        // Convert.ToHexString: upper case.
        let fingerprint = hex::encode_upper(Sha256::digest(canonical.as_bytes()));
        SubsonicCredential {
            values,
            version,
            client,
            fingerprint,
        }
    }

    /// u as sent, or `None` for an API key sign-in, which names nobody by itself.
    pub fn user(&self) -> Option<&str> {
        self.values.get("u").map(String::as_str)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The client name the request sent as c, or "octo" when it sent none.
    pub fn client(&self) -> &str {
        &self.client
    }

    /// A SHA-256 of the sign-in, for filing an answer under. Never the values.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The sign-in a request carries, or `None` when it carries none.
    pub fn from<'a, I>(parameters: I) -> Option<SubsonicCredential>
    where
        I: IntoIterator<Item = (&'a String, &'a String)>,
    {
        let parameters: Vec<(&String, &String)> = parameters.into_iter().collect();
        let get = |key: &str| {
            parameters
                .iter()
                .find(|(k, _)| k.as_str() == key)
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.is_empty())
        };
        let mut values = IndexMap::new();
        for key in SIGN_IN_KEYS {
            if let Some(value) = get(key) {
                values.insert(key.to_string(), value.to_string());
            }
        }
        // A name alone proves nothing, and Navidrome would refuse it anyway.
        if values.keys().all(|key| key == "u") {
            return None;
        }
        Some(SubsonicCredential::new(
            values,
            get("v").unwrap_or("1.16.1").to_string(),
            get("c").unwrap_or("octo").to_string(),
        ))
    }

    /// Parameters for a call made as this person: the sign-in, their version, and the client
    /// name their request sent, so Navidrome files the call under the player it already keeps
    /// for that app (Octo's relays pass c through unchanged). Always JSON. A fresh copy each
    /// time, so a caller can add an id.
    pub fn parameters(&self, extra: &[(&str, &str)]) -> IndexMap<String, String> {
        let mut parameters = self.values.clone();
        parameters.insert("v".into(), self.version.clone());
        parameters.insert("c".into(), self.client.clone());
        parameters.insert("f".into(), "json".into());
        for (key, value) in extra {
            parameters.insert((*key).to_string(), (*value).to_string());
        }
        parameters
    }

    /// The same sign-in with its password swapped for a token, for holding: s is 12 random hex
    /// characters and t the lowercase hex MD5 of the password and s, which Navidrome takes as
    /// u with t and s. A password sent as "enc:" and hex is decoded first. The token still signs
    /// in as the person, but it is not their password. Without p, this credential as it is.
    pub fn without_password(&self) -> SubsonicCredential {
        let Some(password) = self.values.get("p") else {
            return self.clone();
        };
        let mut password = password.clone();
        if let Some(encoded) = password.strip_prefix("enc:") {
            // Not hex after all: the token is then made from what was sent, and Navidrome,
            // which would have refused that password too, refuses the token.
            if let Ok(bytes) = hex::decode(encoded) {
                password = String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        let salt = hex::encode(rand::random::<[u8; 6]>());
        let token = hex::encode(Md5::digest(format!("{password}{salt}").as_bytes()));
        let mut values = IndexMap::new();
        values.insert("t".to_string(), token);
        values.insert("s".to_string(), salt);
        if let Some(user) = self.user() {
            values.insert("u".to_string(), user.to_string());
        }
        SubsonicCredential::new(values, self.version.clone(), self.client.clone())
    }

    /// The raw sign-in values, for tests in this crate.
    #[cfg(test)]
    fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

impl fmt::Display for SubsonicCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SubsonicCredential({})", self.user().unwrap_or("API key"))
    }
}

impl fmt::Debug for SubsonicCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// `CredentialCheckTests.Credential_ToString_HidesTheSecret`.
    #[test]
    fn credential_to_string_hides_the_secret() {
        let token =
            SubsonicCredential::from(&params(&[("u", "alice"), ("t", "tok-3f9a"), ("s", "salt-77c1")]))
                .expect("a sign-in");
        let key = SubsonicCredential::from(&params(&[("apiKey", "key-b81e")])).expect("a sign-in");

        for text in [token.to_string(), format!("{token:?}")] {
            assert!(text.contains("alice"));
            assert!(!text.contains("tok-3f9a"));
            assert!(!text.contains("salt-77c1"));
        }
        assert!(!key.to_string().contains("key-b81e"));
        assert!(!format!("{key:?}").contains("key-b81e"));
    }

    /// The first half of `CredentialCheckTests.NoSignIn_IsRefusedWithoutAsking`.
    #[test]
    fn a_name_alone_is_no_sign_in() {
        assert!(SubsonicCredential::from(&params(&[("u", "alice")])).is_none());
        assert!(SubsonicCredential::from(&params(&[("u", "alice"), ("t", "")])).is_none());
        assert!(SubsonicCredential::from(&params(&[])).is_none());
    }

    #[test]
    fn parameters_carry_the_sign_in_version_client_and_json_only() {
        let credential = SubsonicCredential::from(&params(&[
            ("u", "alice"),
            ("t", "good"),
            ("s", "salt"),
            ("v", "1.16.1"),
            ("c", "Symfonium"),
            ("id", "x"),
            ("f", "xml"),
        ]))
        .expect("a sign-in");
        let sent = credential.parameters(&[("id", "42")]);
        let keys: Vec<&str> = sent.keys().map(String::as_str).collect();
        assert_eq!(keys, ["u", "t", "s", "v", "c", "f", "id"]);
        assert_eq!(sent["c"], "Symfonium");
        assert_eq!(sent["f"], "json");

        let defaults = SubsonicCredential::from(&params(&[("apiKey", "k")])).expect("a sign-in");
        assert_eq!((defaults.version(), defaults.client()), ("1.16.1", "octo"));
        assert_eq!(defaults.user(), None);
    }

    #[test]
    fn the_fingerprint_is_a_sha256_of_every_sign_in_key() {
        let credential = SubsonicCredential::from(&params(&[("u", "alice"), ("t", "good"), ("s", "salt")]))
            .expect("a sign-in");
        let expected = hex::encode_upper(Sha256::digest(b"u=alice\nt=good\ns=salt\np=\napiKey=\njwt="));
        assert_eq!(credential.fingerprint(), expected);
        let other = SubsonicCredential::from(&params(&[("u", "alice"), ("t", "good"), ("s", "salt2")]))
            .expect("a sign-in");
        assert_ne!(credential.fingerprint(), other.fingerprint());
    }

    #[test]
    fn without_password_swaps_the_password_for_a_token() {
        let credential =
            SubsonicCredential::from(&params(&[("u", "alice"), ("p", "enc:736563726574"), ("c", "x")]))
                .expect("a sign-in");
        let held = credential.without_password();
        let salt = held.value("s").expect("a salt");
        assert_eq!(salt.len(), 12);
        assert!(
            salt.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let expected = hex::encode(Md5::digest(format!("secret{salt}").as_bytes()));
        assert_eq!(held.value("t"), Some(expected.as_str()));
        assert_eq!(held.value("p"), None);
        assert_eq!((held.user(), held.client()), (Some("alice"), "x"));

        // Not hex: the token is made from what was sent.
        let odd = SubsonicCredential::from(&params(&[("u", "a"), ("p", "enc:zz")])).expect("a sign-in");
        let held = odd.without_password();
        let salt = held.value("s").expect("a salt");
        let expected = hex::encode(Md5::digest(format!("enc:zz{salt}").as_bytes()));
        assert_eq!(held.value("t"), Some(expected.as_str()));

        let token =
            SubsonicCredential::from(&params(&[("u", "a"), ("t", "x"), ("s", "y")])).expect("a sign-in");
        assert_eq!(token.without_password(), token);
    }
}
