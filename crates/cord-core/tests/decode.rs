use cord_core::Id;
use cord_core::event::{CoreEvent, decode_dispatch};
use cord_core::model::{ChannelKind, OverwriteKind, Ready};
use serde_json::json;

fn ready() -> Ready {
    serde_json::from_str(include_str!("fixtures/ready.json")).unwrap()
}

#[test]
fn decodes_ready_fixture() {
    let r = ready();
    assert_eq!(r.user.display_name(), "Ada");
    assert_eq!(r.session_id, "sess-1");
    assert_eq!(r.users[0].display_name(), "lin");
    let g = &r.guilds[0];
    assert_eq!(g.name, "testguild");
    assert_eq!(g.icon.as_deref(), Some("ic0"));
    assert_eq!(g.channels.len(), 4);
    assert!(g.channels.iter().all(|c| c.guild_id == Some(Id(300))));
    assert_eq!(g.channels[1].parent_id, Some(Id(310)));
    assert_eq!(g.channels[3].kind, ChannelKind::Forum);
    assert!(r.guilds[1].unavailable);
    assert_eq!(r.private_channels[0].recipient_ids, vec![Id(200)]);
    assert_eq!(r.private_channels[1].last_message_id, None);
}

#[test]
fn decodes_odd_messages() {
    // Attachment only, no content; numeric IDs; numeric nonce; null global_name; unknown fields.
    let d = json!({
        "id": 900, "channel_id": "311", "guild_id": "300",
        "author": {"id": "200", "username": "lin", "global_name": null, "avatar": null, "clan": {"tag": "X"}},
        "timestamp": "2026-09-30T10:00:00.000000+00:00",
        "edited_timestamp": null,
        "nonce": 1234567890123456789u64,
        "attachments": [{"id": "1", "filename": "a.png"}],
        "components": [], "flags": 0, "type": 0
    });
    let Some(CoreEvent::MessageCreate(m)) = decode_dispatch("MESSAGE_CREATE", d) else { panic!() };
    assert_eq!(m.id, Id(900));
    assert_eq!(m.content, "");
    assert_eq!(m.author.display_name(), "lin");
    assert_eq!(m.nonce.as_deref(), Some("1234567890123456789"));
    assert_eq!(m.attachments.len(), 1);
}

#[test]
fn dm_channel_create_fills_recipient_ids() {
    let d = json!({"id": "402", "type": 1, "recipients": [{"id": "200", "username": "lin"}]});
    let Some(CoreEvent::ChannelUpsert(c)) = decode_dispatch("CHANNEL_CREATE", d) else { panic!() };
    assert_eq!(c.kind, ChannelKind::Dm);
    assert_eq!(c.recipient_ids, vec![Id(200)]);
    assert_eq!(c.recipients[0].username, "lin");
}

#[test]
fn message_update_and_delete() {
    let up = decode_dispatch("MESSAGE_UPDATE", json!({"id": "900", "channel_id": "311", "content": "hi", "edited_timestamp": "t"}));
    assert!(matches!(up, Some(CoreEvent::MessageUpdate(ref u)) if u.id == Id(900) && u.content.as_deref() == Some("hi")));
    // Embed-only updates carry no content.
    let up = decode_dispatch("MESSAGE_UPDATE", json!({"id": "900", "channel_id": "311", "embeds": []}));
    assert!(matches!(up, Some(CoreEvent::MessageUpdate(ref u)) if u.content.is_none()));
    let del = decode_dispatch("MESSAGE_DELETE", json!({"id": "900", "channel_id": "311"}));
    assert!(matches!(del, Some(CoreEvent::MessageDelete { id: Id(900), channel_id: Id(311) })));
}

#[test]
fn guild_delete_outage_is_ignored() {
    assert!(decode_dispatch("GUILD_DELETE", json!({"id": "300", "unavailable": true})).is_none());
    assert!(matches!(
        decode_dispatch("GUILD_DELETE", json!({"id": "300"})),
        Some(CoreEvent::GuildDelete { id: Id(300) })
    ));
}

#[test]
fn plain_guild_shape_decodes() {
    let d = json!({"id": "302", "name": "plain", "icon": null, "channels": [{"id": "320", "type": 0, "name": "x"}]});
    let Some(CoreEvent::GuildCreate(g)) = decode_dispatch("GUILD_CREATE", d) else { panic!() };
    assert_eq!(g.name, "plain");
    assert_eq!(g.channels[0].guild_id, Some(Id(302)));
}

#[test]
fn unknown_and_broken_dispatches_are_none() {
    assert!(decode_dispatch("SOME_NEW_EVENT", json!({})).is_none());
    assert!(decode_dispatch("MESSAGE_CREATE", json!({"id": "x"})).is_none());
}

#[test]
fn unknown_channel_kind_is_other() {
    let c: cord_core::model::Channel = serde_json::from_value(json!({"id": "1", "type": 99})).unwrap();
    assert_eq!(c.kind, ChannelKind::Other(99));
    assert!(!c.kind.is_text_like());
    assert!(ChannelKind::Announcement.is_text_like());
}

#[test]
fn ready_skips_bad_elements() {
    let r: Ready = serde_json::from_value(json!({
        "user": {"id": "1", "username": "me"},
        "session_id": "s",
        "resume_gateway_url": "wss://x",
        "guilds": [
            {"id": "10", "name": "good", "channels": [
                {"id": "11", "type": 0, "name": "ok"},
                {"id": "12", "type": "x"}
            ]},
            {"id": "not-a-number"}
        ],
        "private_channels": [{"id": "20", "type": "x"}, {"id": "21", "type": 1}],
        "users": [{"id": "30"}, {"id": "31", "username": "u"}]
    })).unwrap();
    assert_eq!(r.guilds.len(), 1);
    assert_eq!(r.guilds[0].channels.len(), 1);
    assert_eq!(r.private_channels.len(), 1);
    assert_eq!(r.users.len(), 1);
    // The identity fields stay mandatory.
    assert!(serde_json::from_value::<Ready>(json!({"user": {"id": "1"}, "session_id": "s", "resume_gateway_url": "u"})).is_err());
}

#[test]
fn ready_carries_roles_overwrites_owner_and_own_members() {
    let d = json!({
        "user": {"id": "100", "username": "me"},
        "session_id": "s", "resume_gateway_url": "u",
        "guilds": [{
            "id": "300",
            "properties": {"name": "g", "owner_id": "999"},
            "roles": [
                {"id": "300", "name": "@everyone", "permissions": "1024", "position": 0},
                {"id": "301", "name": "mod", "permissions": "8", "color": 16711680, "position": 2, "hoist": true}
            ],
            "channels": [{"id": "311", "type": 0, "name": "x", "permission_overwrites": [
                {"id": "300", "type": 0, "allow": "0", "deny": "2048"},
                {"id": "100", "type": 1, "allow": "2048", "deny": "0"},
                {"id": "302", "type": "role", "allow": "1", "deny": "0"}
            ]}]
        }],
        "merged_members": [[{"user_id": "100", "nick": "Boss", "roles": ["301"], "communication_disabled_until": null}]]
    });
    let Some(CoreEvent::Ready(r)) = decode_dispatch("READY", d) else { panic!() };
    let g = &r.guilds[0];
    assert_eq!(g.owner_id, Some(Id(999)));
    assert_eq!(g.roles.len(), 2);
    assert_eq!(g.roles[1].permissions, 8);
    assert_eq!(g.roles[1].color, 0xff0000);
    assert!(g.roles[1].hoist);
    let o = &g.channels[0].permission_overwrites;
    assert_eq!(o.len(), 3);
    assert_eq!((o[0].kind, o[0].deny), (OverwriteKind::Role, 2048));
    assert_eq!((o[1].kind, o[1].allow), (OverwriteKind::Member, 2048));
    assert_eq!(o[2].kind, OverwriteKind::Role);
    let m = &r.merged_members[0][0];
    assert_eq!(m.user_id(), Some(Id(100)));
    assert_eq!(m.nick.as_deref(), Some("Boss"));
    assert_eq!(m.roles, vec![Id(301)]);
}

#[test]
fn guild_messages_carry_the_authors_member() {
    let d = json!({
        "id": "9", "channel_id": "311", "guild_id": "300",
        "author": {"id": "2", "username": "lin"},
        "member": {"nick": "Zenny", "roles": ["301"]},
        "content": "hi", "timestamp": "t"
    });
    let Some(CoreEvent::MessageCreate(m)) = decode_dispatch("MESSAGE_CREATE", d) else { panic!() };
    assert_eq!(m.member.unwrap().nick.as_deref(), Some("Zenny"));
}

#[test]
fn role_and_member_events() {
    let up = decode_dispatch("GUILD_ROLE_UPDATE", json!({"guild_id": "300", "role": {"id": "301", "name": "mod", "permissions": "8"}}));
    assert!(matches!(up, Some(CoreEvent::RoleUpsert { guild_id: Id(300), ref role }) if role.permissions == 8));
    let del = decode_dispatch("GUILD_ROLE_DELETE", json!({"guild_id": "300", "role_id": "301"}));
    assert!(matches!(del, Some(CoreEvent::RoleDelete { guild_id: Id(300), role_id: Id(301) })));
    let mem = decode_dispatch("GUILD_MEMBER_UPDATE", json!({"guild_id": "300", "user": {"id": "2", "username": "lin"}, "nick": "Z", "roles": []}));
    assert!(matches!(mem, Some(CoreEvent::MemberUpsert { guild_id: Id(300), ref member }) if member.user_id() == Some(Id(2))));
}

#[test]
fn decodes_a_reply_with_embeds_and_components() {
    let d = json!({
        "id": "901", "channel_id": "311", "guild_id": "300", "type": 19, "flags": 32768,
        "author": {"id": "200", "username": "lin"},
        "content": "yes",
        "timestamp": "2026-09-30T10:00:00.000000+00:00",
        "message_reference": {"type": 0, "message_id": "900", "channel_id": "311", "guild_id": "300"},
        "referenced_message": {
            "id": "900", "channel_id": "311", "author": {"id": "201", "username": "ann"},
            "content": "question?", "timestamp": "2026-09-30T09:59:00.000000+00:00"
        },
        "mentions": [{"id": "201", "username": "ann", "member": {"roles": []}}, 5],
        "sticker_items": [{"id": "77", "name": "wave", "format_type": 1}],
        "embeds": [{"type": "rich", "title": "t", "fields": [{"name": "a", "value": "b", "inline": true}]}],
        "components": [{"type": 17, "components": [{"type": 10, "content": "hi"}]}, {"type": 1, "components": []}]
    });
    let Some(CoreEvent::MessageCreate(m)) = decode_dispatch("MESSAGE_CREATE", d) else { panic!() };
    assert_eq!((m.kind, m.flags), (19, 32768));
    assert_eq!(m.message_reference.unwrap().message_id, Some(Id(900)));
    assert_eq!(m.referenced_message.unwrap().content, "question?");
    assert_eq!(m.mentions.len(), 1);
    assert_eq!(m.sticker_items[0].name, "wave");
    assert_eq!(m.embeds[0].fields.len(), 1);
    assert_eq!(m.components.len(), 2);
}

#[test]
fn deleted_reply_target_and_broken_nested_message() {
    let base = |extra: serde_json::Value| {
        let mut d = json!({"id": "902", "channel_id": "311", "author": {"id": "200", "username": "lin"}, "timestamp": "t", "type": 19});
        d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        d
    };
    // Discord sends null when the original was deleted.
    let Some(CoreEvent::MessageCreate(m)) = decode_dispatch("MESSAGE_CREATE", base(json!({"referenced_message": null, "message_reference": {"message_id": "1"}}))) else { panic!() };
    assert!(m.referenced_message.is_none());
    assert!(m.message_reference.is_some());
    // A nested message that can't be decoded doesn't lose the reply itself.
    let Some(CoreEvent::MessageCreate(m)) = decode_dispatch("MESSAGE_CREATE", base(json!({"referenced_message": {"id": "x"}}))) else { panic!() };
    assert!(m.referenced_message.is_none());
}

#[test]
fn message_update_carries_new_embeds() {
    let up = decode_dispatch("MESSAGE_UPDATE", json!({"id": "900", "channel_id": "311", "embeds": [{"type": "link", "url": "https://x"}]}));
    let Some(CoreEvent::MessageUpdate(u)) = up else { panic!() };
    assert_eq!(u.embeds.unwrap().len(), 1);
    let up = decode_dispatch("MESSAGE_UPDATE", json!({"id": "900", "channel_id": "311", "content": "x"}));
    assert!(matches!(up, Some(CoreEvent::MessageUpdate(ref u)) if u.embeds.is_none()));
}

#[test]
fn message_update_carries_components_attachments_and_flags_when_present() {
    let up = decode_dispatch(
        "MESSAGE_UPDATE",
        json!({"id": "900", "channel_id": "311", "flags": 32768,
               "components": [{"type": 10, "content": "hi"}],
               "attachments": [{"id": "1", "filename": "a.png", "size": 3, "url": "https://x/a.png"}]}),
    );
    let Some(CoreEvent::MessageUpdate(u)) = up else { panic!() };
    assert_eq!(u.flags, Some(32768));
    assert_eq!(u.components.unwrap().len(), 1);
    assert_eq!(u.attachments.unwrap().len(), 1);
    let up = decode_dispatch("MESSAGE_UPDATE", json!({"id": "900", "channel_id": "311", "content": "x"}));
    let Some(CoreEvent::MessageUpdate(u)) = up else { panic!() };
    assert!(u.components.is_none() && u.attachments.is_none() && u.flags.is_none());
}

#[test]
fn ready_guilds_carry_voice_states_leniently() {
    let d = json!({
        "user": {"id": "100", "username": "me"},
        "session_id": "s", "resume_gateway_url": "u",
        "guilds": [{
            "id": "300", "properties": {"name": "g"},
            "voice_states": [
                {"user_id": "5", "channel_id": "20", "self_mute": true, "self_deaf": false, "self_video": false, "self_stream": true, "extra": 1},
                {"user_id": "6", "channel_id": "20"},
                {"channel_id": "20"}
            ]
        }]
    });
    let Some(CoreEvent::Ready(r)) = decode_dispatch("READY", d) else { panic!() };
    let v = &r.guilds[0].voice_states;
    assert_eq!(v.len(), 2, "the broken entry is skipped");
    assert!(v[0].self_mute && v[0].self_stream && !v[0].self_deaf);
    assert!(!v[1].self_mute && !v[1].is_deafened());
}

#[test]
fn voice_state_update_decodes_guild_state_and_member() {
    let d = json!({
        "guild_id": "300", "user_id": "5", "channel_id": null, "session_id": "x", "deaf": true,
        "member": {"user": {"id": "5", "username": "kai"}, "nick": "K", "roles": []}
    });
    let Some(CoreEvent::VoiceStateUpdate { guild_id, state, member }) = decode_dispatch("VOICE_STATE_UPDATE", d) else { panic!() };
    assert_eq!(guild_id, Id(300));
    assert_eq!(state.user_id, Id(5));
    assert_eq!(state.channel_id, None);
    assert!(state.is_deafened());
    assert_eq!(member.unwrap().nick.as_deref(), Some("K"));
    // A private call has no server: it comes as a DM or group call's voice state.
    let private = decode_dispatch("VOICE_STATE_UPDATE", json!({"user_id": "5", "channel_id": "9"}));
    assert!(matches!(private, Some(CoreEvent::PrivateVoiceState(v)) if v.channel_id == Some(Id(9))));
}
