//! The GIF picker's data, from Discord's own GIF endpoints (Tenor behind them).

use serde::Deserialize;

use crate::model::lenient_vec;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Gif {
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

/// The query part every GIF request carries, as Discord's client sends it.
pub fn params(locale: &str) -> String {
    format!("provider=tenor&locale={locale}&media_format=mp4")
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
        assert_eq!(params("en-US"), "provider=tenor&locale=en-US&media_format=mp4");
    }
}
