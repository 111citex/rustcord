//! Discord CDN URLs. PNG everywhere: for animated assets it is the first
//! frame, which is what the client shows until hovered.

use crate::Id;

pub const CDN: &str = "https://cdn.discordapp.com";

pub fn avatar_url(user: Id, hash: Option<&str>, size: u32) -> String {
    match hash {
        Some(hash) => format!("{CDN}/avatars/{user}/{hash}.png?size={size}"),
        None => format!("{CDN}/embed/avatars/{}.png", (user.0 >> 22) % 6),
    }
}

/// A server-specific avatar, which replaces the global one inside that server.
pub fn guild_avatar_url(guild: Id, user: Id, hash: &str, size: u32) -> String {
    format!("{CDN}/guilds/{guild}/users/{user}/avatars/{hash}.png?size={size}")
}

/// A profile banner: the member's server banner when `guild` is given.
pub fn banner_url(user: Id, guild: Option<Id>, hash: &str, size: u32) -> String {
    let kind = if hash.starts_with("a_") { "gif" } else { "png" };
    match guild {
        Some(guild) => format!("{CDN}/guilds/{guild}/users/{user}/banners/{hash}.{kind}?size={size}"),
        None => format!("{CDN}/banners/{user}/{hash}.{kind}?size={size}"),
    }
}

/// A server's banner, animated if it is.
pub fn guild_banner_url(guild: Id, hash: &str, size: u32) -> String {
    let kind = if hash.starts_with("a_") { "gif" } else { "webp" };
    format!("{CDN}/banners/{guild}/{hash}.{kind}?size={size}")
}

pub fn guild_icon_url(guild: Id, hash: &str, size: u32) -> String {
    format!("{CDN}/icons/{guild}/{hash}.png?size={size}")
}

/// Hosts whose media proxy resizes and re-encodes on request.
const RESIZING_HOSTS: [&str; 2] = ["media.discordapp.net", "cdn.discordapp.com"];

/// Pictures decode to w*h*4 bytes whatever their file size, so ask Discord's
/// proxy for the size the picture is drawn at: `url` with `width` and
/// `height` of the logical `box_w` x `box_h` doubled for HiDPI screens and
/// `format=webp`, unless it is a GIF (which is left whole, to animate). Any
/// other host is returned untouched: we don't know what it understands.
/// The file behind a Discord media proxy URL, for when the proxy has no copy of it.
pub fn original_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    if let Some(path) = rest.strip_prefix("media.discordapp.net/") {
        return Some(format!("https://cdn.discordapp.com/{path}"));
    }
    // `images-ext-1.discordapp.net/external/<hash>/[%3Fquery/]https/host/path`
    let path = rest.strip_prefix("images-ext-1.discordapp.net/external/").or_else(|| rest.strip_prefix("images-ext-2.discordapp.net/external/"))?;
    let (_, path) = path.split_once('/')?;
    let path = path.split('?').next()?;
    let (query, path) = match path.strip_prefix("%3F") {
        Some(rest) => {
            let (query, path) = rest.split_once('/')?;
            (Some(query.replace("%3D", "=").replace("%26", "&")), path)
        }
        None => (None, path),
    };
    let (scheme, path) = path.split_once('/')?;
    if !matches!(scheme, "https" | "http") {
        return None;
    }
    Some(match query {
        Some(query) => format!("{scheme}://{path}?{query}"),
        None => format!("{scheme}://{path}"),
    })
}

pub fn fitted_url(url: &str, box_w: u32, box_h: u32) -> String {
    fitted_url_at(url, box_w, box_h, f32::from_bits(DEVICE_SCALE.load(std::sync::atomic::Ordering::Relaxed)))
}

/// The display's pixels per point, as `f32` bits.
static DEVICE_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);

pub fn device_scale() -> f32 {
    f32::from_bits(DEVICE_SCALE.load(std::sync::atomic::Ordering::Relaxed))
}

/// Pictures are asked for at the size they'll show on this display.
pub fn set_device_scale(scale: f32) {
    DEVICE_SCALE.store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// The best copy of a picture at `w` x `h` real pixels: for the ones that are looked at.
pub fn sharp_url(url: &str, w: u32, h: u32) -> String {
    let fitted = fitted_url_at(url, w, h, 1.);
    // A GIF isn't resized; other hosts have been left as they were.
    if fitted.contains("#fit=") || !fitted.contains("width=") {
        return fitted;
    }
    format!("{fitted}&quality=lossless")
}

fn fitted_url_at(url: &str, box_w: u32, box_h: u32, scale: f32) -> String {
    let Some(rest) = url.strip_prefix("https://") else { return url.to_string() };
    let rest = rest.split('#').next().unwrap_or(rest);
    let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host, path) = location.split_once('/').unwrap_or((location, ""));
    if !RESIZING_HOSTS.contains(&host) {
        return url.to_string();
    }
    let gif = path.to_ascii_lowercase().ends_with(".gif");
    // What we set replaces what was there; signatures (ex, is, hm) and the rest stay.
    let mut params: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| !matches!(p.split('=').next(), Some("width" | "height" | "format")))
        .map(str::to_string)
        .collect();
    // A GIF is fetched whole: resizing it can come back as one still frame.
    if !gif {
        params.push(format!("width={}", ((box_w as f32 * scale).ceil() as u32).max(1)));
        params.push(format!("height={}", ((box_h as f32 * scale).ceil() as u32).max(1)));
        params.push("format=webp".into());
    }
    let device = |points: u32| ((points as f32 * scale).ceil() as u32).max(1);
    // A GIF comes whole, and is scaled down as it's decoded.
    let hint = if gif { format!("#fit={}x{}", device(box_w), device(box_h)) } else { String::new() };
    format!("https://{host}/{path}?{}{hint}", params.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_with_hash() {
        assert_eq!(avatar_url(Id(100), Some("abc"), 64), "https://cdn.discordapp.com/avatars/100/abc.png?size=64");
        // Animated hashes still get .png: the first frame.
        assert_eq!(avatar_url(Id(100), Some("a_abc"), 64), "https://cdn.discordapp.com/avatars/100/a_abc.png?size=64");
    }

    #[test]
    fn default_avatar_from_id() {
        let id = Id(175928847299117063);
        let index = (id.0 >> 22) % 6;
        assert_eq!(avatar_url(id, None, 64), format!("https://cdn.discordapp.com/embed/avatars/{index}.png"));
    }

    #[test]
    fn server_avatar() {
        assert_eq!(
            guild_avatar_url(Id(300), Id(100), "a_x", 80),
            "https://cdn.discordapp.com/guilds/300/users/100/avatars/a_x.png?size=80"
        );
    }

    #[test]
    fn banners() {
        assert_eq!(banner_url(Id(100), None, "b", 480), "https://cdn.discordapp.com/banners/100/b.png?size=480");
        assert_eq!(
            banner_url(Id(100), Some(Id(300)), "b", 480),
            "https://cdn.discordapp.com/guilds/300/users/100/banners/b.png?size=480"
        );
    }

    #[test]
    fn guild_icon() {
        assert_eq!(guild_icon_url(Id(300), "ic0", 96), "https://cdn.discordapp.com/icons/300/ic0.png?size=96");
    }

    #[test]
    fn fitted_urls_ask_for_the_boxs_device_pixels_as_webp() {
        let fitted_url = |url, w, h| fitted_url_at(url, w, h, 2.);
        assert_eq!(
            fitted_url("https://media.discordapp.net/attachments/1/2/a.png", 400, 300),
            "https://media.discordapp.net/attachments/1/2/a.png?width=800&height=600&format=webp"
        );
        // Signed links keep their signature; our own parameters replace old ones.
        assert_eq!(
            fitted_url("https://cdn.discordapp.com/attachments/1/2/a.jpg?ex=1&is=2&hm=3&width=5&format=png", 80, 80),
            "https://cdn.discordapp.com/attachments/1/2/a.jpg?ex=1&is=2&hm=3&width=160&height=160&format=webp"
        );
        assert_eq!(
            fitted_url("https://media.discordapp.net/stickers/9.png?size=160", 160, 160),
            "https://media.discordapp.net/stickers/9.png?size=160&width=320&height=320&format=webp"
        );
    }

    #[test]
    fn sharp_urls_are_lossless_unless_they_cannot_be() {
        assert_eq!(
            sharp_url("https://media.discordapp.net/a.png", 800, 600),
            "https://media.discordapp.net/a.png?width=800&height=600&format=webp&quality=lossless"
        );
        assert!(!sharp_url("https://media.discordapp.net/a.gif", 800, 600).contains("lossless"));
        assert_eq!(sharp_url("https://example.com/a.png", 800, 600), "https://example.com/a.png");
    }

    #[test]
    fn fitted_gifs_stay_gifs() {
        assert_eq!(
            fitted_url_at("https://media.discordapp.net/attachments/1/2/a.GIF?ex=1", 100, 50, 2.),
            "https://media.discordapp.net/attachments/1/2/a.GIF?ex=1#fit=200x100"
        );
    }

    #[test]
    fn other_hosts_are_left_alone() {
        for url in [
            "https://example.com/a.png",
            "https://cdn.discordapp.com.evil.test/a.png",
            "http://media.discordapp.net/a.png",
            "attachment://a.png",
            "",
        ] {
            assert_eq!(fitted_url(url, 10, 10), url);
        }
    }
}

#[cfg(test)]
mod original_url_tests {
    use super::{Id, banner_url, guild_banner_url, original_url};

    #[test]
    fn animated_banners_are_gifs() {
        assert!(banner_url(Id(1), None, "a_x", 600).contains("/a_x.gif?size=600"));
        assert!(banner_url(Id(1), None, "x", 600).contains("/x.png?size=600"));
        assert!(guild_banner_url(Id(1), "a_x", 480).ends_with("/banners/1/a_x.gif?size=480"));
        assert!(guild_banner_url(Id(1), "x", 480).ends_with("/banners/1/x.webp?size=480"));
    }

    #[test]
    fn finds_the_file_behind_a_proxy() {
        assert_eq!(
            original_url("https://media.discordapp.net/attachments/1/2/a.png?ex=1&hm=2").as_deref(),
            Some("https://cdn.discordapp.com/attachments/1/2/a.png?ex=1&hm=2")
        );
        assert_eq!(
            original_url("https://images-ext-1.discordapp.net/external/abc/https/site.com/a.png?format=webp").as_deref(),
            Some("https://site.com/a.png")
        );
        assert_eq!(
            original_url("https://images-ext-1.discordapp.net/external/abc/%3Fsize%3D1024/https/cdn.discordapp.com/a.png").as_deref(),
            Some("https://cdn.discordapp.com/a.png?size=1024")
        );
        assert_eq!(original_url("https://cdn.discordapp.com/a.png"), None);
    }
}
