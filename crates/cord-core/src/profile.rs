//! A user's profile as `GET /users/{id}/profile` returns it, flattened to
//! what the profile card shows. Server-specific values (bio, pronouns, banner,
//! accent colour) take precedence over the user's global ones.

use serde::{Deserialize, Deserializer};

use crate::Id;
use crate::cdn;
use crate::model::User;

#[derive(Clone, Debug, PartialEq)]
pub struct UserProfile {
    pub user: User,
    pub bio: Option<String>,
    pub pronouns: Option<String>,
    /// 0xRRGGBB, shown behind the avatar when there is no banner image.
    pub accent_color: Option<u32>,
    banner: Option<String>,
    /// Set when the banner is the member's server banner.
    banner_guild: Option<Id>,
    server_banner: bool,
    /// The server nickname, when the profile was asked for in a server.
    pub nick: Option<String>,
    /// ISO 8601 time they joined that server.
    pub joined_at: Option<String>,
    pub roles: Vec<Id>,
    /// Nitro profile theme: (primary, accent), the card's gradient top to bottom.
    pub theme_colors: Option<(u32, u32)>,
    pub badges: Vec<ProfileBadge>,
    /// Servers both share, and friends both have (None: not asked / unknown).
    pub mutual_guilds: Option<Vec<Id>>,
    pub mutual_friends: Option<u32>,
    /// The server avatar hash, when the profile was asked for in a server.
    pub guild_avatar: Option<String>,
    guild: Option<Id>,
    /// Up to a few mutual friends' avatars, for the row of faces.
    pub mutual_friend_avatars: Vec<String>,
    /// The profile frame they wear: its shop item's id.
    pub frame_sku: Option<String>,
    /// The profile effect's item, to fetch its pictures with.
    pub effect_sku: Option<String>,
    /// Friends both have, for the full profile's list.
    pub mutual_friend_list: Vec<MutualFriend>,
    /// Linked accounts: (service, name on it).
    pub connections: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MutualFriend {
    pub id: Id,
    pub name: String,
    pub username: String,
    pub avatar_url: String,
}

#[derive(Deserialize)]
struct RawConnection {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
}

/// A profile frame's artwork, from `GET /collectibles-products/{sku}`. Sizes
/// are in the art's own pixels, where the card is `inner_width` wide.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ProfileFrame {
    #[serde(default)]
    pub sku_id: String,
    #[serde(default)]
    pub layers: Vec<FrameLayer>,
    #[serde(default)]
    pub inner_width: f32,
    #[serde(default)]
    pub overflow_top: f32,
    #[serde(default)]
    pub overflow_bottom: f32,
    #[serde(default)]
    pub overflow_horizontal: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct FrameLayer {
    pub id: String,
    /// `staple` (a picture at the top or bottom), `rail`, `border`.
    #[serde(default, rename = "type")]
    pub kind: String,
    /// `front` or `back` of the card.
    #[serde(default)]
    pub order: String,
    /// `top`, `bottom` or `center`.
    #[serde(default)]
    pub anchor: String,
}

impl ProfileFrame {
    pub fn layer_url(&self, layer: &FrameLayer) -> String {
        format!("{}/media/v1/collectibles-shop/{}/{}/static", cdn::CDN, self.sku_id, layer.id)
    }
}

#[derive(Deserialize)]
struct RawProduct {
    #[serde(default)]
    items: Vec<ProfileFrame>,
}

/// The frame in a collectibles product response, if it holds one.
pub fn frame_from_product(body: serde_json::Value) -> Option<ProfileFrame> {
    serde_json::from_value::<RawProduct>(body).ok()?.items.into_iter().find(|i| i.inner_width > 0.)
}

#[derive(Deserialize, Default, Clone)]
struct RawCollectible {
    #[serde(default)]
    sku_id: String,
    #[serde(default, rename = "type")]
    kind: u8,
}

/// The collectibles types of a profile effect and a profile frame.
const EFFECT: u8 = 1;
const FRAME: u8 = 3;

/// A profile effect's artwork, from `GET /collectibles-products/{sku}`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ProfileEffect {
    #[serde(default)]
    pub effects: Vec<EffectLayer>,
}

/// One APNG of an effect, and when it plays, in milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct EffectLayer {
    pub src: String,
    #[serde(default, rename = "loop")]
    pub looping: bool,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub start: u64,
    #[serde(default, rename = "loopDelay")]
    pub loop_delay: u64,
    #[serde(default, rename = "zIndex")]
    pub z_index: i32,
}

/// The effect in a collectibles product response, if it holds one.
pub fn effect_from_product(body: serde_json::Value) -> Option<ProfileEffect> {
    #[derive(Deserialize)]
    struct Product {
        #[serde(default)]
        items: Vec<ProfileEffect>,
    }
    serde_json::from_value::<Product>(body).ok()?.items.into_iter().find(|i| !i.effects.is_empty())
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ProfileBadge {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub link: Option<String>,
}

impl ProfileBadge {
    pub fn icon_url(&self) -> String {
        format!("{}/badge-icons/{}.png", cdn::CDN, self.icon)
    }
}

impl UserProfile {
    /// Records the server the profile was asked for: some responses don't
    /// repeat it, and a server avatar or banner needs it for its URL.
    pub fn seen_from(&mut self, guild: Option<Id>) {
        if self.guild.is_none() {
            self.guild = guild;
        }
        if self.server_banner && self.banner_guild.is_none() {
            self.banner_guild = guild;
        }
    }

    /// Their server avatar in the server the profile was asked for, if set.
    pub fn guild_avatar_url(&self, size: u32) -> Option<String> {
        Some(cdn::guild_avatar_url(self.guild?, self.user.id, self.guild_avatar.as_deref()?, size))
    }

    pub fn banner_url(&self, size: u32) -> Option<String> {
        Some(cdn::banner_url(self.user.id, self.banner_guild, self.banner.as_deref()?, size))
    }
}

#[derive(Deserialize, Default)]
struct RawExtras {
    #[serde(default)]
    guild_id: Option<Id>,
    #[serde(default)]
    bio: Option<String>,
    #[serde(default)]
    pronouns: Option<String>,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    accent_color: Option<u32>,
    #[serde(default)]
    theme_colors: Option<Vec<u32>>,
    #[serde(default)]
    collectibles: Option<Vec<RawCollectible>>,
}

#[derive(Deserialize, Default)]
struct RawMember {
    #[serde(default)]
    nick: Option<String>,
    #[serde(default)]
    joined_at: Option<String>,
    #[serde(default)]
    roles: Vec<Id>,
    #[serde(default)]
    avatar: Option<String>,
    // A member's own copies of their server profile.
    #[serde(default)]
    bio: Option<String>,
    #[serde(default)]
    banner: Option<String>,
}

#[derive(Deserialize)]
struct RawMutualGuild {
    id: Id,
}

#[derive(Deserialize)]
struct RawUser {
    #[serde(flatten)]
    user: User,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    accent_color: Option<u32>,
    #[serde(default)]
    bio: Option<String>,
    #[serde(default)]
    pronouns: Option<String>,
}

#[derive(Deserialize)]
struct Raw {
    user: RawUser,
    #[serde(default)]
    user_profile: Option<RawExtras>,
    #[serde(default)]
    guild_member_profile: Option<RawExtras>,
    #[serde(default)]
    guild_member: Option<RawMember>,
    #[serde(default)]
    badges: Vec<ProfileBadge>,
    #[serde(default)]
    guild_badges: Vec<ProfileBadge>,
    #[serde(default)]
    mutual_guilds: Option<Vec<RawMutualGuild>>,
    #[serde(default)]
    mutual_friends_count: Option<u32>,
    #[serde(default)]
    mutual_friends: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    connected_accounts: Vec<RawConnection>,
}

/// The first value that is present and not empty.
fn first<T>(values: [Option<T>; 3], empty: impl Fn(&T) -> bool) -> Option<T> {
    values.into_iter().flatten().find(|v| !empty(v))
}

impl<'de> Deserialize<'de> for UserProfile {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Raw::deserialize(d)?;
        let (server, global) = (raw.guild_member_profile.unwrap_or_default(), raw.user_profile.unwrap_or_default());
        let text = |s: &String| s.trim().is_empty();
        let member = raw.guild_member.unwrap_or_default();
        let server_banner = server.banner.clone().filter(|b| !b.is_empty()).or(member.banner.clone().filter(|b| !b.is_empty()));
        let has_server_banner = server_banner.is_some();
        let banner_guild = server_banner.as_ref().and(server.guild_id);
        let frame = |c: &Option<Vec<RawCollectible>>| c.iter().flatten().find(|c| c.kind == FRAME).map(|c| c.sku_id.clone());
        let frame_sku = frame(&server.collectibles).or_else(|| frame(&global.collectibles));
        let effect = |c: &Option<Vec<RawCollectible>>| c.iter().flatten().find(|c| c.kind == EFFECT).map(|c| c.sku_id.clone());
        let effect_sku = effect(&server.collectibles).or_else(|| effect(&global.collectibles));
        let theme = |t: Option<Vec<u32>>| t.and_then(|c| Some((*c.first()?, *c.get(1)?)));
        let theme_colors = theme(server.theme_colors).or_else(|| theme(global.theme_colors));
        let mutual_friends = raw.mutual_friends_count.or(raw.mutual_friends.as_ref().map(|f| f.len() as u32));
        Ok(UserProfile {
            theme_colors,
            badges: raw.badges.into_iter().chain(raw.guild_badges).collect(),
            mutual_guilds: raw.mutual_guilds.map(|g| g.into_iter().map(|g| g.id).collect()),
            mutual_friends,
            guild_avatar: member.avatar.filter(|a| !a.is_empty()),
            guild: server.guild_id,
            frame_sku,
            effect_sku,
            mutual_friend_list: raw
                .mutual_friends
                .iter()
                .flatten()
                .filter_map(|f| {
                    let id = Id(f["id"].as_str()?.parse().ok()?);
                    let username = f["username"].as_str().unwrap_or_default().to_string();
                    let name = f["global_name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&username).to_string();
                    Some(MutualFriend { id, name, username, avatar_url: cdn::avatar_url(id, f["avatar"].as_str(), 64) })
                })
                .collect(),
            connections: raw.connected_accounts.into_iter().map(|c| (c.kind, c.name)).collect(),
            server_banner: has_server_banner,
            // Discord sends them in a different order each time: the same three each time here.
            mutual_friend_avatars: {
                let mut faces: Vec<(Id, String)> = raw
                    .mutual_friends
                    .iter()
                    .flatten()
                    .filter_map(|f| {
                        let id = Id(f["id"].as_str()?.parse().ok()?);
                        Some((id, cdn::avatar_url(id, f["avatar"].as_str(), 40)))
                    })
                    .collect();
                faces.sort_by_key(|(id, _)| *id);
                faces.into_iter().take(3).map(|(_, url)| url).collect()
            },
            bio: first([server.bio.or(member.bio), global.bio, raw.user.bio], text),
            pronouns: first([server.pronouns, global.pronouns, raw.user.pronouns], text),
            accent_color: first([server.accent_color, global.accent_color, raw.user.accent_color], |_| false),
            banner_guild,
            banner: first([server_banner, global.banner, raw.user.banner], |b| b.is_empty()),
            nick: member.nick.filter(|n| !n.is_empty()),
            joined_at: member.joined_at,
            roles: member.roles,
            user: raw.user.user,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(v: serde_json::Value) -> UserProfile {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn global_profile() {
        let p = profile(json!({
            "user": {"id": "2", "username": "lin", "global_name": "Lin", "avatar": "av", "banner": "bn", "accent_color": 255},
            "user_profile": {"bio": "hello", "pronouns": "they/them"},
        }));
        assert_eq!(p.user.display_name(), "Lin");
        assert_eq!((p.bio.as_deref(), p.pronouns.as_deref()), (Some("hello"), Some("they/them")));
        assert_eq!(p.accent_color, Some(255));
        assert_eq!(p.banner_url(480).unwrap(), "https://cdn.discordapp.com/banners/2/bn.png?size=480");
        assert!(p.roles.is_empty() && p.joined_at.is_none());
    }

    #[test]
    fn server_values_win() {
        let p = profile(json!({
            "user": {"id": "2", "username": "lin", "banner": "global-banner", "bio": "old"},
            "user_profile": {"bio": "global bio", "pronouns": "he/him", "accent_color": 1},
            "guild_member_profile": {"guild_id": "300", "bio": "server bio", "pronouns": "", "banner": "server-banner", "accent_color": 2},
            "guild_member": {"nick": "Zenny", "roles": ["301", "302"], "joined_at": "2024-01-02T03:04:05.000000+00:00"},
        }));
        assert_eq!(p.bio.as_deref(), Some("server bio"));
        assert_eq!(p.pronouns.as_deref(), Some("he/him"), "an empty server value falls back");
        assert_eq!(p.accent_color, Some(2));
        assert_eq!(
            p.banner_url(480).unwrap(),
            "https://cdn.discordapp.com/guilds/300/users/2/banners/server-banner.png?size=480"
        );
        assert_eq!(p.nick.as_deref(), Some("Zenny"));
        assert_eq!(p.roles, vec![Id(301), Id(302)]);
        assert_eq!(p.joined_at.as_deref(), Some("2024-01-02T03:04:05.000000+00:00"));
    }

    #[test]
    fn theme_badges_and_mutuals() {
        let p = profile(json!({
            "user": {"id": "2", "username": "lin"},
            "user_profile": {"theme_colors": [1, 2]},
            "guild_member_profile": {"guild_id": "300"},
            "guild_member": {"avatar": "sa"},
            "badges": [{"id": "hypesquad", "description": "HypeSquad", "icon": "abc"}],
            "mutual_guilds": [{"id": "300", "nick": null}, {"id": "301"}],
            "mutual_friends_count": 4,
        }));
        assert_eq!(p.theme_colors, Some((1, 2)));
        assert_eq!(p.badges[0].icon_url(), "https://cdn.discordapp.com/badge-icons/abc.png");
        assert_eq!(p.mutual_guilds, Some(vec![Id(300), Id(301)]));
        assert_eq!(p.mutual_friends, Some(4));
        assert!(p.guild_avatar_url(64).unwrap().contains("/guilds/300/users/2/avatars/sa"));
    }

    #[test]
    fn effects_come_from_the_shop() {
        let effect = effect_from_product(serde_json::json!({"items": [{
            "type": 1, "sku_id": "1", "effects": [
                {"src": "https://x/a", "loop": false, "duration": 2880, "start": 0, "loopDelay": 0, "zIndex": 100},
                {"src": "https://x/b", "loop": true, "duration": 2880, "start": 5760, "loopDelay": 5760, "zIndex": 101},
            ],
        }]}))
        .unwrap();
        assert_eq!(effect.effects.len(), 2);
        assert!(effect.effects[1].looping);
        assert_eq!((effect.effects[1].start, effect.effects[1].loop_delay, effect.effects[1].z_index), (5760, 5760, 101));
        assert!(effect_from_product(serde_json::json!({"items": [{"type": 3}]})).is_none());
    }

    #[test]
    fn frames_come_from_collectibles_and_the_shop() {
        let p = profile(json!({
            "user": {"id": "2", "username": "lin"},
            "user_profile": {"collectibles": [{"sku_id": "7", "type": 1}, {"sku_id": "8", "type": 3}]},
        }));
        assert_eq!(p.frame_sku.as_deref(), Some("8"));
        let frame = frame_from_product(json!({"items": [{
            "type": 3, "sku_id": "8", "inner_width": 1200, "overflow_top": 211, "overflow_bottom": 186, "overflow_horizontal": 56,
            "layers": [{"id": "9", "type": "staple", "order": "front", "anchor": "top"}]
        }]}))
        .unwrap();
        assert_eq!(frame.layer_url(&frame.layers[0]), "https://cdn.discordapp.com/media/v1/collectibles-shop/8/9/static");
    }

    #[test]
    fn bare_user_still_decodes() {
        let p = profile(json!({"user": {"id": "2", "username": "lin", "bio": "", "banner": null}}));
        assert_eq!((p.bio.as_deref(), p.banner_url(64)), (None, None));
    }
}
