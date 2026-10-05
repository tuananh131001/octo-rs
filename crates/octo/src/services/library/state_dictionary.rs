//! A `Dictionary<string, T>` property of a state file, as System.Text.Json wrote and read it: a
//! JSON object in the dictionary's enumeration order. Not a C# file: the stores of 5-B
//! (`quality-upgrade.json`'s `Attempts`, `review-sweep.json`'s `Checked`) each had one.
//!
//! On read, a repeated key keeps its first place and its last value, as the converter's
//! `dict[key] = value` did, and a JSON `null` reads as an empty dictionary.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::services::framework::DotnetDictionary;

pub fn serialize<S: Serializer, T: Serialize>(map: &DotnetDictionary<T>, s: S) -> Result<S::Ok, S::Error> {
    let mut out = s.serialize_map(Some(map.len()))?;
    for (key, value) in map.iter() {
        out.serialize_entry(key, value)?;
    }
    out.end()
}

pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<DotnetDictionary<T>, D::Error> {
    d.deserialize_option(OptionVisitor(PhantomData))
}

struct OptionVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for OptionVisitor<T> {
    type Value = DotnetDictionary<T>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object or null")
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(DotnetDictionary::new())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(DotnetDictionary::new())
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(MapVisitor(PhantomData))
    }
}

struct MapVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for MapVisitor<T> {
    type Value = DotnetDictionary<T>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut map = DotnetDictionary::new();
        while let Some((key, value)) = access.next_entry::<String, T>()? {
            map.set(key, value);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize)]
    struct Holder {
        #[serde(with = "super")]
        map: DotnetDictionary<i32>,
    }

    #[test]
    fn a_repeated_key_keeps_its_first_place_and_its_last_value_and_null_is_empty() {
        let holder: Holder = serde_json::from_str(r#"{"map":{"b":1,"a":2,"b":3}}"#).expect("reads");
        assert_eq!(octo_core::json::to_string(&holder), r#"{"map":{"b":3,"a":2}}"#);
        let empty: Holder = serde_json::from_str(r#"{"map":null}"#).expect("reads null");
        assert!(empty.map.is_empty());
    }
}
