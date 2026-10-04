//! List covers and cover image files (C# `Services/CoverArt`): the design shared with the Octo
//! apps, the painted backgrounds and the veil under the words, the type, and the service that
//! serves list covers, the placeholder and the Octo badge.

pub mod cover_art_service;
pub mod cover_backgrounds;
pub mod cover_book;
pub mod cover_colours;
pub mod cover_files;
pub mod cover_fonts;
pub mod cover_image;
pub mod cover_layout;
pub mod cover_painter;
pub mod cover_veil;
mod design;
mod text;

pub use cover_art_service::{CoverArtService, CoverSeed, ListCover, list_kinds};
pub use cover_book::CoverBook;
pub use cover_colours::{CoverMusic, Lch, Swatch};
pub use cover_fonts::CoverTypesetter;
pub use cover_layout::{
    CoverAlign, CoverScript, CoverSpec, CoverType, CoverWords, ICoverTypesetter, Measured, WordsRole,
};
pub use cover_painter::CoverArt;

#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod test_support;
