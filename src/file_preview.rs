//! Text files in chat: their contents right there, markdown drawn as markdown
//! with a switch to the raw text, and a download button on every file.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use cord_core::Core;
use cord_core::rich::Attachment;
use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{AnyElement, App, ClipboardItem, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div};

use crate::markdown_view::{self, Env};
use crate::ui::u;

/// Larger files stay plain file cards.
const MAX_BYTES: u64 = 512 * 1024;
/// What's drawn of a file at most, and of a closed one.
const MAX_CHARS: usize = 20_000;
const CLOSED_LINES: usize = 20;

#[derive(Clone)]
enum Load {
    Loading,
    Text(Rc<str>),
    Failed,
}

#[derive(Default)]
struct Previews {
    core: Option<Arc<Core>>,
    files: HashMap<String, Load>,
    /// Markdown files showing their raw text, and files opened in full.
    raw: HashSet<String>,
    open: HashSet<String>,
}

thread_local! {
    static PREVIEWS: RefCell<Previews> = RefCell::default();
}

pub fn set_core(core: Arc<Core>) {
    PREVIEWS.with_borrow_mut(|p| p.core = Some(core));
}

fn extension(name: &str) -> String {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

pub fn is_markdown(name: &str) -> bool {
    matches!(extension(name).as_str(), "md" | "markdown")
}

/// Whether to show the file's contents: text, and not huge.
pub fn previewable(a: &Attachment) -> bool {
    const TEXT: [&str; 26] = [
        "txt", "md", "markdown", "log", "json", "toml", "yaml", "yml", "csv", "ini", "cfg", "conf", "rs", "py", "js", "ts", "c", "h", "cpp", "hpp",
        "java", "go", "sh", "lua", "html", "css",
    ];
    let by_type = a.content_type.as_deref().is_some_and(|t| t.starts_with("text/") || t.contains("json"));
    a.size <= MAX_BYTES && !a.url.is_empty() && (by_type || TEXT.contains(&extension(&a.filename).as_str()))
}

/// What to draw of `text`, and whether there's more than that.
fn shown(text: &str, open: bool) -> (&str, bool) {
    let end = if open {
        text.len()
    } else {
        text.match_indices('\n').nth(CLOSED_LINES - 1).map_or(text.len(), |(at, _)| at)
    };
    let end = text.char_indices().nth(MAX_CHARS).map_or(end, |(at, _)| at.min(end));
    (&text[..end], end < text.len())
}

fn load(url: &str, message: cord_core::Id, remeasure: Rc<dyn Fn(cord_core::Id, &mut App)>, cx: &App) -> Load {
    let (known, core) = PREVIEWS.with_borrow(|p| (p.files.get(url).cloned(), p.core.clone()));
    if let Some(known) = known {
        return known;
    }
    let Some(core) = core else { return Load::Failed };
    PREVIEWS.with_borrow_mut(|p| p.files.insert(url.to_string(), Load::Loading));
    let url = url.to_string();
    cx.spawn(async move |cx| {
        let loaded = match core.cdn_bytes(url.clone()).await {
            Ok(bytes) => Load::Text(String::from_utf8_lossy(&bytes).replace("\r\n", "\n").into()),
            Err(error) => {
                tracing::warn!(%error, "couldn't load a file preview");
                Load::Failed
            }
        };
        PREVIEWS.with_borrow_mut(|p| p.files.insert(url, loaded));
        let _ = cx.update(|cx| {
            remeasure(message, cx);
            cx.refresh_windows();
        });
    })
    .detach();
    Load::Loading
}

/// A free spot for `name` in `folder`, and only ever inside it: the name comes from a stranger.
fn download_path(folder: &Path, name: &str) -> Option<PathBuf> {
    let name = Path::new(name).file_name()?.to_str()?;
    let (stem, dot_ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    std::iter::once(folder.join(name)).chain((1..1000).map(|n| folder.join(format!("{stem} ({n}){dot_ext}")))).find(|p| !p.exists())
}

pub fn download(url: String, name: String, cx: &mut App) {
    let Some(core) = PREVIEWS.with_borrow(|p| p.core.clone()) else { return };
    cx.spawn(async move |cx| {
        let saved = match core.cdn_bytes(url).await {
            Ok(bytes) => {
                cx.background_executor()
                    .spawn(async move {
                        let folder = dirs::download_dir()?;
                        std::fs::create_dir_all(&folder).ok()?;
                        let path = download_path(&folder, &name)?;
                        std::fs::write(&path, bytes).ok()?;
                        Some(path)
                    })
                    .await
            }
            Err(error) => {
                tracing::warn!(%error, "couldn't download a file");
                None
            }
        };
        match saved {
            Some(path) => crate::notifications::show("Downloaded", &path.display().to_string(), false, None),
            None => crate::notifications::show("Couldn't download the file", "", false, None),
        }
    })
    .detach();
}

fn icon_button(id: impl Into<ElementId>, icon: IconName, tip: &'static str, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .size(u(28.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(u(5.))
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(|d| d.bg(theme.secondary_hover).text_color(theme.foreground))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(Icon::new(icon).size(u(18.)))
}

/// The download button on a file.
pub fn download_button(id: impl Into<ElementId>, url: &str, name: &str, cx: &App) -> impl IntoElement {
    let (url, name) = (url.to_string(), name.to_string());
    icon_button(id, IconName::Download, "Download", cx).on_click(move |_, _, cx| {
        cx.stop_propagation();
        download(url.clone(), name.clone(), cx);
    })
}

/// A text file's contents and its bar of buttons. `None` if it couldn't be read.
pub fn view(env: &Env, a: &Attachment, size_label: String, cx: &App) -> Option<AnyElement> {
    let theme = cx.theme();
    let message = env.message.id;
    let remeasure = env.shared.remeasure.clone();
    let url = a.url.clone();
    let load = load(&url, message, remeasure.clone(), cx);
    if matches!(load, Load::Failed) {
        return None;
    }
    let markdown = is_markdown(&a.filename);
    let (raw, open) = PREVIEWS.with_borrow(|p| (p.raw.contains(&url), p.open.contains(&url)));
    let id = |what: &str| SharedString::from(format!("file-{what}-{}-{}", message, a.id));
    let text = if let Load::Text(text) = &load { Some(text.clone()) } else { None };
    let (body, more): (AnyElement, bool) = match &text {
        None => (div().text_color(theme.muted_foreground).child("Loading…").into_any_element(), false),
        Some(text) => {
            let (part, more) = shown(text, open);
            let element = if markdown && !raw {
                let parsed = env.parse(part);
                markdown_view::blocks(env, &parsed, &format!("file-{}", a.id), cx)
            } else {
                div().font_family(theme.mono_font_family.clone()).text_size(u(13.)).child(part.to_string()).into_any_element()
            };
            (element, more)
        }
    };
    let flip = |set: fn(&mut Previews) -> &mut HashSet<String>| {
        let (url, remeasure) = (url.clone(), remeasure.clone());
        move |_: &gpui_kit::ClickEvent, _: &mut gpui_kit::Window, cx: &mut App| {
            cx.stop_propagation();
            PREVIEWS.with_borrow_mut(|p| {
                let set = set(p);
                if !set.remove(&url) {
                    set.insert(url.clone());
                }
            });
            remeasure(message, cx);
            cx.refresh_windows();
        }
    };
    Some(
        v_flex()
            .w_full()
            .max_w(u(720.))
            .mt(u(4.))
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(div().w_full().min_w_0().p(u(12.)).child(body))
            .child(
                h_flex()
                    .w_full()
                    .px(u(8.))
                    .py(u(6.))
                    .gap(u(6.))
                    .items_center()
                    .border_t_1()
                    .border_color(theme.border)
                    .when(more || open, |d| {
                        let icon = if open { IconName::ChevronUp } else { IconName::ChevronDown };
                        d.child(icon_button(id("open"), icon, if open { "Show Less" } else { "Show All" }, cx).on_click(flip(|p| &mut p.open)))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_size(u(14.)).font_weight(FontWeight::MEDIUM).child(a.filename.clone()))
                            .child(div().text_size(u(12.)).text_color(theme.muted_foreground).child(size_label)),
                    )
                    .when(markdown, |d| {
                        let tip = if raw { "View Formatted" } else { "View Raw" };
                        let icon = if raw { IconName::FileText } else { IconName::Code };
                        d.child(icon_button(id("raw"), icon, tip, cx).on_click(flip(|p| &mut p.raw)))
                    })
                    .when_some(text, |d, text| {
                        d.child(icon_button(id("copy"), IconName::Copy, "Copy", cx).on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                        }))
                    })
                    .child(download_button(id("download"), &a.url, &a.filename, cx)),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(name: &str, kind: Option<&str>, size: u64) -> Attachment {
        Attachment { id: cord_core::Id(1), filename: name.into(), url: "https://x/y".into(), proxy_url: String::new(), content_type: kind.map(String::from), width: None, height: None, size, duration_secs: None, waveform: None }
    }

    #[test]
    fn previews_text_of_a_sensible_size() {
        assert!(previewable(&attachment("a.txt", None, 10)));
        assert!(previewable(&attachment("notes.MD", None, 10)));
        assert!(previewable(&attachment("noext", Some("text/plain; charset=utf-8"), 10)));
        assert!(!previewable(&attachment("a.zip", Some("application/zip"), 10)));
        assert!(!previewable(&attachment("a.txt", None, MAX_BYTES + 1)));
    }

    #[test]
    fn a_closed_file_shows_its_first_lines() {
        let text = (0..30).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let (part, more) = shown(&text, false);
        assert_eq!(part.lines().count(), CLOSED_LINES);
        assert!(more);
        assert_eq!(shown(&text, true), (text.as_str(), false));
        assert_eq!(shown("short", false), ("short", false));
        let long = "é".repeat(MAX_CHARS + 5);
        let (huge, more) = shown(&long, true);
        assert_eq!(huge.chars().count(), MAX_CHARS);
        assert!(more);
    }

    #[test]
    fn downloads_stay_in_the_folder_and_never_overwrite() {
        let folder = std::env::temp_dir().join(format!("rustcord-dl-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(download_path(&folder, "../../evil.txt").unwrap(), folder.join("evil.txt"));
        assert_eq!(download_path(&folder, "/etc/passwd").unwrap(), folder.join("passwd"));
        std::fs::write(folder.join("a.txt"), "x").unwrap();
        assert_eq!(download_path(&folder, "a.txt").unwrap(), folder.join("a (1).txt"));
        assert!(download_path(&folder, "..").is_none());
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
