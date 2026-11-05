//! Desktop notifications, through the system's own `notify-send`. Silent: what
//! they sound like, if anything, is the notification daemon's business.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// What a notification says: plain text, kept short, and safe for the markup
/// notification daemons read their body as.
pub fn body_text(content: &str, has_attachment: bool) -> String {
    const MAX_CHARS: usize = 200;
    let text = content.trim();
    if text.is_empty() {
        return if has_attachment { "Sent an attachment".to_string() } else { String::new() };
    }
    let short: String = text.chars().take(MAX_CHARS).collect();
    let more = if text.chars().count() > MAX_CHARS { "\u{2026}" } else { "" };
    format!("{short}{more}").replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Where the picture from `url` is kept: notify-send wants a file.
pub fn picture_path(url: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    cord_core::paths::cache_dir().join("notification-pictures").join(format!("{:016x}.png", hasher.finish()))
}

/// Saves a downloaded picture; the path if that worked.
pub fn keep_picture(path: PathBuf, bytes: Vec<u8>) -> Option<PathBuf> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

pub fn show(title: &str, content: &str, has_attachment: bool, icon: Option<PathBuf>) {
    let (title, body) = (title.to_string(), body_text(content, has_attachment));
    std::thread::spawn(move || {
        let mut command = std::process::Command::new("notify-send");
        command.args(["-a", "Rustcord"]);
        if let Some(icon) = &icon {
            command.arg("-i").arg(icon);
        }
        let sent = command.args(["--", &title, &body]).status();
        if let Err(error) = sent {
            tracing::warn!(%error, "couldn't show a notification");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_picture_gets_its_own_file() {
        assert_eq!(picture_path("a"), picture_path("a"));
        assert_ne!(picture_path("a"), picture_path("b"));
    }

    #[test]
    fn bodies_are_short_and_escaped() {
        assert_eq!(body_text("a <b> & c", false), "a &lt;b&gt; &amp; c");
        assert_eq!(body_text("  ", true), "Sent an attachment");
        assert_eq!(body_text("", false), "");
        let long = body_text(&"x".repeat(300), false);
        assert_eq!(long.chars().count(), 201);
        assert!(long.ends_with('\u{2026}'));
    }
}
