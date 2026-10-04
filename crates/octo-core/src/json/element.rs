//! `System.Text.Json.JsonElement`'s reading rules over a `serde_json::Value`, for the ports
//! that walk an external service's answer by hand.
//!
//! The C# readers lean on `JsonElement` throwing where serde_json would quietly answer `None`:
//! `TryGetProperty` on something that is not an object, `GetString()` on a number,
//! `GetInt32()` on `3.5`, `GetProperty` on a missing key. Each throw ended the whole read and
//! landed in the caller's catch, so the Rust readers return a [`Result`] at the same places and
//! use `?` where the C# let the exception travel.

use serde_json::Value;

/// What a `JsonElement` accessor threw.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElementError {
    /// `InvalidOperationException`: the element is not of the kind the accessor needs.
    #[error(
        "The requested operation requires an element of type '{expected}', but the target element has type '{actual}'."
    )]
    WrongKind {
        expected: &'static str,
        actual: &'static str,
    },
    /// `KeyNotFoundException` from `GetProperty`.
    #[error("The given key '{0}' was not present in the dictionary.")]
    KeyNotFound(String),
    /// `FormatException`: a number that does not fit the type asked for.
    #[error("One or more errors occurred: the JSON value could not be converted to {0}.")]
    Format(&'static str),
}

pub type ElementResult<T> = Result<T, ElementError>;

/// `JsonElement.ValueKind`, by its .NET name.
pub fn kind(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "Object",
        Value::Array(_) => "Array",
        Value::String(_) => "String",
        Value::Number(_) => "Number",
        Value::Bool(true) => "True",
        Value::Bool(false) => "False",
        Value::Null => "Null",
    }
}

fn wrong(expected: &'static str, value: &Value) -> ElementError {
    ElementError::WrongKind {
        expected,
        actual: kind(value),
    }
}

/// `TryGetProperty`: throws unless the element is an object.
pub fn try_get_property<'a>(element: &'a Value, name: &str) -> ElementResult<Option<&'a Value>> {
    match element {
        Value::Object(map) => Ok(map.get(name)),
        other => Err(wrong("Object", other)),
    }
}

/// `GetProperty`: throws unless the element is an object that has the property.
pub fn get_property<'a>(element: &'a Value, name: &str) -> ElementResult<&'a Value> {
    try_get_property(element, name)?.ok_or_else(|| ElementError::KeyNotFound(name.to_string()))
}

/// `GetString()`: the text of a string, `None` for a JSON null, a throw for anything else.
pub fn get_string(value: &Value) -> ElementResult<Option<&str>> {
    match value {
        Value::String(text) => Ok(Some(text)),
        Value::Null => Ok(None),
        other => Err(wrong("String", other)),
    }
}

/// `TryGetInt32`: a number written as an integer that fits, else `None`. Never throws for a
/// number; a non-number throws, as `TryGetInt32` did.
pub fn try_get_int32(value: &Value) -> ElementResult<Option<i32>> {
    match value {
        Value::Number(number) => Ok(number.as_i64().and_then(|n| i32::try_from(n).ok())),
        other => Err(wrong("Number", other)),
    }
}

/// `GetInt32()`: throws `FormatException` for a number that is not an integer in range.
pub fn get_int32(value: &Value) -> ElementResult<i32> {
    try_get_int32(value)?.ok_or(ElementError::Format("Int32"))
}

/// `GetInt64()`.
pub fn get_int64(value: &Value) -> ElementResult<i64> {
    match value {
        Value::Number(number) => number.as_i64().ok_or(ElementError::Format("Int64")),
        other => Err(wrong("Number", other)),
    }
}

/// `GetDouble()`.
pub fn get_double(value: &Value) -> ElementResult<f64> {
    match value {
        Value::Number(number) => number.as_f64().ok_or(ElementError::Format("Double")),
        other => Err(wrong("Number", other)),
    }
}

/// `GetBoolean()`.
pub fn get_boolean(value: &Value) -> ElementResult<bool> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        other => Err(wrong("Boolean", other)),
    }
}

/// `EnumerateArray()`: throws unless the element is an array.
pub fn enumerate_array(value: &Value) -> ElementResult<&[Value]> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(wrong("Array", other)),
    }
}

/// `GetArrayLength()`.
pub fn array_length(value: &Value) -> ElementResult<usize> {
    enumerate_array(value).map(<[Value]>::len)
}

/// `GetRawText()`: the element written back compactly. serde_json does not keep the source
/// text, so this is a re-serialisation; the C# only parsed it again.
pub fn raw_text(value: &Value) -> String {
    value.to_string()
}

/// The readers' common `Str` helper:
/// `e.TryGetProperty(name, out v) && v.ValueKind == String ? v.GetString() : null`.
pub fn str_prop<'a>(element: &'a Value, name: &str) -> ElementResult<Option<&'a str>> {
    Ok(match try_get_property(element, name)? {
        Some(Value::String(text)) => Some(text),
        _ => None,
    })
}

/// [`str_prop`], owned.
pub fn string_prop(element: &Value, name: &str) -> ElementResult<Option<String>> {
    str_prop(element, name).map(|text| text.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accessors_throw_where_json_element_did() {
        assert!(try_get_property(&json!("x"), "a").is_err());
        assert_eq!(try_get_property(&json!({"a": 1}), "b").unwrap(), None);
        assert!(get_property(&json!({}), "id").is_err());
        assert!(get_string(&json!(5)).is_err());
        assert_eq!(get_string(&json!(null)).unwrap(), None);
        assert_eq!(get_int32(&json!(7)).unwrap(), 7);
        assert!(get_int32(&json!(3.5)).is_err());
        assert!(get_int32(&json!(4_000_000_000_i64)).is_err());
        assert_eq!(try_get_int32(&json!(3.5)).unwrap(), None);
        assert_eq!(str_prop(&json!({"a": 1}), "a").unwrap(), None);
        assert_eq!(str_prop(&json!({"a": "x"}), "a").unwrap(), Some("x"));
    }
}
