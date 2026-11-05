//! Theme loading and switching.
//!
//! The app ships the files in `themes/` inside the binary and copies them
//! to `~/.config/rustcord/themes` on first run. That folder is watched, so a
//! theme dropped in (or edited) there shows up in Settings immediately.

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeConfig, ThemeRegistry, try_parse_color};
use gpui_kit::{App, Hsla, SharedString};

pub const DEFAULT_THEME: &str = "Discord Midnight";

pub fn themes_dir() -> std::path::PathBuf {
    cord_core::paths::config_dir().join("themes")
}

const BUNDLED: &[(&str, &str)] = &[
    ("adventure.json", include_str!("../themes/adventure.json")),
    ("alduin.json", include_str!("../themes/alduin.json")),
    ("asciinema.json", include_str!("../themes/asciinema.json")),
    ("aurora.json", include_str!("../themes/aurora.json")),
    ("ayu.json", include_str!("../themes/ayu.json")),
    ("discord.json", include_str!("../themes/discord.json")),
    ("catppuccin.json", include_str!("../themes/catppuccin.json")),
    ("everforest.json", include_str!("../themes/everforest.json")),
    ("fahrenheit.json", include_str!("../themes/fahrenheit.json")),
    ("flexoki.json", include_str!("../themes/flexoki.json")),
    ("gruvbox.json", include_str!("../themes/gruvbox.json")),
    ("harper.json", include_str!("../themes/harper.json")),
    ("hybrid.json", include_str!("../themes/hybrid.json")),
    ("jellybeans.json", include_str!("../themes/jellybeans.json")),
    ("kibble.json", include_str!("../themes/kibble.json")),
    (
        "macos-classic.json",
        include_str!("../themes/macos-classic.json"),
    ),
    (
        "mellifluous.json",
        include_str!("../themes/mellifluous.json"),
    ),
    ("molokai.json", include_str!("../themes/molokai.json")),
    ("solarized.json", include_str!("../themes/solarized.json")),
    ("spaceduck.json", include_str!("../themes/spaceduck.json")),
    ("tokyonight.json", include_str!("../themes/tokyonight.json")),
    ("twilight.json", include_str!("../themes/twilight.json")),
];

/// Registers every theme and applies `selected`, then keeps watching the
/// user's theme folder.
pub fn init(selected: String, cx: &mut App) {
    let dir = themes_dir();

    if let Err(error) = seed(&dir) {
        tracing::warn!(%error, "Couldn't copy bundled themes");
    }

    // Register the bundled themes right away so the first frame is themed
    // even before the folder watcher finishes its initial load.
    for (_, content) in BUNDLED {
        if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(content) {
            tracing::warn!(%error, "Invalid bundled theme");
        }
    }

    use_desktop_fonts(cx);
    apply(&selected, cx);

    let watched = selected.clone();
    if let Err(error) = ThemeRegistry::watch_dir(dir, cx, move |cx| {
        // Keep whichever theme the user picked since start-up.
        let current = Theme::global(cx).theme_name().to_string();
        let name = if current.is_empty() {
            watched.clone()
        } else {
            current
        };
        apply(&name, cx);
    }) {
        tracing::error!(%error, "Failed to watch themes directory");
    }
}

/// Draws text in the desktop's own fonts (fontconfig's `sans-serif` and
/// `monospace`) instead of GPUI's guess at a system font. A theme file that
/// names a family still wins, since `apply_config` only sets what it holds.
fn use_desktop_fonts(cx: &mut App) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let family = |pattern: &str| -> Option<SharedString> {
        let out = std::process::Command::new("fc-match").args(["-f", "%{family[0]}", pattern]).output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !name.is_empty()).then(|| name.into())
    };
    let (sans, mono) = (family("sans-serif"), family("monospace"));
    tracing::info!(?sans, ?mono, "desktop fonts");
    let theme = Theme::global_mut(cx);
    if let Some(sans) = sans {
        theme.font_family = sans;
    }
    if let Some(mono) = mono {
        theme.mono_font_family = mono;
    }
}

fn seed(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;

    for (file, content) in BUNDLED {
        let path = dir.join(file);
        if !path.exists() {
            std::fs::write(path, content)?;
        }
    }

    Ok(())
}

/// Switches to the theme called `name`, falling back to the default theme.
pub fn apply(name: &str, cx: &mut App) -> bool {
    let registry = ThemeRegistry::global(cx);

    let config = registry
        .themes()
        .get(&SharedString::from(name.to_string()))
        .or_else(|| registry.themes().get(&SharedString::from(DEFAULT_THEME)))
        .cloned();

    let Some(config) = config else {
        tracing::warn!(theme = name, "Theme not found");
        return false;
    };

    Theme::update(cx, |theme| theme.apply_config(&config));

    true
}

/// Every registered theme, dark themes first, then by name.
pub fn available(cx: &App) -> Vec<Rc<ThemeConfig>> {
    let mut themes = ThemeRegistry::global(cx).themes().values().cloned().collect::<Vec<_>>();
    themes.sort_by(|a, b| {
        b.mode.is_dark().cmp(&a.mode.is_dark()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    themes
}

/// Colors that identify a theme in the picker. A theme without a value
/// falls back to the active theme's, so a partial file still previews.
pub struct Swatch {
    pub background: Hsla,
    pub surface: Hsla,
    pub foreground: Hsla,
    pub primary: Hsla,
}

pub fn swatch(config: &ThemeConfig, cx: &App) -> Swatch {
    let theme = Theme::global(cx);
    let colors = &config.colors;
    let pick = |value: &Option<SharedString>, fallback: Hsla| {
        value.as_ref().and_then(|value| try_parse_color(value).ok()).unwrap_or(fallback)
    };
    Swatch {
        background: pick(&colors.background, theme.background),
        surface: pick(&colors.secondary, pick(&colors.muted, theme.secondary)),
        foreground: pick(&colors.foreground, theme.foreground),
        primary: pick(&colors.primary, theme.primary),
    }
}
