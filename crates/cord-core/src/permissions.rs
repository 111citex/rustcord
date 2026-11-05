//! Discord's permission algorithm ("Permission Overwrites" in the API docs),
//! as pure functions over the model types.

use crate::Id;
use crate::model::{Channel, Guild, OverwriteKind};

pub const ALL: u64 = u64::MAX;
pub const CREATE_INSTANT_INVITE: u64 = 1 << 0;
pub const KICK_MEMBERS: u64 = 1 << 1;
pub const BAN_MEMBERS: u64 = 1 << 2;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const PIN_MESSAGES: u64 = 1 << 51;
pub const MANAGE_NICKNAMES: u64 = 1 << 27;
pub const MANAGE_ROLES: u64 = 1 << 28;
pub const MODERATE_MEMBERS: u64 = 1 << 40;
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const MANAGE_GUILD: u64 = 1 << 5;
pub const CREATE_EVENTS: u64 = 1 << 44;
pub const ADD_REACTIONS: u64 = 1 << 6;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const CONNECT: u64 = 1 << 20;
pub const MUTE_MEMBERS: u64 = 1 << 22;
pub const DEAFEN_MEMBERS: u64 = 1 << 23;
pub const MOVE_MEMBERS: u64 = 1 << 24;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const EMBED_LINKS: u64 = 1 << 14;
pub const ATTACH_FILES: u64 = 1 << 15;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const MENTION_EVERYONE: u64 = 1 << 17;
pub const USE_EXTERNAL_EMOJIS: u64 = 1 << 18;
pub const USE_EXTERNAL_STICKERS: u64 = 1 << 37;
pub const SEND_MESSAGES_IN_THREADS: u64 = 1 << 38;

/// Server-wide permissions: everything for the owner, else @everyone (the
/// role whose id is the guild's) plus the member's roles. Roles the guild no
/// longer has are ignored.
pub fn base(guild: &Guild, user: Id, member_roles: &[Id]) -> u64 {
    if guild.owner_id == Some(user) {
        return ALL;
    }
    let mut perms = guild.roles.iter().find(|r| r.id == guild.id).map_or(0, |r| r.permissions);
    for role in guild.roles.iter().filter(|r| member_roles.contains(&r.id)) {
        perms |= role.permissions;
    }
    if perms & ADMINISTRATOR != 0 { ALL } else { perms }
}

/// What `user` may do in `channel`: [`base`] with the channel's overwrites
/// applied in Discord's order (@everyone, then all roles together, then the
/// member). Administrators skip the overwrites.
pub fn compute(guild: &Guild, channel: &Channel, user: Id, member_roles: &[Id]) -> u64 {
    let mut perms = base(guild, user, member_roles);
    if perms & ADMINISTRATOR != 0 {
        return ALL;
    }
    let overwrites = &channel.permission_overwrites;
    if let Some(o) = overwrites.iter().find(|o| o.kind == OverwriteKind::Role && o.id == guild.id) {
        perms = perms & !o.deny | o.allow;
    }
    let (mut deny, mut allow) = (0, 0);
    for o in overwrites
        .iter()
        .filter(|o| o.kind == OverwriteKind::Role && o.id != guild.id && member_roles.contains(&o.id))
    {
        deny |= o.deny;
        allow |= o.allow;
    }
    perms = perms & !deny | allow;
    if let Some(o) = overwrites.iter().find(|o| o.kind == OverwriteKind::Member && o.id == user) {
        perms = perms & !o.deny | o.allow;
    }
    perms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChannelKind, PermissionOverwrite, Role};

    const GUILD: Id = Id(100);
    const ME: Id = Id(1);
    const MOD: Id = Id(200);
    const MUTED: Id = Id(201);

    fn role(id: Id, permissions: u64) -> Role {
        Role { id, name: String::new(), color: 0, position: 0, permissions, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false }
    }

    fn guild(roles: Vec<Role>) -> Guild {
        Guild {
            id: GUILD, name: "g".into(), icon: None, unavailable: false, channels: vec![],
            roles, owner_id: Some(Id(999)), voice_states: vec![], emojis: vec![], premium_tier: 0, banner: None, rules_channel: None,
        }
    }

    fn channel(overwrites: Vec<PermissionOverwrite>) -> Channel {
        Channel {
            id: Id(300), kind: ChannelKind::Text, guild_id: Some(GUILD), name: None, position: 0,
            parent_id: None, last_message_id: None, recipient_ids: vec![], recipients: vec![], owner_id: None, icon: None,
            permission_overwrites: overwrites, topic: None,
        }
    }

    fn ow(id: Id, kind: OverwriteKind, allow: u64, deny: u64) -> PermissionOverwrite {
        PermissionOverwrite { id, kind, allow, deny }
    }

    const BASIC: u64 = VIEW_CHANNEL | SEND_MESSAGES;

    #[test]
    fn owner_gets_everything_even_when_denied() {
        let mut g = guild(vec![role(GUILD, 0)]);
        g.owner_id = Some(ME);
        let c = channel(vec![ow(GUILD, OverwriteKind::Role, 0, ALL)]);
        assert_eq!(compute(&g, &c, ME, &[]), ALL);
    }

    #[test]
    fn administrator_bypasses_overwrites() {
        let g = guild(vec![role(GUILD, VIEW_CHANNEL), role(MOD, ADMINISTRATOR)]);
        let c = channel(vec![ow(MOD, OverwriteKind::Role, 0, VIEW_CHANNEL), ow(ME, OverwriteKind::Member, 0, ALL)]);
        assert_eq!(compute(&g, &c, ME, &[MOD]), ALL);
    }

    #[test]
    fn roles_add_to_everyone() {
        let g = guild(vec![role(GUILD, VIEW_CHANNEL), role(MOD, SEND_MESSAGES)]);
        assert_eq!(compute(&g, &channel(vec![]), ME, &[MOD]), BASIC);
        assert_eq!(compute(&g, &channel(vec![]), ME, &[]), VIEW_CHANNEL);
    }

    #[test]
    fn everyone_deny_then_role_allow() {
        let g = guild(vec![role(GUILD, BASIC), role(MOD, 0)]);
        let c = channel(vec![
            ow(GUILD, OverwriteKind::Role, 0, VIEW_CHANNEL),
            ow(MOD, OverwriteKind::Role, VIEW_CHANNEL, 0),
        ]);
        assert_eq!(compute(&g, &c, ME, &[]), SEND_MESSAGES);
        assert_eq!(compute(&g, &c, ME, &[MOD]), BASIC);
    }

    #[test]
    fn role_overwrites_combine_with_allow_beating_deny() {
        let g = guild(vec![role(GUILD, BASIC), role(MOD, 0), role(MUTED, 0)]);
        let c = channel(vec![
            ow(MUTED, OverwriteKind::Role, 0, SEND_MESSAGES),
            ow(MOD, OverwriteKind::Role, SEND_MESSAGES, 0),
        ]);
        assert_eq!(compute(&g, &c, ME, &[MUTED, MOD]), BASIC);
        assert_eq!(compute(&g, &c, ME, &[MUTED]), VIEW_CHANNEL);
    }

    #[test]
    fn member_overwrite_beats_roles() {
        let g = guild(vec![role(GUILD, BASIC), role(MUTED, 0)]);
        let c = channel(vec![
            ow(MUTED, OverwriteKind::Role, 0, SEND_MESSAGES),
            ow(ME, OverwriteKind::Member, SEND_MESSAGES, 0),
        ]);
        assert_eq!(compute(&g, &c, ME, &[MUTED]), BASIC);
        let c = channel(vec![ow(ME, OverwriteKind::Member, 0, VIEW_CHANNEL)]);
        assert_eq!(compute(&g, &c, ME, &[]), SEND_MESSAGES);
    }

    #[test]
    fn other_members_overwrites_are_ignored() {
        let g = guild(vec![role(GUILD, BASIC)]);
        let c = channel(vec![ow(Id(2), OverwriteKind::Member, 0, VIEW_CHANNEL)]);
        assert_eq!(compute(&g, &c, ME, &[]), BASIC);
    }

    #[test]
    fn a_role_missing_from_the_guild_is_ignored() {
        let g = guild(vec![role(GUILD, BASIC)]);
        assert_eq!(compute(&g, &channel(vec![]), ME, &[Id(777)]), BASIC);
    }

    #[test]
    fn no_everyone_role_means_no_base_permissions() {
        let g = guild(vec![]);
        assert_eq!(compute(&g, &channel(vec![]), ME, &[]), 0);
    }

    #[test]
    fn empty_overwrites_leave_the_base() {
        let g = guild(vec![role(GUILD, BASIC | ATTACH_FILES)]);
        assert_eq!(compute(&g, &channel(vec![]), ME, &[]), BASIC | ATTACH_FILES);
    }
}
