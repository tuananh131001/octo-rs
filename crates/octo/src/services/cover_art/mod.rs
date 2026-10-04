//! Cover-art lookups (`Services/CoverArt`): the sources behind the aggregator, the iTunes
//! master lookup, the Cover Art Archive, and the cover upgrade with the album cover finder it
//! asks. List-cover rendering is `octo_media::cover`.

pub mod album_cover_finder;
pub mod cover_art_aggregator;
pub mod cover_art_archive_lookup;
pub mod cover_upgrade;
pub mod deezer_cover_art_lookup;
pub mod i_cover_art_source;
pub mod itunes_cover_art_lookup;
pub mod last_fm_cover_art_lookup;

pub use album_cover_finder::{AlbumCoverFinder, AlbumCoverQuery, FoundCover, IAlbumCoverFinder};
pub use cover_art_aggregator::CoverArtAggregator;
pub use cover_art_archive_lookup::CoverArtArchiveLookup;
pub use cover_upgrade::{
    CoverUpgradeChange, CoverUpgradeItem, CoverUpgradeJournal, CoverUpgradeJournalEntry, CoverUpgradeMode,
    CoverUpgradeRequest, CoverUpgradeRun, CoverUpgradeScope, CoverUpgradeServices, CoverUpgradeStatus,
    CoverUpgradeStore, CoverUpgradeWorker,
};
pub use deezer_cover_art_lookup::DeezerCoverArtLookup;
pub use i_cover_art_source::ICoverArtSource;
pub use itunes_cover_art_lookup::ITunesCoverArtLookup;
pub use last_fm_cover_art_lookup::LastFmCoverArtLookup;
