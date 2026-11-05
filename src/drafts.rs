//! Unsent text, per channel, kept across restarts: what you were typing, and a
//! message you were part way through editing. For this account's eyes only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cord_core::Id;
use gpui_kit::App;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    #[serde(default)]
    pub text: String,
    /// A message being edited, and its text so far.
    #[serde(default)]
    pub edit: Option<(Id, String)>,
    /// Files waiting to be sent with it.
    #[serde(default)]
    pub files: Vec<crate::uploads::Pending>,
}

impl Draft {
    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.edit.is_none() && self.files.is_empty()
    }
}

type Drafts = HashMap<Id, Draft>;

thread_local! {
    static DRAFTS: RefCell<Option<Drafts>> = const { RefCell::new(None) };
}

/// Saves waiting: only the newest writes, once typing pauses.
static GENERATION: AtomicU64 = AtomicU64::new(0);
const SAVE_AFTER: Duration = Duration::from_millis(300);

fn path() -> PathBuf {
    cord_core::paths::config_dir().join("drafts.json")
}

fn with<R>(f: impl FnOnce(&mut Drafts) -> R) -> R {
    DRAFTS.with_borrow_mut(|held| {
        let drafts = held.get_or_insert_with(|| std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
        f(drafts)
    })
}

fn write(body: Vec<u8>) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn save_soon(cx: &App) {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_AFTER).await;
        if GENERATION.load(Ordering::Relaxed) != generation {
            return;
        }
        let body = with(|drafts| serde_json::to_vec(drafts).unwrap_or_default());
        cx.background_executor().spawn(async move { write(body) }).await;
    })
    .detach();
}

pub fn get(channel: Id) -> Draft {
    with(|drafts| drafts.get(&channel).cloned().unwrap_or_default())
}

fn change(drafts: &mut Drafts, channel: Id, f: impl FnOnce(&mut Draft)) -> bool {
    let draft = drafts.entry(channel).or_default();
    let before = draft.clone();
    f(draft);
    let changed = *draft != before;
    if draft.is_empty() {
        drafts.remove(&channel);
    }
    changed
}

pub fn set_text(channel: Id, text: String, cx: &App) {
    if with(|d| change(d, channel, |draft| draft.text = text)) {
        save_soon(cx);
    }
}

pub fn set_files(channel: Id, files: Vec<crate::uploads::Pending>, cx: &App) {
    if with(|d| change(d, channel, |draft| draft.files = files)) {
        save_soon(cx);
    }
}

pub fn set_edit(channel: Id, edit: Option<(Id, String)>, cx: &App) {
    if with(|d| change(d, channel, |draft| draft.edit = edit)) {
        save_soon(cx);
    }
}

/// Signing out: they're not the next account's to see.
pub fn forget_all() {
    with(Drafts::clear);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    let _ = std::fs::remove_file(path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_leaves_the_message_being_written_alone() {
        let mut drafts = Drafts::new();
        change(&mut drafts, Id(1), |d| d.text = "hello".into());
        change(&mut drafts, Id(1), |d| d.edit = Some((Id(9), "fixed".into())));
        change(&mut drafts, Id(1), |d| d.edit = None);
        assert_eq!(drafts[&Id(1)].text, "hello");
    }

    #[test]
    fn empty_ones_are_not_kept_and_changes_are_noticed() {
        let mut drafts = Drafts::new();
        assert!(change(&mut drafts, Id(1), |d| d.text = "a".into()));
        assert!(!change(&mut drafts, Id(1), |d| d.text = "a".into()));
        assert!(change(&mut drafts, Id(1), |d| d.text.clear()));
        assert!(drafts.is_empty());
    }

    #[test]
    fn survive_being_written_out() {
        let mut drafts = Drafts::new();
        change(&mut drafts, Id(5), |d| {
            d.text = "<@1> hi".into();
            d.edit = Some((Id(7), "x".into()));
        });
        let back: Drafts = serde_json::from_slice(&serde_json::to_vec(&drafts).unwrap()).unwrap();
        assert_eq!(back, drafts);
    }
}
