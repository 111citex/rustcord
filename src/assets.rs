//! The app's asset source: gpui-kit's default component icons plus the few
//! extra Lucide icons rustcord uses. gpui-kit only embeds about 100 icons by
//! default; anything else must be listed here or it fails to load.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

// The Lucide icons rustcord draws beyond gpui-kit's defaults, shared by every
// view. Add new ones here (the name is the file stem in CamelCase).
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        AtSign, ArrowUpRight, Download, PinOff, Eye, Wifi, PhoneOff, UserPlus, Bell, Copy, Link, Play, Pause, VolumeX, BellOff, CircleDot, CircleMinus, CirclePlus, Compass, Crown, FaceSlightlySmiling,
        FaceSlightlySmilingPlus, Film, Gamepad2, Gift, Hash, HeadphoneOff, Headphones, Image, Lock, Megaphone,
        MessageCircle, MessageSquare, Mic, MicOff, Monitor, Moon, Music, Paperclip, Pencil, Pin, Reply, Search,
        Shield, Smartphone, Sparkles, Sticker, Trash, Users, UsersRound, Volume2, ArrowLeft, ArrowRight, Inbox,
        CircleQuestionMark, Languages, Blocks, SendHorizontal, MessageSquareText, Folder, Gem, Clock, Flag, Leaf, Coffee,
        Plane, Lightbulb,
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = crate::discord_icons::load(path) {
            return Ok(Some(Cow::Owned(bytes)));
        }
        if path == "icons/new-member.svg" {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/icons/new-member.svg"))));
        }
        // Our own, built in, for until Discord's are here.
        let bundled: Option<&'static [u8]> = match path {
            "icons/win-minimize.svg" => Some(include_bytes!("../assets/icons/win-minimize.svg")),
            "icons/win-maximize.svg" => Some(include_bytes!("../assets/icons/win-maximize.svg")),
            "icons/win-restore.svg" => Some(include_bytes!("../assets/icons/win-restore.svg")),
            "icons/win-close.svg" => Some(include_bytes!("../assets/icons/win-close.svg")),
            "icons/rules.svg" => Some(include_bytes!("../assets/icons/rules.svg")),
            "icons/pin-off.svg" => Some(include_bytes!("../assets/icons/pin-off.svg")),
            "icons/phone.svg" => Some(include_bytes!("../assets/icons/phone.svg")),
            "icons/user-check.svg" => Some(include_bytes!("../assets/icons/user-check.svg")),
            "icons/crown.svg" => Some(include_bytes!("../assets/icons/crown.svg")),
            "icons/poll.svg" => Some(include_bytes!("../assets/icons/poll.svg")),
            "icons/automod.svg" => Some(include_bytes!("../assets/icons/automod.svg")),
            "icons/toolbar-threads.svg" => Some(include_bytes!("../assets/icons/toolbar-threads.svg")),
            "icons/toolbar-bell.svg" => Some(include_bytes!("../assets/icons/toolbar-bell.svg")),
            "icons/toolbar-pin.svg" => Some(include_bytes!("../assets/icons/toolbar-pin.svg")),
            "icons/toolbar-members.svg" => Some(include_bytes!("../assets/icons/toolbar-members.svg")),
            _ => None,
        };
        if let Some(bytes) = bundled {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if path == "icons/boost.svg" {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/icons/boost.svg"))));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_and_default_icons_both_load() {
        for path in ["icons/users.svg", "icons/lock.svg", "icons/hash.svg", "icons/volume-2.svg", "icons/settings.svg"] {
            assert!(AppAssets.load(path).unwrap().is_some(), "{path} missing");
        }
    }
}
