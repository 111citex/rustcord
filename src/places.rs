//! Where you were: the page to open at start, the direct message the home button goes
//! back to, and the channel each server goes back to. For this account: signing out
//! removes it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cord_core::Id;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Places {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub dm: Option<Id>,
    #[serde(default)]
    pub guilds: HashMap<Id, Id>,
    /// How loud each person is in calls: 1 as they come, up to 2.
    #[serde(default)]
    pub volumes: HashMap<Id, f32>,
    /// Those muted just for us.
    #[serde(default)]
    pub muted: Vec<Id>,
    /// People whose messages are folded away for us.
    #[serde(default)]
    pub ignored: Vec<Id>,
}

impl Places {
    /// Takes in a page being opened. Whether it changed anything.
    fn note(&mut self, guild: Option<Id>, channel: Option<Id>) -> bool {
        let before = self.clone();
        self.path = Some(match (guild, channel) {
            (Some(guild), Some(channel)) => format!("/channels/{guild}/{channel}"),
            (Some(guild), None) => format!("/channels/{guild}"),
            (None, Some(channel)) => format!("/channels/@me/{channel}"),
            (None, None) => "/channels/@me".to_string(),
        });
        match (guild, channel) {
            (Some(guild), Some(channel)) => {
                self.guilds.insert(guild, channel);
            }
            (None, Some(channel)) => self.dm = Some(channel),
            _ => {}
        }
        *self != before
    }
}

thread_local! {
    static HELD: RefCell<Option<Places>> = const { RefCell::new(None) };
}

fn path() -> PathBuf {
    cord_core::paths::config_dir().join("places.json")
}

fn read_from(path: &Path) -> Places {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write_to(path: &Path, places: &Places) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(places).unwrap_or_default())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)
}

/// What was saved, read once.
pub fn get() -> Places {
    HELD.with_borrow_mut(|held| held.get_or_insert_with(|| read_from(&path())).clone())
}

/// Whether we've muted `user` for ourselves.
pub fn is_muted(user: Id) -> bool {
    HELD.with_borrow_mut(|held| held.get_or_insert_with(|| read_from(&path())).muted.contains(&user))
}

/// Remembers a page being opened.
pub fn note(guild: Option<Id>, channel: Option<Id>) {
    let changed = HELD.with_borrow_mut(|held| {
        let places = held.get_or_insert_with(|| read_from(&path()));
        places.note(guild, channel).then(|| places.clone())
    });
    if let Some(places) = changed
        && let Err(error) = write_to(&path(), &places)
    {
        tracing::warn!(%error, "couldn't remember where you were");
    }
}

fn change(edit: impl FnOnce(&mut Places)) {
    let places = HELD.with_borrow_mut(|held| {
        let places = held.get_or_insert_with(|| read_from(&path()));
        edit(places);
        places.clone()
    });
    if let Err(error) = write_to(&path(), &places) {
        tracing::warn!(%error, "couldn't save that");
    }
}

pub fn set_volume(user: Id, volume: f32) {
    change(|places| {
        places.volumes.insert(user, volume);
    });
}

pub fn set_muted(user: Id, muted: bool) {
    change(|places| {
        places.muted.retain(|id| *id != user);
        if muted {
            places.muted.push(user);
        }
    });
}

/// Whether we've ignored `user`: their messages show folded.
pub fn is_ignored(user: Id) -> bool {
    HELD.with_borrow_mut(|held| held.get_or_insert_with(|| read_from(&path())).ignored.contains(&user))
}

pub fn set_ignored(user: Id, ignored: bool) {
    change(|places| {
        places.ignored.retain(|id| *id != user);
        if ignored {
            places.ignored.push(user);
        }
    });
}

/// What `user` should be played at.
pub fn gain_of(places: &Places, user: Id) -> f32 {
    if places.muted.contains(&user) { 0. } else { places.volumes.get(&user).copied().unwrap_or(1.) }
}

/// Signing out: it's not the next account's to see.
pub fn forget() {
    HELD.with_borrow_mut(|held| *held = Some(Places::default()));
    let _ = std::fs::remove_file(path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_survives_being_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("places.json");
        assert_eq!(read_from(&file), Places::default());
        let mut places = Places::default();
        places.note(Some(Id(1)), Some(Id(2)));
        places.note(None, Some(Id(3)));
        write_to(&file, &places).unwrap();
        let back = read_from(&file);
        assert_eq!(back.path.as_deref(), Some("/channels/@me/3"));
        assert_eq!(back.dm, Some(Id(3)));
        assert_eq!(back.guilds.get(&Id(1)), Some(&Id(2)));
    }

    #[test]
    fn a_broken_file_means_nothing_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("places.json");
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(read_from(&file), Places::default());
    }

    #[test]
    fn the_same_page_again_changes_nothing() {
        let mut places = Places::default();
        assert!(places.note(Some(Id(1)), Some(Id(2))));
        assert!(!places.note(Some(Id(1)), Some(Id(2))));
    }
}
