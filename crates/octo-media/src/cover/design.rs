//! The list cover design, shared with the Octo apps, with its fonts and painted backgrounds,
//! built into the binary as the csproj embedded them (`Octo.CoverDesign.*` resources).
//!
//! The files are read from the C# tree for now; the cutover commit moves `Design/` beside this
//! crate and these paths with it. `Fonts/OFL.txt` (Inter's licence) ships as a file
//! (`licenses/Inter-OFL.txt`), not inside the binary.

/// Where the design lives until the cutover moves it.
macro_rules! design_path {
    ($file:expr) => {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../octo/Services/CoverArt/Design/",
            $file
        )
    };
}

pub(crate) const COVER_DESIGN_JSON: &str = include_str!(design_path!("cover-design.json"));
pub(crate) const LIST_HUES_JSON: &str = include_str!(design_path!("list-hues.json"));
pub(crate) const BACKGROUNDS_JSON: &str = include_str!(design_path!("Backgrounds/backgrounds.json"));

/// The three weights of Inter Display the design names, by file name.
pub(crate) const FONTS: &[(&str, &[u8])] = &[
    (
        "InterDisplay-Light.ttf",
        include_bytes!(design_path!("Fonts/InterDisplay-Light.ttf")),
    ),
    (
        "InterDisplay-Medium.ttf",
        include_bytes!(design_path!("Fonts/InterDisplay-Medium.ttf")),
    ),
    (
        "InterDisplay-SemiBold.ttf",
        include_bytes!(design_path!("Fonts/InterDisplay-SemiBold.ttf")),
    ),
];

macro_rules! backgrounds {
    ($($file:literal),* $(,)?) => {
        &[$(($file, include_bytes!(design_path!(concat!("Backgrounds/", $file))) as &[u8])),*]
    };
}

/// Every painted background, by file name.
pub(crate) const BACKGROUNDS: &[(&str, &[u8])] = backgrounds![
    "afterglow.webp",
    "amber-night.webp",
    "aurora.webp",
    "blue-hour.webp",
    "bubblegum.webp",
    "chiffon.webp",
    "citrus.webp",
    "clear-sky.webp",
    "cobalt.webp",
    "coral.webp",
    "crimson.webp",
    "dusk.webp",
    "ember.webp",
    "fern.webp",
    "firewave.webp",
    "garnet.webp",
    "honey.webp",
    "iris.webp",
    "jungle.webp",
    "lagoon.webp",
    "lemonade.webp",
    "lilac.webp",
    "limelight.webp",
    "magenta.webp",
    "magma.webp",
    "meadow.webp",
    "mercury.webp",
    "midnight.webp",
    "mint.webp",
    "neon-tide.webp",
    "nightshade.webp",
    "night-swim.webp",
    "northern-lights.webp",
    "opal.webp",
    "orchid.webp",
    "peach.webp",
    "periwinkle.webp",
    "raspberry.webp",
    "reef.webp",
    "rosewood.webp",
    "sea-glass.webp",
    "slate.webp",
    "sunset.webp",
    "tangerine.webp",
    "teal-drift.webp",
    "tide.webp",
    "tropic.webp",
    "velvet.webp",
];

/// An embedded font by file name.
pub(crate) fn font(file: &str) -> Option<&'static [u8]> {
    FONTS
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, bytes)| *bytes)
}

/// An embedded background by file name.
pub(crate) fn background(file: &str) -> Option<&'static [u8]> {
    BACKGROUNDS
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, bytes)| *bytes)
}
