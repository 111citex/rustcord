//! Discord's own icons, read off its web app on this computer and kept in the cache.
//! None of them ship with rustcord: until they're here, or if Discord's files change
//! shape, our own drawings in `assets/icons` are used instead.
//!
//! The file is large, so this is careful with it: at most one look a week, a download
//! only when Discord has actually shipped a new one, off the UI thread, and nothing
//! kept but the icons themselves. New icons show from the next launch.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use regex::bytes::Regex;

/// Our file name, and what Discord exports that icon as.
const ICONS: [(&str, &str); 12] = [
    ("rules", "BookCheckIcon"),
    ("poll", "PollsIcon"),
    ("toolbar-threads", "ThreadIcon"),
    ("toolbar-bell", "BellIcon"),
    ("toolbar-members", "GroupIcon"),
    ("toolbar-pin", "PinIcon"),
    ("automod", "ShieldIcon"),
    ("boost", "BoostGemIcon"),
    ("new-member", "NewUserIcon"),
    ("user-check", "UserCheckIcon"),
    ("phone", "PhoneCallIcon"),
    ("crown", "CrownIcon"),
];

const LOOK_EVERY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// How far from its export an icon's import can be, and how long its code can run.
const IMPORT_RANGE: usize = 3_000_000;
const MODULE_RANGE: usize = 8_000;

fn folder() -> PathBuf {
    cord_core::paths::cache_dir().join("icons")
}

/// An icon that's been fetched, for `path` like `icons/pin.svg`.
pub fn load(path: &str) -> Option<Vec<u8>> {
    let stem = path.strip_prefix("icons/")?.strip_suffix(".svg")?;
    ICONS.iter().any(|(name, _)| *name == stem).then(|| std::fs::read(folder().join(format!("{stem}.svg"))).ok()).flatten()
}

/// `export` as an SVG, drawn in the current color. `None` if it can't be found.
fn extract(js: &[u8], export: &str) -> Option<String> {
    // `,PinIcon:()=>Sm.t`: the name it's imported under here. The comma keeps
    // `ChatShieldIcon` from passing for `ShieldIcon`.
    let table = Regex::new(&format!(r"[,{{]{export}:\(\)=>([A-Za-z0-9_$]+)")).ok()?.captures(js)?;
    let (at, local) = (table.get(0)?.end(), table.get(1)?.as_bytes());
    // `,Sm=n(366605)`: which module that is.
    let local = regex::escape(std::str::from_utf8(local).ok()?);
    let import = Regex::new(&format!(r"[,; ]{local}=[a-z]\((\d+)\)")).ok()?;
    let id = import.captures(js.get(at..(at + IMPORT_RANGE).min(js.len()))?)?.get(1)?.as_bytes();
    let id = std::str::from_utf8(id).ok()?;
    // `},366605(e,t,n){…`: the module itself, up to the next one.
    let module = Regex::new(&format!(r"[}},]{id}\(e,t,n\)\{{")).ok()?.find(js)?.end();
    let body = js.get(module..(module + MODULE_RANGE).min(js.len()))?;
    let next = Regex::new(r"\},\d+\(e").ok()?.find(body).map_or(body.len(), |m| m.start());
    let body = &body[..next];
    if !body.windows(5).any(|w| w == b"\"svg\"") {
        return None;
    }
    let paths: Vec<&str> = Regex::new(r#"[{,]d:"([^"]+)""#).ok()?.captures_iter(body).filter_map(|c| std::str::from_utf8(c.get(1)?.as_bytes()).ok()).collect();
    // Only path data goes in the file: anything else means this isn't what we thought.
    if paths.is_empty() || !paths.iter().all(|d| d.bytes().all(|b| b.is_ascii_alphanumeric() || b" .,-".contains(&b))) {
        return None;
    }
    let even_odd = body.windows(7).any(|w| w == b"evenodd");
    let rule = if even_odd { r#" fill-rule="evenodd""# } else { "" };
    let drawn: String = paths.iter().map(|d| format!(r#"<path fill="currentColor"{rule} d="{d}"/>"#)).collect();
    Some(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none">{drawn}</svg>"#))
}

/// The web app's main script, named on its login page.
fn main_script(page: &[u8]) -> Option<String> {
    let found = Regex::new(r"/assets/web\.[0-9a-f]+\.js").ok()?.find(page)?;
    String::from_utf8(found.as_bytes().to_vec()).ok()
}

/// Writes every icon it can find; returns how many that was.
fn save(js: &[u8], script: &str) -> usize {
    let dir = folder();
    let _ = std::fs::create_dir_all(&dir);
    let mut saved = 0;
    for (name, export) in ICONS {
        let Some(svg) = extract(js, export) else {
            tracing::info!(export, "discord icons: not found, keeping our own");
            continue;
        };
        // Whole files only, so a crash midway can't leave half an icon.
        let (temporary, file) = (dir.join(format!("{name}.tmp")), dir.join(format!("{name}.svg")));
        if std::fs::write(&temporary, svg).is_ok() && std::fs::rename(&temporary, &file).is_ok() {
            saved += 1;
        }
    }
    let _ = std::fs::write(dir.join("from"), script);
    saved
}

/// Brings the icons up to date, if it's time to look.
pub async fn refresh(core: std::sync::Arc<cord_core::Core>, background: gpui_kit::BackgroundExecutor) {
    let stamp = folder().join("looked");
    let recent = std::fs::metadata(&stamp).and_then(|m| m.modified()).ok().and_then(|at| SystemTime::now().duration_since(at).ok()).is_some_and(|age| age < LOOK_EVERY);
    if recent {
        return;
    }
    let Ok(page) = core.cdn_bytes("https://discord.com/login".to_string()).await else { return };
    let Some(script) = main_script(&page) else {
        tracing::info!("discord icons: no main script on the page");
        return;
    };
    // The same file as last time: nothing to download.
    let known = std::fs::read_to_string(folder().join("from")).is_ok_and(|from| from == script);
    if !known {
        let Ok(js) = core.cdn_bytes(format!("https://discord.com{script}")).await else { return };
        let name = script.clone();
        let saved = background.spawn(async move { save(&js, &name) }).await;
        tracing::info!(saved, total = ICONS.len(), "discord icons: updated");
    }
    let _ = std::fs::create_dir_all(folder());
    let _ = std::fs::write(stamp, "");
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made-up code in the same shape: a table, an import, and a module.
    const JS: &[u8] = br#"x.d(t,{ChatShieldIcon:()=>Aa.q,ShieldIcon:()=>Bb.r,GoneIcon:()=>Cc.s,BadIcon:()=>Dd.t});var Aa=n(111),Bb=n(222),Dd=n(444);
    },111(e,t,n){"use strict";r("svg",{children:r("path",{d:"M1 1h1"})})},222(e,t,n){"use strict";r("svg",{children:[r("path",{fillRule:"evenodd",d:"M2 2h2z"}),r("path",{d:"M3 3h3"})]})},444(e,t,n){"use strict";r("svg",{children:r("path",{d:"<script>"})})},555(e,t,n){}"#;

    #[test]
    fn it_finds_the_right_icon_and_not_one_ending_the_same() {
        let svg = extract(JS, "ShieldIcon").unwrap();
        assert!(svg.contains(r#"d="M2 2h2z""#) && svg.contains(r#"d="M3 3h3""#));
        assert!(!svg.contains("M1 1h1"), "that's ChatShieldIcon");
        assert!(svg.contains("currentColor") && svg.contains("evenodd"));
    }

    #[test]
    fn missing_or_odd_icons_are_left_alone() {
        assert_eq!(extract(JS, "NoSuchIcon"), None);
        assert_eq!(extract(JS, "GoneIcon"), None, "never imported");
        assert_eq!(extract(JS, "BadIcon"), None, "only path data gets into the file");
    }

    #[test]
    fn it_finds_the_main_script() {
        assert_eq!(main_script(br#"<script src="/assets/1.ab.js"></script><script src="/assets/web.0bcf.js">"#).as_deref(), Some("/assets/web.0bcf.js"));
        assert_eq!(main_script(b"<html>"), None);
    }
}
