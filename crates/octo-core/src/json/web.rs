//! Reading request bodies the way ASP.NET Core's `JsonSerializerDefaults.Web` reads them.
//!
//! Controllers bound JSON bodies case-insensitively (`"url"`, `"Url"` and `"URL"` all fill
//! `Url`) and accepted numbers written as strings (`"30"`). The dashboard and the apps rely
//! on that leniency, so bodies are read through a deserializer over `serde_json::Value`
//! that matches struct fields ignoring case and underscores, and parses numbers and bools out
//! of strings when the field wants one.

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug)]
pub struct WebError(String);

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WebError {}

impl de::Error for WebError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        WebError(msg.to_string())
    }
}

/// Deserializes `T` from a JSON value with Web-defaults leniency.
pub fn from_value<T: DeserializeOwned>(value: &Value) -> Result<T, WebError> {
    T::deserialize(Lenient(value))
}

/// Parses bytes as JSON, then deserializes with Web-defaults leniency.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, WebError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| WebError(e.to_string()))?;
    from_value(&value)
}

fn normalise(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

#[derive(Clone, Copy)]
struct Lenient<'a>(&'a Value);

macro_rules! number {
    ($method:ident, $visit:ident, $ty:ty, $as:ident) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
            match self.0 {
                Value::Number(n) => match n.$as().and_then(|v| <$ty>::try_from(v).ok()) {
                    Some(v) => visitor.$visit(v),
                    None => Err(WebError(format!("{n} does not fit {}", stringify!($ty)))),
                },
                Value::String(s) => match s.trim().parse::<$ty>() {
                    Ok(v) => visitor.$visit(v),
                    Err(_) => Err(WebError(format!("'{s}' is not a number"))),
                },
                Value::Null => visitor.visit_none(),
                other => Err(WebError(format!("expected a number, found {other}"))),
            }
        }
    };
}

impl<'de, 'a> de::Deserializer<'de> for Lenient<'a> {
    type Error = WebError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    visitor.visit_u64(u)
                } else if let Some(i) = n.as_i64() {
                    visitor.visit_i64(i)
                } else {
                    visitor.visit_f64(n.as_f64().unwrap_or(0.0))
                }
            }
            Value::String(s) => visitor.visit_str(s),
            Value::Array(a) => visitor.visit_seq(Seq(a.iter())),
            Value::Object(m) => visitor.visit_map(Obj {
                iter: m.iter(),
                value: None,
            }),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::Bool(b) => visitor.visit_bool(*b),
            // STJ does not read bools from strings even under Web defaults, but being
            // lenient here costs nothing and the dashboard never sends them quoted.
            Value::String(s) if s.eq_ignore_ascii_case("true") => visitor.visit_bool(true),
            Value::String(s) if s.eq_ignore_ascii_case("false") => visitor.visit_bool(false),
            other => Err(WebError(format!("expected true or false, found {other}"))),
        }
    }

    number!(deserialize_i8, visit_i8, i8, as_i64);
    number!(deserialize_i16, visit_i16, i16, as_i64);
    number!(deserialize_i32, visit_i32, i32, as_i64);
    number!(deserialize_i64, visit_i64, i64, as_i64);
    number!(deserialize_u8, visit_u8, u8, as_u64);
    number!(deserialize_u16, visit_u16, u16, as_u64);
    number!(deserialize_u32, visit_u32, u32, as_u64);
    number!(deserialize_u64, visit_u64, u64, as_u64);

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::Number(n) => visitor.visit_f64(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => match s.trim().parse::<f64>() {
                Ok(v) => visitor.visit_f64(v),
                Err(_) => Err(WebError(format!("'{s}' is not a number"))),
            },
            other => Err(WebError(format!("expected a number, found {other}"))),
        }
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::String(s) => visitor.visit_str(s),
            Value::Null => visitor.visit_none(),
            other => Err(WebError(format!("expected a string, found {other}"))),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_any(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_any(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, WebError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, WebError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::Array(a) => visitor.visit_seq(Seq(a.iter())),
            Value::Null => visitor.visit_seq(Seq([].iter())),
            other => Err(WebError(format!("expected an array, found {other}"))),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: usize,
        visitor: V,
    ) -> Result<V::Value, WebError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        match self.0 {
            Value::Object(m) => visitor.visit_map(Obj {
                iter: m.iter(),
                value: None,
            }),
            Value::Null => visitor.visit_map(Obj {
                iter: EMPTY.iter(),
                value: None,
            }),
            other => Err(WebError(format!("expected an object, found {other}"))),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, WebError> {
        let Value::Object(m) = self.0 else {
            return Err(WebError(format!("expected an object, found {}", self.0)));
        };
        let wanted: Vec<(String, &'static str)> = fields.iter().map(|f| (normalise(f), *f)).collect();
        let mut entries: Vec<(&'static str, &'a Value)> = Vec::new();
        for (k, v) in m {
            let nk = normalise(k);
            // Two spellings of one field: the later wins, as STJ sets the property twice.
            let field = fields
                .iter()
                .find(|f| **f == k.as_str())
                .copied()
                .or_else(|| wanted.iter().find(|(n, _)| *n == nk).map(|(_, f)| *f));
            if let Some(f) = field {
                if let Some(slot) = entries.iter_mut().find(|(e, _)| *e == f) {
                    slot.1 = v;
                } else {
                    entries.push((f, v));
                }
            }
        }
        visitor.visit_map(Fields {
            iter: entries.into_iter(),
            value: None,
        })
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, WebError> {
        match self.0 {
            Value::String(s) => {
                let found = variants
                    .iter()
                    .find(|v| v.eq_ignore_ascii_case(s))
                    .copied()
                    .unwrap_or(s.as_str());
                visitor.visit_enum(found.into_deserializer())
            }
            Value::Number(n) => {
                let idx = n.as_u64().unwrap_or(u64::MAX) as usize;
                match variants.get(idx) {
                    Some(v) => visitor.visit_enum((*v).into_deserializer()),
                    None => Err(WebError(format!("{n} is not a valid value"))),
                }
            }
            Value::Object(m) if m.len() == 1 => {
                let (k, v) = m.iter().next().unwrap();
                visitor.visit_enum(Enum { variant: k, value: v })
            }
            other => Err(WebError(format!("expected an enum, found {other}"))),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, WebError> {
        visitor.visit_unit()
    }
}

static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);

struct Seq<'a, I: Iterator<Item = &'a Value>>(I);

impl<'de, 'a, I: Iterator<Item = &'a Value>> SeqAccess<'de> for Seq<'a, I> {
    type Error = WebError;
    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, WebError> {
        match self.0.next() {
            Some(v) => seed.deserialize(Lenient(v)).map(Some),
            None => Ok(None),
        }
    }
}

struct Obj<'a, I: Iterator<Item = (&'a String, &'a Value)>> {
    iter: I,
    value: Option<&'a Value>,
}

impl<'de, 'a, I: Iterator<Item = (&'a String, &'a Value)>> MapAccess<'de> for Obj<'a, I> {
    type Error = WebError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, WebError> {
        match self.iter.next() {
            Some((k, v)) => {
                self.value = Some(v);
                seed.deserialize(Lenient(&Value::String(k.clone()))).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, WebError> {
        seed.deserialize(Lenient(self.value.take().expect("value follows key")))
    }
}

struct Fields<'a, I: Iterator<Item = (&'static str, &'a Value)>> {
    iter: I,
    value: Option<&'a Value>,
}

impl<'de, 'a, I: Iterator<Item = (&'static str, &'a Value)>> MapAccess<'de> for Fields<'a, I> {
    type Error = WebError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, WebError> {
        match self.iter.next() {
            Some((k, v)) => {
                self.value = Some(v);
                seed.deserialize(k.into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, WebError> {
        seed.deserialize(Lenient(self.value.take().expect("value follows key")))
    }
}

struct Enum<'a> {
    variant: &'a str,
    value: &'a Value,
}

impl<'de, 'a> EnumAccess<'de> for Enum<'a> {
    type Error = WebError;
    type Variant = Lenient<'a>;
    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Lenient<'a>), WebError> {
        let v = seed.deserialize(self.variant.into_deserializer())?;
        Ok((v, Lenient(self.value)))
    }
}

impl<'de, 'a> VariantAccess<'de> for Lenient<'a> {
    type Error = WebError;
    fn unit_variant(self) -> Result<(), WebError> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, WebError> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, WebError> {
        de::Deserializer::deserialize_seq(self, visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, WebError> {
        de::Deserializer::deserialize_struct(self, "", fields, visitor)
    }
}

impl<'a> IntoDeserializer<'_, WebError> for Lenient<'a> {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        song_id: String,
        count: i32,
        #[serde(default)]
        tags: Vec<String>,
        rating: Option<i32>,
    }

    #[test]
    fn fields_match_ignoring_case_and_numbers_read_from_strings() {
        let b: Body =
            from_value(&json!({"SongId": "s1", "COUNT": "3", "tags": ["a"], "rating": null})).unwrap();
        assert_eq!(
            b,
            Body {
                song_id: "s1".into(),
                count: 3,
                tags: vec!["a".into()],
                rating: None
            }
        );
    }

    #[test]
    fn a_later_spelling_of_the_same_field_wins() {
        let b: Body = from_value(&json!({"songId": "first", "SONGID": "later", "count": 1})).unwrap();
        assert_eq!(b.song_id, "later");
    }
}
