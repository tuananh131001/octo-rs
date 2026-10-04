//! Rust-only: `TagPreview` over fake services. The C# exercised it only through the admin
//! endpoint (`AdminContractTests.TagPreview_*`, 6-B), which checks the request, not the preview.

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use super::*;
use crate::settings::AppSettings;
use crate::tagging::tag_evidence::FileFacts;

struct FakeFacts;

impl FileFactsReader for FakeFacts {
    fn read_facts(&self, path: &str, tags_are_evidence: bool) -> FileFacts {
        if path.is_empty() {
            return FileFacts::unknown("");
        }
        FileFacts {
            duration_seconds: 330,
            extension: ".flac".into(),
            title: Some("Teardrop".into()),
            artist: Some("Massive Attack".into()),
            album: Some("Mezzanine".into()),
            year: Some(1998),
            tags_are_evidence,
            ..Default::default()
        }
    }
}

struct FakeVerifier {
    enabled: bool,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl FingerprintVerifier for FakeVerifier {
    fn is_fingerprinting_enabled(&self) -> bool {
        self.enabled
    }

    async fn verify(&self, path: &str, artist: &str, title: &str) -> VerificationResult {
        self.asked.lock().push(format!("{path}|{artist}|{title}"));
        VerificationResult::default()
    }
}

struct FakeMeter(Option<MeasuredLoudness>, bool);

#[async_trait]
impl PreviewLoudnessMeter for FakeMeter {
    async fn measure(
        &self,
        _path: &str,
        timeout_seconds: i32,
        _ct: &CancellationToken,
    ) -> anyhow::Result<Option<MeasuredLoudness>> {
        assert_eq!(timeout_seconds, 45);
        if self.1 {
            anyhow::bail!("ffmpeg went away");
        }
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct FakeFiller(AtomicBool);

impl CatalogBlankFiller for FakeFiller {
    fn fill_blanks_from_catalog(&self, song: &mut Song, meta: Option<&FullTrackMeta>) {
        self.0.store(true, Ordering::SeqCst);
        assert!(meta.is_none());
        if song.genre.is_none() {
            song.genre = Some("filled".into());
        }
    }
}

fn preview(
    verifier: Option<Arc<FakeVerifier>>,
    meter: Option<FakeMeter>,
    filler: Arc<FakeFiller>,
) -> TagPreview {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let facts: Arc<dyn FileFactsReader> = Arc::new(FakeFacts);
    let identifier = Arc::new(ReleaseIdentifier::new(
        settings.clone(),
        facts.clone(),
        None,
        None,
    ));
    TagPreview::new(
        settings,
        identifier,
        facts,
        verifier.map(|v| v as Arc<dyn FingerprintVerifier>),
        meter.map(|m| Arc::new(m) as Arc<dyn PreviewLoudnessMeter>),
        filler,
    )
}

#[tokio::test]
async fn a_name_alone_is_matched_without_a_fingerprint_or_loudness() {
    let filler = Arc::new(FakeFiller::default());
    let preview = preview(None, None, filler.clone());

    let report = preview
        .preview(
            None,
            Some(" Massive Attack "),
            Some("Teardrop"),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("a report");

    assert_eq!(report.confidence, "None");
    assert_eq!(
        report.notes,
        ["matched by name alone: no file, so no fingerprint and no loudness"]
    );
    assert!(filler.0.load(Ordering::SeqCst));
    assert_eq!(report.integrated_lufs, None);
}

#[tokio::test]
async fn a_file_is_verified_measured_and_weighed_by_its_own_tags() {
    let verifier = Arc::new(FakeVerifier {
        enabled: true,
        asked: Mutex::new(Vec::new()),
    });
    let meter = FakeMeter(
        Some(MeasuredLoudness {
            integrated_lufs: -11.5,
            true_peak_dbfs: -0.3,
            replay_gain: Some(ReplayGainText {
                gain_text: "-6.50 dB".into(),
                peak_text: "0.966051".into(),
            }),
        }),
        false,
    );
    let preview = preview(
        Some(verifier.clone()),
        Some(meter),
        Arc::new(FakeFiller::default()),
    );

    let report = preview
        .preview(Some("/m/t.flac"), None, None, None, &CancellationToken::new())
        .await
        .expect("a report");

    // The file's own name and artist stand in for the request's.
    assert_eq!(
        verifier.asked.lock().clone(),
        ["/m/t.flac|Massive Attack|Teardrop"]
    );
    assert_eq!(report.integrated_lufs, Some(-11.5));
    assert_eq!(report.true_peak_dbfs, Some(-0.3));
    assert_eq!(
        report.fields["replayGain"],
        FieldDecision::new(Some("-6.50 dB, peak 0.966051"), Some("Measured"))
    );
    // The file's album is a candidate to weigh, not a request to keep.
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].source, "FileTags");
    assert_eq!(report.candidates[0].album, "Mezzanine");
    // A peer's tags alone never set the album-level fields.
    assert_eq!(
        report.notes,
        ["the album-level tags were not taken from 'Mezzanine' 1998: Medium from FileTags"]
    );
}

#[tokio::test]
async fn verification_off_is_noted_and_a_failed_measurement_leaves_no_loudness() {
    let verifier = Arc::new(FakeVerifier {
        enabled: false,
        asked: Mutex::new(Vec::new()),
    });
    let preview = preview(
        Some(verifier.clone()),
        Some(FakeMeter(None, true)),
        Arc::new(FakeFiller::default()),
    );

    let report = preview
        .preview(
            Some("/m/t.flac"),
            Some("Massive Attack"),
            Some("Teardrop"),
            Some("Mezzanine"),
            &CancellationToken::new(),
        )
        .await
        .expect("a report");

    assert!(verifier.asked.lock().is_empty());
    assert_eq!(
        report.notes[0],
        "download verification is off or has no key, so the fingerprint service was not asked"
    );
    assert_eq!(report.integrated_lufs, None);
    assert!(!report.fields.contains_key("replayGain"));
}
