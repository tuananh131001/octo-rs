//! STUB(4-A): replaced when 4-A lands with the port of
//! `Services/Soulseek/SoulseekMetadataService.cs`. Only the short-id decoder is here, which the
//! acquisition worker (4-D) names a failed download by.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use octo_core::common::dotnet;
use octo_core::soulseek::SoulseekRouting;

pub struct SoulseekMetadataService;

impl SoulseekMetadataService {
    // ====== Short opaque ID format ======
    // Pipe-delimited fields, base64url where needed.
    //   yt|{videoId}|{artistB64}|{titleB64}|{durationSec}
    pub fn try_decode_external_id(external_id: Option<&str>) -> Option<SoulseekRouting> {
        let external_id = external_id.filter(|id| !dotnet::is_blank(id))?;
        let parts: Vec<&str> = external_id.split('|').collect();
        if parts.len() < 4 || parts[0] != "yt" {
            return None;
        }
        let duration = parts.get(4).and_then(|d| d.trim().parse::<i32>().ok());
        Some(SoulseekRouting {
            you_tube_id: Some(parts[1].to_string()),
            artist: Some(b64_url_decode(parts[2])?),
            title: Some(b64_url_decode(parts[3])?),
            duration,
            ..Default::default()
        })
    }
}

fn b64_url_decode(s: &str) -> Option<String> {
    let mut s = s.replace('-', "+").replace('_', "/");
    match s.len() % 4 {
        2 => s.push_str("=="),
        3 => s.push('='),
        _ => {}
    }
    let bytes = STANDARD.decode(s).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
