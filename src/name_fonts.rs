//! The fonts of Discord's "Display Name Styles" (Nitro), bundled in
//! `assets/name-fonts` (SIL OFL, see the LICENSE there) and keyed by the
//! `font_id` Discord sends.

use std::borrow::Cow;

use gpui_kit::App;

/// (font_id, family inside the file, the file). 11 is Discord's default font.
const FONTS: &[(u8, &str, &[u8])] = &[
    (1, "Bangers", include_bytes!("../assets/name-fonts/Bangers-Regular.ttf")),
    (2, "BioRhyme", include_bytes!("../assets/name-fonts/BioRhyme-wdth-wght.ttf")),
    (3, "Cherry Bomb One", include_bytes!("../assets/name-fonts/CherryBombOne-Regular.ttf")),
    (4, "Chicle", include_bytes!("../assets/name-fonts/Chicle-Regular.ttf")),
    (6, "MuseoModerno", include_bytes!("../assets/name-fonts/MuseoModerno-wght.ttf")),
    (7, "Medieval", include_bytes!("../assets/name-fonts/NeoCastel.ttf")),
    (8, "Pixelify Sans", include_bytes!("../assets/name-fonts/PixelifySans-wght.ttf")),
    (10, "Vampyre", include_bytes!("../assets/name-fonts/Sinistre.ttf")),
    (12, "Zilla Slab", include_bytes!("../assets/name-fonts/ZillaSlab-Regular.ttf")),
    (13, "Playpen Sans", include_bytes!("../assets/name-fonts/PlaypenSans-wght.ttf")),
    (14, "Orbitron", include_bytes!("../assets/name-fonts/Orbitron-wght.ttf")),
    (15, "New Rocker", include_bytes!("../assets/name-fonts/NewRocker-Regular.ttf")),
    (16, "Kalam", include_bytes!("../assets/name-fonts/Kalam-Regular.ttf")),
];

/// Hands the fonts to GPUI's text system. Once, at start.
pub fn init(cx: &mut App) {
    let fonts = FONTS.iter().map(|(_, _, bytes)| Cow::Borrowed(*bytes)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "couldn't load the display name fonts");
    }
}

/// The family to draw a name in, for a style's `font_id`; `None` keeps the UI font.
pub fn family(font_id: u8) -> Option<&'static str> {
    FONTS.iter().find(|(id, ..)| *id == font_id).map(|(_, family, _)| *family)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids_have_families() {
        assert_eq!(family(10), Some("Vampyre"));
        assert_eq!(family(11), None);
        assert_eq!(family(99), None);
    }
}
