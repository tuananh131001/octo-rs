//! STUB(5-B): replaced when 5-B (queues and sweeps) lands with the port of
//! `Services/Library/DuplicateScanWorker.cs`. Only `IsLosslessFile` is here, ported as the C#
//! wrote it, which `LibraryOwnership` (4-D) ranks the library's copies by.

use octo_core::common::dotnet::eq_ignore_case;

const LOSSLESS_SUFFIXES: [&str; 8] = ["flac", "alac", "wav", "aiff", "aif", "ape", "wv", "dsf"];

/// ALAC arrives as m4a, told apart from AAC only by its bitrate.
pub fn is_lossless_file(suffix: &str, bit_rate: i32) -> bool {
    LOSSLESS_SUFFIXES.iter().any(|s| eq_ignore_case(s, suffix))
        || (eq_ignore_case(suffix, "m4a") && bit_rate > 500)
}
