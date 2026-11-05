//! User preferences: what the Settings page edits. Saved as JSON in the
//! config folder on every change; missing fields take defaults and
//! out-of-range values are clamped, so old or hand-edited files still load.

use std::path::{Path, PathBuf};

use gpui_kit::Context;
use serde::{Deserialize, Serialize};

pub const ZOOM_MIN: u16 = 50;
pub const ZOOM_MAX: u16 = 200;
const ZOOM_STEP: u16 = 10;
pub const CHAT_TEXT_MIN: u8 = 12;
pub const CHAT_TEXT_MAX: u8 = 24;
/// GPUI's default rem, which zoom scales.
const BASE_REM: f32 = 16.;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    /// Whole-UI scale, in percent.
    pub zoom: u16,
    /// Message text size, in pixels.
    pub chat_text: u8,
    /// One line per message: time, name, text; no avatars.
    pub compact: bool,
    pub smooth_scrolling: bool,
    /// How many screens' worth of pictures to keep ready either way, 0 to 5.
    pub image_screens: u8,
    /// Allocations from this many KB up go straight back to the system when freed.
    pub big_memory_kb: u16,
    /// The member list on the right of a server channel.
    pub show_members: bool,
    /// How wide the channel column and member list have been dragged, in the
    /// UI's own pixels.
    pub column_width: f32,
    pub members_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: crate::theme::DEFAULT_THEME.into(),
            zoom: 100,
            chat_text: 16,
            compact: false,
            smooth_scrolling: true,
            image_screens: 1,
            big_memory_kb: BIG_MEMORY_STEPS[0],
            show_members: true,
            column_width: 303.,
            members_width: 264.,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        cord_core::paths::config_dir().join("settings.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "ignoring unreadable settings file");
                    Settings::default()
                })
                .clamped(),
            Err(_) => Settings::default(),
        }
    }

    /// Writes through a temporary file so a crash never leaves half a file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("settings serialize"))?;
        std::fs::rename(tmp, path)
    }

    pub fn clamped(mut self) -> Self {
        self.column_width = self.column_width.clamp(240., 480.);
        self.members_width = self.members_width.clamp(200., 420.);
        let zoom = self.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self.zoom = (zoom + ZOOM_STEP / 2) / ZOOM_STEP * ZOOM_STEP;
        self.chat_text = self.chat_text.clamp(CHAT_TEXT_MIN, CHAT_TEXT_MAX);
        self.image_screens = self.image_screens.min(MAX_IMAGE_SCREENS);
        self.big_memory_kb = step_big_memory(self.big_memory_kb, 0);
        self
    }

    /// The window's rem size for this zoom.
    pub fn rem_px(&self) -> f32 {
        BASE_REM * self.zoom as f32 / 100.
    }
}

pub fn step_zoom(zoom: u16, direction: i8) -> u16 {
    let next = if direction < 0 { zoom.saturating_sub(ZOOM_STEP) } else { zoom + ZOOM_STEP };
    next.clamp(ZOOM_MIN, ZOOM_MAX)
}

pub const MAX_IMAGE_SCREENS: u8 = 5;
pub const BIG_MEMORY_STEPS: [u16; 6] = [64, 128, 256, 512, 1024, 2048];

/// The next step of the setting that way, from the one nearest `kb`.
pub fn step_big_memory(kb: u16, direction: i8) -> u16 {
    let at = BIG_MEMORY_STEPS.iter().position(|step| *step >= kb).unwrap_or(BIG_MEMORY_STEPS.len() - 1);
    BIG_MEMORY_STEPS[at.saturating_add_signed(direction as isize).min(BIG_MEMORY_STEPS.len() - 1)]
}

pub fn step_image_screens(screens: u8, direction: i8) -> u8 {
    screens.saturating_add_signed(direction).min(MAX_IMAGE_SCREENS)
}

pub fn step_chat_text(size: u8, direction: i8) -> u8 {
    let next = if direction < 0 { size.saturating_sub(1) } else { size + 1 };
    next.clamp(CHAT_TEXT_MIN, CHAT_TEXT_MAX)
}

/// The settings as an entity views observe; every change is saved at once.
pub struct SettingsStore {
    settings: Settings,
}

impl SettingsStore {
    pub fn new(settings: Settings) -> Self {
        SettingsStore { settings }
    }

    pub fn get(&self) -> &Settings {
        &self.settings
    }

    /// For a change still in motion, like a drag: applied now, saved by `update`
    /// (or `save`) when it ends, not on every step.
    pub fn update_unsaved(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Settings)) {
        edit(&mut self.settings);
        self.settings = std::mem::take(&mut self.settings).clamped();
        cx.notify();
    }

    pub fn save(&self) {
        if let Err(error) = self.settings.save_to(&Settings::path()) {
            tracing::error!(%error, "couldn't save settings");
        }
    }

    pub fn update(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Settings)) {
        let before = self.settings.clone();
        edit(&mut self.settings);
        self.settings = std::mem::take(&mut self.settings).clamped();
        if self.settings == before {
            return;
        }
        if let Err(error) = self.settings.save_to(&Settings::path()) {
            tracing::error!(%error, "couldn't save settings");
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let settings = Settings { image_screens: 3, big_memory_kb: 256, theme: "Gruvbox".into(), zoom: 120, chat_text: 18, compact: true, smooth_scrolling: false, show_members: false, column_width: 303., members_width: 264., };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
    }

    #[test]
    fn missing_or_corrupt_files_give_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load_from(&dir.path().join("none.json")), Settings::default());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(Settings::load_from(&bad), Settings::default());
    }

    #[test]
    fn old_files_keep_their_values_and_fill_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.json");
        std::fs::write(&path, r#"{"theme":"Tokyo Night"}"#).unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.theme, "Tokyo Night");
        assert_eq!(s.zoom, 100);
        assert!(s.smooth_scrolling);
        assert!(s.show_members, "the member list is on unless turned off");
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wild.json");
        std::fs::write(&path, r#"{"zoom":500,"chat_text":2}"#).unwrap();
        let s = Settings::load_from(&path);
        assert_eq!((s.zoom, s.chat_text), (ZOOM_MAX, CHAT_TEXT_MIN));
        // Off-step zoom snaps to the nearest step.
        assert_eq!(Settings { zoom: 113, ..Settings::default() }.clamped().zoom, 110);
    }

    #[test]
    fn zoom_steps_stay_in_range() {
        assert_eq!(step_zoom(100, 1), 110);
        assert_eq!(step_zoom(ZOOM_MAX, 1), ZOOM_MAX);
        assert_eq!(step_zoom(ZOOM_MIN, -1), ZOOM_MIN);
        assert_eq!(step_big_memory(64, -1), 64);
        assert_eq!(step_big_memory(64, 1), 128);
        assert_eq!(step_big_memory(2048, 1), 2048);
        assert_eq!(step_big_memory(100, 0), 128, "something in between lands on a step");
        assert_eq!(step_image_screens(0, -1), 0);
        assert_eq!(step_image_screens(1, 1), 2);
        assert_eq!(step_image_screens(MAX_IMAGE_SCREENS, 1), MAX_IMAGE_SCREENS);
        assert_eq!(step_chat_text(16, 1), 17);
        assert_eq!(step_chat_text(CHAT_TEXT_MAX, 1), CHAT_TEXT_MAX);
        assert_eq!(Settings { zoom: 125, ..Settings::default() }.rem_px(), 20.);
    }
}
