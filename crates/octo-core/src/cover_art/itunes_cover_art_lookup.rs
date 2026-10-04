//! The pure half of `Services/CoverArt/ITunesCoverArtLookup.cs`: the barcode forms, which
//! `ReleaseDistance` also reads. The lookup itself is
//! `octo::services::cover_art::itunes_cover_art_lookup`.

use crate::common::dotnet;

/// A barcode as given, and as the 12-digit UPC when it is a longer form with
/// leading zeros. Only digits count; anything else is not a barcode.
pub fn barcode_forms(code: Option<&str>) -> Vec<String> {
    let digits: String = code
        .unwrap_or("")
        .chars()
        .filter(|&c| dotnet::is_digit_utf16(c))
        .collect();
    let length = dotnet::utf16_len(&digits);
    if !(8..=14).contains(&length) {
        return Vec::new();
    }
    let mut forms = vec![digits.clone()];
    let trimmed = digits.trim_start_matches('0');
    let upc = format!(
        "{}{trimmed}",
        "0".repeat(12usize.saturating_sub(dotnet::utf16_len(trimmed)))
    );
    if length > 12 && dotnet::utf16_len(&upc) == 12 && upc != digits {
        forms.push(upc);
    }
    forms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barcode_forms_add_the_upc_of_a_zero_padded_ean() {
        assert_eq!(
            barcode_forms(Some("0602475682233")),
            ["0602475682233", "602475682233"]
        );
        assert_eq!(barcode_forms(Some("724384559922")), ["724384559922"]);
        assert_eq!(barcode_forms(Some("1234567890123")), ["1234567890123"]);
        assert_eq!(barcode_forms(Some("00012345678")), ["00012345678"]);
        assert_eq!(
            barcode_forms(Some("0-602475-682233")),
            ["0602475682233", "602475682233"]
        );
        assert!(barcode_forms(Some("1234567")).is_empty());
        assert!(barcode_forms(Some("123456789012345")).is_empty());
        assert!(barcode_forms(None).is_empty());
    }
}
