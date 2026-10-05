//! TagLib#'s ID3v1 genre table (`TagLib.Genres.Audio`): the 148 names a numeric genre stands
//! for. It is TagLib#'s own spelling ("Classic Rock", "Jazz+Funk"), not lofty's table, which
//! spells 19 of them differently and runs to 192.

use super::net::parse_byte;

/// `TagLib.Genres.Audio`, in index order. "Fusion" is there twice (30 and 84), as in TagLib#.
pub(crate) const AUDIO: [&str; 148] = [
    "Blues",
    "Classic Rock",
    "Country",
    "Dance",
    "Disco",
    "Funk",
    "Grunge",
    "Hip-Hop",
    "Jazz",
    "Metal",
    "New Age",
    "Oldies",
    "Other",
    "Pop",
    "R&B",
    "Rap",
    "Reggae",
    "Rock",
    "Techno",
    "Industrial",
    "Alternative",
    "Ska",
    "Death Metal",
    "Pranks",
    "Soundtrack",
    "Euro-Techno",
    "Ambient",
    "Trip-Hop",
    "Vocal",
    "Jazz+Funk",
    "Fusion",
    "Trance",
    "Classical",
    "Instrumental",
    "Acid",
    "House",
    "Game",
    "Sound Clip",
    "Gospel",
    "Noise",
    "Alternative Rock",
    "Bass",
    "Soul",
    "Punk",
    "Space",
    "Meditative",
    "Instrumental Pop",
    "Instrumental Rock",
    "Ethnic",
    "Gothic",
    "Darkwave",
    "Techno-Industrial",
    "Electronic",
    "Pop-Folk",
    "Eurodance",
    "Dream",
    "Southern Rock",
    "Comedy",
    "Cult",
    "Gangsta",
    "Top 40",
    "Christian Rap",
    "Pop/Funk",
    "Jungle",
    "Native American",
    "Cabaret",
    "New Wave",
    "Psychedelic",
    "Rave",
    "Showtunes",
    "Trailer",
    "Lo-Fi",
    "Tribal",
    "Acid Punk",
    "Acid Jazz",
    "Polka",
    "Retro",
    "Musical",
    "Rock & Roll",
    "Hard Rock",
    "Folk",
    "Folk/Rock",
    "National Folk",
    "Swing",
    "Fusion",
    "Bebob",
    "Latin",
    "Revival",
    "Celtic",
    "Bluegrass",
    "Avantgarde",
    "Gothic Rock",
    "Progressive Rock",
    "Psychedelic Rock",
    "Symphonic Rock",
    "Slow Rock",
    "Big Band",
    "Chorus",
    "Easy Listening",
    "Acoustic",
    "Humour",
    "Speech",
    "Chanson",
    "Opera",
    "Chamber Music",
    "Sonata",
    "Symphony",
    "Booty Bass",
    "Primus",
    "Porn Groove",
    "Satire",
    "Slow Jam",
    "Club",
    "Tango",
    "Samba",
    "Folklore",
    "Ballad",
    "Power Ballad",
    "Rhythmic Soul",
    "Freestyle",
    "Duet",
    "Punk Rock",
    "Drum Solo",
    "A Cappella",
    "Euro-House",
    "Dance Hall",
    "Goa",
    "Drum & Bass",
    "Club-House",
    "Hardcore",
    "Terror",
    "Indie",
    "BritPop",
    "Negerpunk",
    "Polsk Punk",
    "Beat",
    "Christian Gangsta Rap",
    "Heavy Metal",
    "Black Metal",
    "Crossover",
    "Contemporary Christian",
    "Christian Rock",
    "Merengue",
    "Salsa",
    "Thrash Metal",
    "Anime",
    "Jpop",
    "Synthpop",
];

/// `Genres.AudioToIndex`: the index of a name spelled exactly as in the table, or 255.
pub(crate) fn audio_to_index(name: &str) -> u8 {
    AUDIO
        .iter()
        .position(|genre| *genre == name)
        .map_or(255, |index| index as u8)
}

/// `Genres.IndexToAudio(byte)`.
pub(crate) fn index_to_audio(index: u8) -> Option<&'static str> {
    AUDIO.get(index as usize).copied()
}

/// `Genres.IndexToAudio(string)`: the text is a number, alone or in parentheses ("(17)").
pub(crate) fn text_to_audio(text: &str) -> Option<&'static str> {
    index_to_audio(string_to_byte(text))
}

/// `Genres.StringToByte`: "(n)..." or a plain number, as `byte.TryParse` reads them; 255
/// otherwise.
fn string_to_byte(text: &str) -> u8 {
    if text.chars().count() > 2
        && text.starts_with('(')
        && let Some(close) = text.find(')')
        && let Some(value) = parse_byte(&text[1..close])
    {
        return value;
    }
    parse_byte(text).unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is TagLib#'s, as the C# generator wrote it out.
    #[test]
    fn the_table_is_taglibs() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/tags/csharp/genres.txt"
        );
        let text = std::fs::read_to_string(path).expect("genres.txt");
        let names: Vec<&str> = text.lines().collect();
        assert_eq!(names, AUDIO.to_vec());
    }

    #[test]
    fn names_and_numbers_map_as_taglib_maps_them() {
        assert_eq!(audio_to_index("Rock"), 17);
        assert_eq!(audio_to_index("rock"), 255);
        assert_eq!(audio_to_index("Trip Hop"), 255);
        assert_eq!(audio_to_index("Fusion"), 30);
        assert_eq!(text_to_audio("17"), Some("Rock"));
        assert_eq!(text_to_audio("(17)"), Some("Rock"));
        assert_eq!(text_to_audio(" 13 "), Some("Pop"));
        assert_eq!(text_to_audio("Shoegaze"), None);
        assert_eq!(text_to_audio("200"), None);
    }
}
