//! `Services/Metadata/AcceptLanguageHeader.cs`.
//!
//! STUB(2-B metadata): replaced when 2-B lands. It splits and trims as the C# did, but does not
//! validate each part as `TryParseAdd` did, and joins with ", " as the header collection did.

use crate::settings::MetadataSettings;

/// The Accept-Language header value for the configured metadata language, or None when none
/// is to be sent. Tolerates a pasted browser-style list ("en-US,en;q=0.9").
pub fn header_value(settings: &MetadataSettings) -> Option<String> {
    let parts: Vec<&str> = settings
        .language
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}
