//! Calls in DMs and group chats. Discord sends CALL_CREATE when one starts (and for
//! each one under way after READY), CALL_UPDATE as who's being rung changes, and
//! CALL_DELETE when the last person leaves. Who's in it comes as VOICE_STATE_UPDATE
//! without a server.

use serde::Deserialize;

use crate::Id;
use crate::model::{VoiceState, lenient_vec};

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Call {
    pub channel_id: Id,
    /// The "started a call" message in the chat.
    #[serde(default)]
    pub message_id: Option<Id>,
    /// Who's being rung right now.
    #[serde(default)]
    pub ringing: Vec<Id>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub voice_states: Vec<VoiceState>,
}

impl Call {
    /// Who's connected, in the order they joined as far as we know.
    pub fn participants(&self) -> impl Iterator<Item = Id> + '_ {
        self.voice_states.iter().filter(|v| v.channel_id == Some(self.channel_id)).map(|v| v.user_id)
    }

    /// Takes in someone joining, leaving or changing how they're connected.
    /// Whether they're in this call now.
    pub fn apply_state(&mut self, state: &VoiceState) -> bool {
        let inside = state.channel_id == Some(self.channel_id);
        match self.voice_states.iter().position(|v| v.user_id == state.user_id) {
            Some(i) if inside => self.voice_states[i] = state.clone(),
            Some(i) => {
                self.voice_states.remove(i);
            }
            None if inside => self.voice_states.push(state.clone()),
            None => {}
        }
        // Joining stops their ringing.
        if inside {
            self.ringing.retain(|u| *u != state.user_id);
        }
        inside
    }
}

/// What kind of conversation a call is in, for whether it may ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPlace {
    /// A one-on-one DM; `friend`: the other person is a friend.
    Dm { friend: bool },
    /// A group chat; `muted`: we muted it.
    Group { muted: bool },
}

/// Whether an incoming call shows its ringing panel. Discord already decides who
/// it rings; on top of that a DM rings only from a friend, and a group only while
/// it isn't muted. Nothing rings for a call we're already in, or one we started.
pub fn should_ring(place: CallPlace, rung: bool, in_it: bool) -> bool {
    if !rung || in_it {
        return false;
    }
    match place {
        CallPlace::Dm { friend } => friend,
        CallPlace::Group { muted } => !muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(user: u64, channel: Option<u64>) -> VoiceState {
        VoiceState {
            guild_id: None,
            user_id: Id(user),
            channel_id: channel.map(Id),
            self_mute: false,
            self_deaf: false,
            self_video: false,
            self_stream: false,
            session_id: None,
            mute: false,
            deaf: false,
        }
    }

    #[test]
    fn dms_ring_only_from_friends_and_groups_only_unmuted() {
        assert!(should_ring(CallPlace::Dm { friend: true }, true, false));
        assert!(!should_ring(CallPlace::Dm { friend: false }, true, false), "not a friend");
        assert!(should_ring(CallPlace::Group { muted: false }, true, false));
        assert!(!should_ring(CallPlace::Group { muted: true }, true, false), "muted group");
        assert!(!should_ring(CallPlace::Dm { friend: true }, false, false), "Discord isn't ringing us");
        assert!(!should_ring(CallPlace::Group { muted: false }, true, true), "already in it");
    }

    #[test]
    fn people_join_and_leave_a_call() {
        let mut call = Call { channel_id: Id(9), ringing: vec![Id(2), Id(3)], ..Default::default() };
        assert!(call.apply_state(&state(2, Some(9))));
        assert_eq!(call.participants().collect::<Vec<_>>(), vec![Id(2)]);
        assert_eq!(call.ringing, vec![Id(3)], "joining stops their ring");
        assert!(call.apply_state(&state(2, Some(9))), "a change keeps them once");
        assert_eq!(call.voice_states.len(), 1);
        assert!(!call.apply_state(&state(2, None)));
        assert_eq!(call.participants().count(), 0);
        assert!(!call.apply_state(&state(4, Some(10))), "someone else's call");
        assert!(call.voice_states.is_empty());
    }

    #[test]
    fn decodes_a_call_create() {
        let call: Call = serde_json::from_value(serde_json::json!({
            "channel_id": "9", "message_id": "11", "region": "rotterdam", "ringing": ["3"],
            "voice_states": [{"user_id": "2", "channel_id": "9", "session_id": "s"}, {"bad": true}]
        }))
        .unwrap();
        assert_eq!(call.ringing, vec![Id(3)]);
        assert_eq!(call.participants().collect::<Vec<_>>(), vec![Id(2)]);
    }
}
