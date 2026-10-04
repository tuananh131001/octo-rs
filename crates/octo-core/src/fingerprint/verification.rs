//! The data half of `Services/Fingerprint/DownloadVerificationService.cs`.

// STUB(wave 2 fingerprint): replaced when the fingerprint port lands. `Song` holds a
// `VerificationResult` (never serialised), so the type has to exist; its fields come with the port.

/// What AcoustID said about a downloaded file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerificationResult {}
