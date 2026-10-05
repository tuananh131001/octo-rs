//! What makes a cover usable (#51): decodable, big enough, and square.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};

/// JPEG rounding and one-pixel crops leave real covers a few pixels off square.
const SQUARE_TOLERANCE: f64 = 0.03;
const MIN_SIDE: u32 = 150;

pub fn measure(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Decodable and big enough, and square when `require_square`. A cover that is not square is a
/// video thumbnail: a 16:9 frame with the artist off-centre, which in a grid of square covers is
/// the one that looks broken.
pub fn is_usable(bytes: Option<&[u8]>, require_square: bool) -> bool {
    let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
        return false;
    };
    let Some((width, height)) = measure(bytes) else {
        return false;
    };
    if width.min(height) < MIN_SIDE {
        return false;
    }
    !require_square || (width as f64 - height as f64).abs() <= width.max(height) as f64 * SQUARE_TOLERANCE
}

pub(crate) fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}

/// A picture as a JPEG at `quality`, with no chroma subsampling (the only kind the `image`
/// encoder writes).
pub(crate) fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, quality)
        .encode_image(&image.to_rgb8())
        .ok()?;
    Some(output)
}

/// The centre square. A YouTube "Topic" upload letterboxes the real cover inside a 16:9 frame,
/// so for those this IS the cover; for any other video it is the middle of the picture, which
/// still beats a stretched thumbnail.
pub fn crop_to_square(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = decode(bytes)?;
    let side = image.width().min(image.height());
    let x = (image.width() - side) / 2;
    let y = (image.height() - side) / 2;
    encode_jpeg(&image.crop_imm(x, y, side, side), 90)
}

/// JPEG as-is, anything else re-encoded, for a cover.jpg that is what its name says.
pub fn to_jpeg(bytes: &[u8]) -> Vec<u8> {
    if image::guess_format(bytes).ok() == Some(ImageFormat::Jpeg) {
        return bytes.to_vec();
    }
    decode(bytes)
        .and_then(|image| encode_jpeg(&image, 90))
        .unwrap_or_else(|| bytes.to_vec())
}

/// No larger than `max_side` on its longer side, as a JPEG. The cover kept in every file of an
/// album is its copy of the art, so a 3000 px master embedded in fifteen tracks would add tens
/// of megabytes; the full master goes to cover.jpg instead. Returned as it is when it already
/// fits or cannot be read.
pub fn fit_within(bytes: &[u8], max_side: u32) -> Vec<u8> {
    let Some((width, height)) = measure(bytes) else {
        return bytes.to_vec();
    };
    if width.max(height) <= max_side {
        return bytes.to_vec();
    }
    let Some(image) = decode(bytes) else {
        return bytes.to_vec();
    };
    // ImageSharp's ResizeMode.Max: the longer side to max_side, the other in proportion.
    let (w, h) = if width >= height {
        (
            max_side,
            ((height as f64 * max_side as f64 / width as f64).round() as u32).max(1),
        )
    } else {
        (
            ((width as f64 * max_side as f64 / height as f64).round() as u32).max(1),
            max_side,
        )
    };
    let resized = image.resize_exact(w, h, FilterType::Lanczos3);
    encode_jpeg(&resized, 92).unwrap_or_else(|| bytes.to_vec())
}

/// A 64-bit fingerprint of what a picture looks like (a difference hash: the picture shrunk to
/// 9 by 8 greys, one bit per neighbour pair, set when the left one is darker). The same artwork
/// at another size or compression comes out within a few bits; a different cover about half of
/// them apart. sacad checks covers the same way (a block hash, 8 bits of 64). None when the
/// picture cannot be read.
pub fn looks_hash(bytes: Option<&[u8]>) -> Option<u64> {
    let bytes = bytes.filter(|b| !b.is_empty())?;
    let grey = decode(bytes)?.to_luma8();
    // ImageSharp's default resampler is bicubic (Catmull-Rom).
    let small = image::imageops::resize(&grey, 9, 8, FilterType::CatmullRom);
    let mut hash = 0u64;
    let mut bit = 0;
    for y in 0..8 {
        for x in 0..8 {
            if small.get_pixel(x, y).0[0] < small.get_pixel(x + 1, y).0[0] {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    Some(hash)
}

/// Bits two fingerprints may differ by and still be the same artwork.
pub(crate) const LIKENESS_TOLERANCE: u32 = 10;

pub fn look_alike(a: u64, b: u64) -> bool {
    (a ^ b).count_ones() <= LIKENESS_TOLERANCE
}

const OCTO_MARK: &[u8] = b"Written by Octo";

/// The JPEG with a comment saying Octo wrote it, so a later, sharper cover may replace a
/// cover.jpg that is Octo's own and never one the owner put there. The comment goes after the
/// APPn segments, where JFIF readers expect them to stay first. Anything that is not a JPEG
/// comes back unchanged.
pub fn mark_as_octo(jpeg: &[u8]) -> Vec<u8> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 || is_octo_cover(jpeg) {
        return jpeg.to_vec();
    }
    let mut at = 2usize;
    while at + 4 <= jpeg.len() && jpeg[at] == 0xFF && (0xE0..=0xEF).contains(&jpeg[at + 1]) {
        at += 2 + ((jpeg[at + 2] as usize) << 8 | jpeg[at + 3] as usize);
    }
    if at > jpeg.len() {
        return jpeg.to_vec();
    }
    let length = OCTO_MARK.len() + 2;
    let mut marked = Vec::with_capacity(jpeg.len() + 2 + length);
    marked.extend_from_slice(&jpeg[..at]);
    marked.extend_from_slice(&[0xFF, 0xFE, (length >> 8) as u8, length as u8]);
    marked.extend_from_slice(OCTO_MARK);
    marked.extend_from_slice(&jpeg[at..]);
    marked
}

/// True when the JPEG carries the comment [`mark_as_octo`] writes.
pub fn is_octo_cover(jpeg: &[u8]) -> bool {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return false;
    }
    let mut at = 2usize;
    // Only the header segments; the picture itself starts at SOS.
    while at + 4 <= jpeg.len() && jpeg[at] == 0xFF && jpeg[at + 1] != 0xDA && jpeg[at + 1] != 0xD9 {
        let length = (jpeg[at + 2] as usize) << 8 | jpeg[at + 3] as usize;
        if length < 2 {
            return false;
        }
        if jpeg[at + 1] == 0xFE
            && length - 2 == OCTO_MARK.len()
            && at + 4 + OCTO_MARK.len() <= jpeg.len()
            && &jpeg[at + 4..at + 4 + OCTO_MARK.len()] == OCTO_MARK
        {
            return true;
        }
        at += 2 + length;
    }
    false
}

pub fn mime_type(bytes: &[u8]) -> &'static str {
    image::guess_format(bytes)
        .map(|format| format.to_mime_type())
        .unwrap_or("image/jpeg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        encode_jpeg(&DynamicImage::new_rgb8(width, height), 90).expect("encodes")
    }

    #[test]
    fn marked_covers_are_recognised_and_marked_once() {
        let plain = jpeg(200, 200);
        assert!(!is_octo_cover(&plain));
        let marked = mark_as_octo(&plain);
        assert!(is_octo_cover(&marked));
        assert_eq!(mark_as_octo(&marked), marked);
        assert_eq!(measure(&marked), Some((200, 200)));
        assert_eq!(mark_as_octo(b"not a jpeg"), b"not a jpeg");
    }

    #[test]
    fn usable_covers_are_big_and_square() {
        assert!(is_usable(Some(&jpeg(200, 204)), true));
        assert!(!is_usable(Some(&jpeg(320, 180)), true));
        assert!(is_usable(Some(&jpeg(320, 180)), false));
        assert!(!is_usable(Some(&jpeg(100, 100)), false));
        assert!(!is_usable(None, false));
        assert_eq!(
            measure(&crop_to_square(&jpeg(320, 180)).expect("crops")),
            Some((180, 180))
        );
        assert_eq!(measure(&fit_within(&jpeg(400, 200), 100)), Some((100, 50)));
        assert_eq!(mime_type(&jpeg(10, 10)), "image/jpeg");
        let hash = looks_hash(Some(&jpeg(64, 64))).expect("hashes");
        assert!(look_alike(hash, hash));
    }
}
