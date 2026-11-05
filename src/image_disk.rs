//! Downloaded pictures, kept on disk. Pictures go through GPUI's HTTP client, so
//! this wraps it: asked for one it has, it answers from the file, and Discord isn't
//! asked at all. What's in memory can then be let go of freely, since getting it
//! back costs a file read. The oldest files go first past `LIMIT`.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use futures::AsyncReadExt as _;
use futures::future::BoxFuture;
use gpui_kit::http_client::{AsyncBody, HttpClient, Method, Request, Response, Url, http::HeaderValue};

/// Bytes on disk before the oldest files are removed, down to `KEEP`.
const LIMIT: u64 = 512 * 1024 * 1024;
const KEEP: u64 = 400 * 1024 * 1024;
/// Not worth a file of its own past this.
const LARGEST: usize = 32 * 1024 * 1024;
/// How many new files between checks of the total.
const CHECK_EVERY: u32 = 256;
/// Only Discord's own hosts: nothing else's content should stay around.
const HOSTS: [&str; 4] = ["cdn.discordapp.com", "media.discordapp.net", "images-ext-1.discordapp.net", "images-ext-2.discordapp.net"];

pub fn dir() -> PathBuf {
    cord_core::paths::cache_dir().join("images")
}

/// Signing out: they're not the next account's to see.
pub fn forget_all() {
    let _ = std::fs::remove_dir_all(dir());
}

/// What identifies a picture: its address without the signature, which changes while
/// the picture doesn't. `None` for anything that isn't kept.
fn key(uri: &str) -> Option<u64> {
    let rest = uri.strip_prefix("https://")?;
    let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
    let host = location.split('/').next()?;
    if !HOSTS.contains(&host) {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    location.hash(&mut hasher);
    for param in query.split('&').filter(|p| !matches!(p.split('=').next(), Some("ex" | "is" | "hm"))) {
        param.hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn file_for(dir: &Path, key: u64) -> PathBuf {
    let name = format!("{key:016x}");
    dir.join(&name[..2]).join(&name)
}

/// A file from Discord's CDN, from disk if it's there, off the UI thread.
pub async fn cdn_bytes(core: &Arc<cord_core::Core>, url: String, cx: &gpui_kit::AsyncApp) -> Option<Vec<u8>> {
    let file = key(&url).map(|key| file_for(&dir(), key));
    let background = cx.background_executor().clone();
    if let Some(file) = file.clone()
        && let Some(bytes) = background.spawn(async move { read(&file) }).await
    {
        return Some(bytes);
    }
    let bytes = core.cdn_bytes(url).await.ok()?;
    if let Some(file) = file {
        let copy = bytes.clone();
        background
            .spawn(async move {
                if let Err(error) = write(&file, &copy) {
                    tracing::warn!(%error, "couldn't keep a file on disk");
                }
            })
            .detach();
    }
    Some(bytes)
}

/// Reads a kept picture, marking it as just used.
fn read(file: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(file).ok()?;
    if let Ok(handle) = std::fs::File::options().write(true).open(file) {
        let _ = handle.set_modified(SystemTime::now());
    }
    Some(bytes)
}

/// Writes whole or not at all: a half-written file would be a broken picture for good.
fn write(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = file.parent().ok_or(std::io::ErrorKind::NotFound)?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir(), std::fs::Permissions::from_mode(0o700));
    }
    let partial = file.with_extension("partial");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(partial, file)
}

/// Every file under `dir`, with its size and last use.
fn files(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut found = Vec::new();
    for folder in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        for file in std::fs::read_dir(folder.path()).into_iter().flatten().flatten() {
            if let Ok(meta) = file.metadata() {
                found.push((file.path(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    found
}

/// Removes the least recently used files, if there's too much.
fn trim(dir: &Path, limit: u64, keep: u64) {
    let mut found = files(dir);
    let mut total: u64 = found.iter().map(|f| f.1).sum();
    if total <= limit {
        return;
    }
    found.sort_by_key(|f| f.2);
    for (path, size, _) in found {
        if total <= keep {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total -= size;
        }
    }
}

fn trim_soon() {
    std::thread::spawn(|| trim(&dir(), LIMIT, KEEP));
}

pub struct DiskCached {
    inner: Arc<dyn HttpClient>,
    written: AtomicU32,
}

impl DiskCached {
    pub fn new(inner: Arc<dyn HttpClient>) -> Self {
        trim_soon();
        DiskCached { inner, written: AtomicU32::new(0) }
    }
}

impl HttpClient for DiskCached {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }

    fn send(&self, request: Request<AsyncBody>) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let file = (request.method() == Method::GET)
            .then(|| key(&request.uri().to_string()))
            .flatten()
            .map(|key| file_for(&dir(), key));
        let Some(file) = file else { return self.inner.send(request) };
        if let Some(bytes) = read(&file) {
            return Box::pin(async move { Ok(Response::builder().status(200).body(AsyncBody::from(bytes))?) });
        }
        let sent = self.inner.send(request);
        let check = self.written.fetch_add(1, Ordering::Relaxed) % CHECK_EVERY == CHECK_EVERY - 1;
        Box::pin(async move {
            let mut response = sent.await?;
            if !response.status().is_success() {
                return Ok(response);
            }
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await?;
            if body.len() <= LARGEST {
                if let Err(error) = write(&file, &body) {
                    tracing::warn!(%error, "couldn't keep a picture on disk");
                }
                if check {
                    trim_soon();
                }
            }
            let (parts, _) = response.into_parts();
            Ok(Response::from_parts(parts, AsyncBody::from(body)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_signature_is_still_the_same_picture() {
        let a = key("https://cdn.discordapp.com/attachments/1/2/a.png?ex=1&is=2&hm=3&width=80");
        let b = key("https://cdn.discordapp.com/attachments/1/2/a.png?ex=9&is=8&hm=7&width=80");
        assert_eq!(a, b);
        assert!(a.is_some());
    }

    #[test]
    fn a_different_size_is_a_different_file() {
        let a = key("https://media.discordapp.net/a.png?width=80");
        let b = key("https://media.discordapp.net/a.png?width=160");
        assert_ne!(a, b);
    }

    #[test]
    fn other_hosts_are_not_kept() {
        assert_eq!(key("https://example.com/a.png"), None);
        assert_eq!(key("http://cdn.discordapp.com/a.png"), None);
    }

    #[test]
    fn the_least_recently_used_go_first() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("ab");
        std::fs::create_dir_all(&folder).unwrap();
        let now = SystemTime::now();
        for (name, age) in [("old", 300), ("mid", 200), ("new", 100)] {
            let path = folder.join(name);
            std::fs::write(&path, [0u8; 100]).unwrap();
            let handle = std::fs::File::options().write(true).open(&path).unwrap();
            handle.set_modified(now - std::time::Duration::from_secs(age)).unwrap();
        }
        trim(dir.path(), 250, 150);
        assert!(!folder.join("old").exists());
        assert!(!folder.join("mid").exists());
        assert!(folder.join("new").exists());
    }

    #[test]
    fn under_the_limit_nothing_goes() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("ab");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a"), [0u8; 100]).unwrap();
        trim(dir.path(), 250, 150);
        assert!(folder.join("a").exists());
    }

    #[test]
    fn a_kept_file_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = file_for(dir.path(), 0xabcd_ef01_2345_6789);
        write(&file, b"picture").unwrap();
        assert_eq!(read(&file).as_deref(), Some(&b"picture"[..]));
        assert!(!file.with_extension("partial").exists());
    }
}
