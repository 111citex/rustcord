//! What a message carries besides its text: attachments, embeds, stickers and
//! Components V2 layouts. Decoding is lenient like the rest of the model:
//! unknown fields are ignored, missing ones default, and a component of a
//! type we don't know becomes [`Component::Unknown`] instead of an error.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::Id;
use crate::model::lenient_vec;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub id: Id,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub size: u64,
    /// A voice message's length, and its loudness as base64 bytes.
    #[serde(default)]
    pub duration_secs: Option<f32>,
    #[serde(default)]
    pub waveform: Option<String>,
}

impl Attachment {
    /// A voice message's loudness over time, 0 to 255.
    pub fn waveform_levels(&self) -> Vec<u8> {
        use base64::Engine;
        self.waveform.as_deref().and_then(|w| base64::engine::general_purpose::STANDARD.decode(w).ok()).unwrap_or_default()
    }

    /// Drawn inline as a picture (as opposed to a file card).
    pub fn is_video(&self) -> bool {
        match &self.content_type {
            Some(t) => t.starts_with("video/"),
            None => {
                let name = self.filename.to_ascii_lowercase();
                [".mp4", ".webm", ".mov", ".mkv"].iter().any(|e| name.ends_with(e))
            }
        }
    }

    pub fn is_image(&self) -> bool {
        match &self.content_type {
            Some(t) => t.starts_with("image/"),
            None => {
                let name = self.filename.to_ascii_lowercase();
                [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| name.ends_with(e))
            }
        }
    }

    /// Pixel size, when Discord told us (it does for images and video).
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height).filter(|(w, h)| *w > 0 && *h > 0)
    }

    /// The proxied copy when there is one.
    pub fn display_url(&self) -> &str {
        if self.proxy_url.is_empty() { &self.url } else { &self.proxy_url }
    }
}

/// An image, thumbnail or video reference inside an embed.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct EmbedMedia {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl EmbedMedia {
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height).filter(|(w, h)| *w > 0 && *h > 0)
    }

    /// The proxied copy loads without exposing the viewer to the origin.
    pub fn display_url(&self) -> &str {
        self.proxy_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(&self.url)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct EmbedAuthor {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct EmbedFooter {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct EmbedProvider {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct EmbedField {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Embed {
    /// `rich`, `image`, `video`, `gifv`, `article`, `link`…
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// 0xRRGGBB.
    #[serde(default)]
    pub color: Option<u32>,
    /// ISO 8601.
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub author: Option<EmbedAuthor>,
    #[serde(default)]
    pub footer: Option<EmbedFooter>,
    #[serde(default)]
    pub image: Option<EmbedMedia>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMedia>,
    #[serde(default)]
    pub video: Option<EmbedMedia>,
    #[serde(default)]
    pub provider: Option<EmbedProvider>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub fields: Vec<EmbedField>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct StickerItem {
    #[serde(default)]
    pub id: Id,
    #[serde(default)]
    pub name: String,
    /// 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    #[serde(default)]
    pub format_type: u8,
}

/// What a reply points at.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MessageReference {
    #[serde(default)]
    pub message_id: Option<Id>,
    #[serde(default)]
    pub channel_id: Option<Id>,
    #[serde(default)]
    pub guild_id: Option<Id>,
}

/// A file or image reference inside a Components V2 layout.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ComponentMedia {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl ComponentMedia {
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height).filter(|(w, h)| *w > 0 && *h > 0)
    }

    pub fn display_url(&self) -> &str {
        self.proxy_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(&self.url)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ComponentEmoji {
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub animated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Button {
    #[serde(default)]
    pub label: Option<String>,
    /// 1 primary, 2 secondary, 3 success, 4 danger, 5 link, 6 premium.
    #[serde(default)]
    pub style: u8,
    #[serde(default)]
    pub emoji: Option<ComponentEmoji>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SelectOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub emoji: Option<ComponentEmoji>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Select {
    /// The component type: 3 string, 5 user, 6 role, 7 mentionable, 8 channel.
    #[serde(default, rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub options: Vec<SelectOption>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct GalleryItem {
    #[serde(default)]
    pub media: ComponentMedia,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub spoiler: bool,
}

/// A Components V2 / action-row component, decoded by its `type`.
#[derive(Clone, Debug, PartialEq)]
pub enum Component {
    ActionRow(Vec<Component>),
    Button(Button),
    Select(Select),
    Section { components: Vec<Component>, accessory: Option<Box<Component>> },
    TextDisplay(String),
    Thumbnail { media: ComponentMedia, description: Option<String> },
    MediaGallery(Vec<GalleryItem>),
    File { file: ComponentMedia, name: Option<String>, size: Option<u64> },
    Separator { divider: bool, spacing: u8 },
    Container { components: Vec<Component>, accent_color: Option<u32> },
    Unknown(u8),
}

#[derive(Deserialize)]
struct RawComponent {
    #[serde(default, rename = "type")]
    kind: u8,
    #[serde(default, deserialize_with = "lenient_vec")]
    components: Vec<Component>,
    #[serde(default)]
    accessory: Option<Value>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    media: Option<ComponentMedia>,
    #[serde(default)]
    file: Option<ComponentMedia>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, deserialize_with = "lenient_vec")]
    items: Vec<GalleryItem>,
    #[serde(default = "yes")]
    divider: bool,
    #[serde(default)]
    spacing: Option<u8>,
    #[serde(default)]
    accent_color: Option<u32>,
}

fn yes() -> bool {
    true
}

impl<'de> Deserialize<'de> for Component {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let raw: RawComponent = serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
        Ok(match raw.kind {
            1 => Component::ActionRow(raw.components),
            2 => Component::Button(serde_json::from_value(value).map_err(serde::de::Error::custom)?),
            3 | 5..=8 => Component::Select(serde_json::from_value(value).map_err(serde::de::Error::custom)?),
            9 => Component::Section {
                components: raw.components,
                accessory: raw.accessory.and_then(|v| serde_json::from_value(v).ok()).map(Box::new),
            },
            10 => Component::TextDisplay(raw.content),
            11 => Component::Thumbnail { media: raw.media.unwrap_or_default(), description: raw.description },
            12 => Component::MediaGallery(raw.items),
            13 => Component::File { file: raw.file.unwrap_or_default(), name: raw.name, size: raw.size },
            14 => Component::Separator { divider: raw.divider, spacing: raw.spacing.unwrap_or(1) },
            17 => Component::Container { components: raw.components, accent_color: raw.accent_color },
            other => Component::Unknown(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn component(v: Value) -> Component {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn attachment_with_image_metadata() {
        let a: Attachment = serde_json::from_value(json!({
            "id": "1", "filename": "cat.png", "size": 2048, "url": "https://cdn.discordapp.com/a/cat.png",
            "proxy_url": "https://media.discordapp.net/a/cat.png", "width": 640, "height": 480,
            "content_type": "image/png", "placeholder": "xyz", "ephemeral": false
        }))
        .unwrap();
        assert_eq!(a.filename, "cat.png");
        assert_eq!(a.dimensions(), Some((640, 480)));
        assert!(a.is_image());
        assert_eq!(a.display_url(), "https://media.discordapp.net/a/cat.png");
    }

    #[test]
    fn attachment_without_content_type_falls_back_to_extension() {
        let a: Attachment = serde_json::from_value(json!({"id": "1", "filename": "A.JPG"})).unwrap();
        assert!(a.is_image());
        assert_eq!(a.dimensions(), None);
        let b: Attachment = serde_json::from_value(json!({"id": "2", "filename": "notes.txt", "content_type": "text/plain"})).unwrap();
        assert!(!b.is_image());
    }

    #[test]
    fn rich_embed() {
        let e: Embed = serde_json::from_value(json!({
            "type": "rich", "title": "Build", "description": "**ok**", "url": "https://ci", "color": 5763719,
            "timestamp": "2026-09-30T10:00:00.000000+00:00",
            "author": {"name": "CI", "icon_url": "https://i/a.png", "proxy_icon_url": "https://p/a.png"},
            "footer": {"text": "run 4"},
            "image": {"url": "https://i/x.png", "proxy_url": "https://p/x.png", "width": 800, "height": 400, "flags": 0},
            "thumbnail": {"url": "https://i/t.png"},
            "provider": {"name": "Hub"},
            "fields": [{"name": "a", "value": "1", "inline": true}, {"name": "b", "value": "2"}, 7]
        }))
        .unwrap();
        assert_eq!(e.color, Some(0x57F287));
        assert_eq!(e.author.as_ref().unwrap().name, "CI");
        assert_eq!(e.image.as_ref().unwrap().dimensions(), Some((800, 400)));
        assert_eq!(e.image.as_ref().unwrap().display_url(), "https://p/x.png");
        assert_eq!(e.thumbnail.as_ref().unwrap().display_url(), "https://i/t.png");
        // The malformed field is skipped, the rest kept.
        assert_eq!(e.fields.len(), 2);
        assert!(e.fields[0].inline && !e.fields[1].inline);
    }

    #[test]
    fn bare_link_embed() {
        let e: Embed = serde_json::from_value(json!({"type": "gifv", "url": "https://tenor.com/x", "video": {"url": "https://v/x.mp4", "width": 10, "height": 10}})).unwrap();
        assert_eq!(e.kind.as_deref(), Some("gifv"));
        assert!(e.video.is_some());
        assert!(e.fields.is_empty());
    }

    #[test]
    fn buttons_and_selects() {
        let row = component(json!({"type": 1, "id": 1, "components": [
            {"type": 2, "style": 5, "label": "Docs", "url": "https://x", "emoji": {"name": "📖"}},
            {"type": 2, "style": 1, "label": "Go", "custom_id": "go", "disabled": true},
            {"type": 3, "custom_id": "s", "placeholder": "Pick", "options": [{"label": "A", "value": "a", "description": "first"}, {"label": "B", "value": "b"}]},
            {"type": 5, "custom_id": "u"}
        ]}));
        let Component::ActionRow(items) = row else { panic!() };
        assert_eq!(items.len(), 4);
        let Component::Button(link) = &items[0] else { panic!() };
        assert_eq!((link.style, link.url.as_deref(), link.label.as_deref()), (5, Some("https://x"), Some("Docs")));
        assert_eq!(link.emoji.as_ref().unwrap().name.as_deref(), Some("📖"));
        let Component::Button(go) = &items[1] else { panic!() };
        assert!(go.disabled);
        let Component::Select(menu) = &items[2] else { panic!() };
        assert_eq!((menu.kind, menu.placeholder.as_deref(), menu.options.len()), (3, Some("Pick"), 2));
        let Component::Select(users) = &items[3] else { panic!() };
        assert_eq!(users.kind, 5);
    }

    #[test]
    fn components_v2_layout() {
        let c = component(json!({"type": 17, "accent_color": 16711680, "components": [
            {"type": 10, "content": "# Title"},
            {"type": 14, "divider": true, "spacing": 2},
            {"type": 9, "components": [{"type": 10, "content": "side"}],
             "accessory": {"type": 11, "media": {"url": "https://t/x.png", "width": 64, "height": 64}, "description": "alt"}},
            {"type": 12, "items": [{"media": {"url": "https://g/1.png"}, "description": "one", "spoiler": true}]},
            {"type": 13, "file": {"url": "attachment://a.zip"}, "name": "a.zip", "size": 10},
            {"type": 99}
        ]}));
        let Component::Container { components, accent_color } = c else { panic!() };
        assert_eq!(accent_color, Some(0xFF0000));
        assert_eq!(components[0], Component::TextDisplay("# Title".into()));
        assert_eq!(components[1], Component::Separator { divider: true, spacing: 2 });
        let Component::Section { components: inner, accessory } = &components[2] else { panic!() };
        assert_eq!(inner.len(), 1);
        assert!(matches!(accessory.as_deref(), Some(Component::Thumbnail { .. })));
        let Component::MediaGallery(items) = &components[3] else { panic!() };
        assert!(items[0].spoiler);
        assert!(matches!(&components[4], Component::File { name: Some(n), .. } if n == "a.zip"));
        assert_eq!(components[5], Component::Unknown(99));
    }

    #[test]
    fn separator_defaults() {
        assert_eq!(component(json!({"type": 14})), Component::Separator { divider: true, spacing: 1 });
    }

    #[test]
    fn sticker_and_reference() {
        let s: StickerItem = serde_json::from_value(json!({"id": "5", "name": "wave", "format_type": 1})).unwrap();
        assert_eq!((s.id, s.format_type), (Id(5), 1));
        let r: MessageReference = serde_json::from_value(json!({"message_id": "9", "channel_id": "8", "type": 0})).unwrap();
        assert_eq!(r.message_id, Some(Id(9)));
    }
}
