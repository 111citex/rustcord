//! The GIF picker's data, from Discord's own GIF endpoints. Klipy is behind them now
//! (Tenor before it): Discord's client names no provider, it asks `/gifs/trending`,
//! `/gifs/trending-gifs` and `/gifs/search` with a locale, a media format and a limit.

use serde::Deserialize;

use crate::model::lenient_vec;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Gif {
    #[serde(default)]
    pub id: String,
    /// The page address: what a message sends.
    #[serde(default)]
    pub url: String,
    /// The video.
    #[serde(default)]
    pub src: String,
    /// The same as a GIF picture, which moves wherever it's drawn.
    #[serde(default)]
    pub gif_src: String,
    /// A still of it, light enough to fill a grid with.
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// A trending search, shown as a tile that searches for it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Category {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub src: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Trending {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub categories: Vec<Category>,
    /// The first trending GIF, for the "Trending GIFs" tile.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub gifs: Vec<Gif>,
}

/// The format Discord's own client on Linux asks for: a small animated WebP, which
/// draws as a moving picture with no video player behind it.
pub const MEDIA_FORMAT: &str = "tinywebp";

/// How many GIFs a search or the trending list asks for at once.
pub const PAGE: u32 = 50;

/// The query part every GIF request carries, as Discord's client sends it.
pub fn params(locale: &str) -> String {
    format!("media_format={MEDIA_FORMAT}&locale={locale}")
}

/// The paths Discord's client asks, query and all.
pub fn trending_path(locale: &str) -> String {
    format!("/gifs/trending?{}", params(locale))
}

pub fn trending_gifs_path(locale: &str) -> String {
    format!("/gifs/trending-gifs?{}&limit={PAGE}", params(locale))
}

pub fn search_path(query: &str, locale: &str) -> String {
    let q: String = query.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    format!("/gifs/search?q={q}&{}&limit={PAGE}", params(locale))
}

/// `//host/path` as `https://host/path`: Discord keeps GIF addresses without their scheme.
pub fn absolute(url: &str) -> String {
    match url.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

/// What kind of file an address is, by its ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A picture: GIF, WebP and the like, moving or not.
    Picture,
    Video,
    Unknown,
}

pub fn kind(url: &str) -> Kind {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    if [".gif", ".webp", ".avif", ".png", ".jpg", ".jpeg"].iter().any(|end| path.ends_with(end)) {
        Kind::Picture
    } else if [".mp4", ".webm"].iter().any(|end| path.ends_with(end)) {
        Kind::Video
    } else {
        Kind::Unknown
    }
}

/// The address behind one of Discord's media proxy addresses,
/// `https://media.discordapp.net/external/<signature>/https/host/path`; others as they are.
pub fn unproxied(url: &str) -> String {
    let url = absolute(url);
    let Some(rest) = url.strip_prefix("https://media.discordapp.net/external/").or_else(|| url.strip_prefix("https://images-ext-1.discordapp.net/external/")) else {
        return url;
    };
    let Some((_, inner)) = rest.split_once('/') else { return url };
    match inner.split_once('/') {
        Some((scheme @ ("https" | "http"), rest)) => format!("{scheme}://{rest}"),
        _ => url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_trending_and_skips_broken_entries() {
        let trending: Trending = serde_json::from_value(serde_json::json!({
            "categories": [{"name": "happy", "src": "https://x/happy.mp4"}, 5],
            "gifs": [{"url": "https://tenor.com/view/a", "src": "https://x/a.mp4", "width": 498, "height": 280}]
        }))
        .unwrap();
        assert_eq!(trending.categories.len(), 1);
        assert_eq!(trending.gifs[0].width, 498);
    }

    #[test]
    fn asks_the_way_discords_client_does() {
        assert_eq!(trending_path("en-US"), "/gifs/trending?media_format=tinywebp&locale=en-US");
        assert_eq!(trending_gifs_path("en-US"), "/gifs/trending-gifs?media_format=tinywebp&locale=en-US&limit=50");
        assert_eq!(search_path("cat dance", "en-US"), "/gifs/search?q=cat%20dance&media_format=tinywebp&locale=en-US&limit=50");
        assert!(!params("en-US").contains("provider"), "Discord names no provider any more");
    }

    #[test]
    fn addresses_get_their_scheme_and_kind() {
        assert_eq!(absolute("//static.example.com/a.webp"), "https://static.example.com/a.webp");
        assert_eq!(absolute("https://x/a.gif"), "https://x/a.gif");
        assert_eq!(kind("https://x/a.webp?size=2"), Kind::Picture);
        assert_eq!(kind("https://x/A.MP4"), Kind::Video);
        assert_eq!(kind("https://x/a.webm"), Kind::Video);
        assert_eq!(kind("https://x/view/a"), Kind::Unknown);
    }

    #[test]
    fn the_media_proxy_gives_up_what_it_wraps() {
        assert_eq!(unproxied("https://media.discordapp.net/external/AbC_1/https/media.example.com/x/cat.mp4"), "https://media.example.com/x/cat.mp4");
        assert_eq!(unproxied("//media.discordapp.net/external/sig/https/h/p.gif"), "https://h/p.gif");
        assert_eq!(unproxied("https://other.example.com/a.gif"), "https://other.example.com/a.gif");
    }
}
