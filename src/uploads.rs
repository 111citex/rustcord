//! Files waiting in the message box: pasted or picked, shown above it, sent
//! with the message. A pasted picture only exists on the clipboard, so it's
//! written to the cache folder until then.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use cord_core::http::Upload;

/// Discord's limit on one message.
pub const MAX_FILES: usize = 10;

/// One file on its way, as (message, its place in the message).
struct Sending {
    sent: std::sync::Arc<std::sync::atomic::AtomicU64>,
    first_byte: Option<std::time::Instant>,
    failed: bool,
}

thread_local! {
    static SENDING: std::cell::RefCell<std::collections::HashMap<(cord_core::Id, u64), Sending>> = std::cell::RefCell::default();
}

/// Starts following a file of a message being sent.
pub fn track(message: cord_core::Id, index: u64, sent: std::sync::Arc<std::sync::atomic::AtomicU64>) {
    SENDING.with_borrow_mut(|s| s.insert((message, index), Sending { sent, first_byte: None, failed: false }));
}

/// The message's sending is over: forgotten if it went, marked if it didn't.
pub fn finish(message: cord_core::Id, ok: bool) {
    SENDING.with_borrow_mut(|s| {
        if ok {
            s.retain(|(m, _), _| *m != message);
        } else {
            s.iter_mut().filter(|((m, _), _)| *m == message).for_each(|(_, f)| f.failed = true);
        }
    });
}

/// What's happening with a file now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    Failed,
    /// Bytes sent, and seconds since the first went.
    Sending(u64, f32),
    /// Gone from here; Discord's yet to answer.
    Finishing,
}

pub fn progress(message: cord_core::Id, index: u64) -> Progress {
    SENDING.with_borrow_mut(|s| {
        let Some(file) = s.get_mut(&(message, index)) else { return Progress::Finishing };
        if file.failed {
            return Progress::Failed;
        }
        let sent = file.sent.load(std::sync::atomic::Ordering::Relaxed);
        if sent > 0 && file.first_byte.is_none() {
            file.first_byte = Some(std::time::Instant::now());
        }
        Progress::Sending(sent, file.first_byte.map_or(0., |t| t.elapsed().as_secs_f32()))
    })
}

pub fn megabytes(bytes: u64) -> String {
    let mb = bytes as f64 / (1024. * 1024.);
    if mb >= 1. { format!("{mb:.1} MB") } else { format!("{:.0} KB", bytes as f64 / 1024.) }
}

pub fn too_large(name: &str, size: u64, limit: u64) -> String {
    format!("{name} is {}. The limit here is {}.", megabytes(size), megabytes(limit))
}

/// "42%  •  1.2 MB/s  •  8s left  •  3.1 MB of 7.4 MB"
pub fn status_line(sent: u64, total: u64, secs: f32) -> String {
    let size = format!("{} of {}", megabytes(sent), megabytes(total));
    if sent == 0 {
        return format!("Starting  \u{2022}  {}", megabytes(total));
    }
    if sent >= total {
        return format!("Finishing  \u{2022}  {}", megabytes(total));
    }
    let percent = sent * 100 / total.max(1);
    if secs < 0.25 {
        return format!("{percent}%  \u{2022}  {size}");
    }
    let speed = sent as f32 / secs;
    let left = (total - sent) as f32 / speed.max(1.);
    let time = if left >= 60. { format!("{}m {}s left", left as u32 / 60, left as u32 % 60) } else { format!("{}s left", left.ceil() as u32) };
    format!("{percent}%  \u{2022}  {}/s  \u{2022}  {time}  \u{2022}  {size}", megabytes(speed as u64))
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pending {
    pub name: String,
    pub mime: String,
    pub path: PathBuf,
    /// Our own copy of a pasted picture, to delete afterwards.
    pub temporary: bool,
}

impl Pending {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    /// The bytes to send, and tidies up a temporary copy.
    pub fn into_upload(self) -> Option<Upload> {
        let bytes = std::fs::read(&self.path).ok()?;
        if self.temporary {
            let _ = std::fs::remove_file(&self.path);
        }
        Some(Upload { name: self.name, mime: self.mime, bytes, sent: Default::default() })
    }

    pub fn discard(&self) {
        if self.temporary {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn from_path(path: &Path) -> Pending {
        let name = path.file_name().map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
        Pending { mime: mime_of(&name).to_string(), name, path: path.to_path_buf(), temporary: false }
    }
}

/// The type Discord should be told, from a file's extension.
pub fn mime_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "txt" => "text/plain",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn clipboard(args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("wl-paste").args(args).output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// `file:///home/me/a%20b.png` → the path.
fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let raw = uri.trim().strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let escaped = (raw[i] == b'%')
            .then(|| std::str::from_utf8(raw.get(i + 1..i + 3)?).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(raw[i]);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// What the clipboard holds that could be attached: copied files, or a
/// picture. Text stays with the input's own paste. Blocks: run off the UI thread.
pub fn from_clipboard() -> Vec<Pending> {
    let Some(types) = clipboard(&["--list-types"]) else { return Vec::new() };
    let types = String::from_utf8_lossy(&types).into_owned();
    if types.lines().any(|t| t == "text/uri-list") {
        let uris = clipboard(&["--type", "text/uri-list"]).unwrap_or_default();
        return String::from_utf8_lossy(&uris)
            .lines()
            .filter_map(path_from_uri)
            .filter(|p| p.is_file())
            .map(|p| Pending::from_path(&p))
            .collect();
    }
    if types.lines().any(|t| t.starts_with("text/plain")) {
        return Vec::new();
    }
    let Some(mime) = ["image/png", "image/jpeg", "image/webp", "image/gif"].iter().find(|m| types.lines().any(|t| t == **m)) else {
        return Vec::new();
    };
    let Some(bytes) = clipboard(&["--type", mime]) else { return Vec::new() };
    let ext = mime.rsplit('/').next().unwrap_or("png");
    let dir = cord_core::paths::cache_dir().join("uploads");
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = dir.join(format!("{stamp}.{ext}"));
    if std::fs::create_dir_all(&dir).is_err() || std::fs::write(&path, bytes).is_err() {
        return Vec::new();
    }
    vec![Pending { name: format!("image.{ext}"), mime: mime.to_string(), path, temporary: true }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_file_uris_become_paths() {
        assert_eq!(path_from_uri("file:///home/me/a%20b.png"), Some(PathBuf::from("/home/me/a b.png")));
        assert_eq!(path_from_uri("https://x.y/a.png"), None);
    }

    #[test]
    fn types_come_from_the_extension() {
        assert_eq!(mime_of("A.PNG"), "image/png");
        assert_eq!(mime_of("clip.mp4"), "video/mp4");
        assert_eq!(mime_of("noext"), "application/octet-stream");
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn says_how_its_going() {
        assert_eq!(status_line(0, 4 * MB, 0.), "Starting  \u{2022}  4.0 MB");
        assert_eq!(status_line(MB, 4 * MB, 1.), "25%  \u{2022}  1.0 MB/s  \u{2022}  3s left  \u{2022}  1.0 MB of 4.0 MB");
        assert_eq!(status_line(MB, 200 * MB, 1.), "0%  \u{2022}  1.0 MB/s  \u{2022}  3m 19s left  \u{2022}  1.0 MB of 200.0 MB");
        assert_eq!(status_line(4 * MB, 4 * MB, 2.), "Finishing  \u{2022}  4.0 MB");
    }

    #[test]
    fn says_why_its_refused() {
        assert_eq!(too_large("a.mp4", 12 * MB, 10 * MB), "a.mp4 is 12.0 MB. The limit here is 10.0 MB.");
    }
}
