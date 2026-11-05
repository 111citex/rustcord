//! Server invites: finding `discord.gg/…` codes in a message, and what
//! `GET /invites/{code}?with_counts=true` says about one.

use serde::Deserialize;

use crate::Id;
use crate::cdn;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Invite {
    pub code: String,
    #[serde(default)]
    pub guild: Option<InviteGuild>,
    #[serde(default)]
    pub approximate_member_count: Option<u64>,
    #[serde(default)]
    pub approximate_presence_count: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct InviteGuild {
    pub id: Id,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub banner: Option<String>,
    #[serde(default)]
    pub splash: Option<String>,
}

impl InviteGuild {
    pub fn icon_url(&self, size: u32) -> Option<String> {
        Some(cdn::guild_icon_url(self.id, self.icon.as_deref()?, size))
    }

    /// The server's banner, or its invite splash.
    pub fn banner_url(&self, width: u32) -> Option<String> {
        match (&self.banner, &self.splash) {
            (Some(b), _) => Some(format!("{}/banners/{}/{b}.png?size={width}", cdn::CDN, self.id)),
            (None, Some(s)) => Some(format!("{}/splashes/{}/{s}.png?size={width}", cdn::CDN, self.id)),
            _ => None,
        }
    }
}

/// The invite codes linked in `content`, first appearance order, no repeats.
pub fn invite_codes(content: &str) -> Vec<String> {
    const PREFIXES: [&str; 4] = ["discord.gg/", "discord.com/invite/", "discordapp.com/invite/", "discord.gg/invite/"];
    let mut codes: Vec<String> = Vec::new();
    for (i, _) in content.char_indices() {
        let rest = &content[i..];
        let Some(prefix) = PREFIXES.iter().find(|p| rest.starts_with(**p)) else { continue };
        // Only at a word start or after a scheme / "www.".
        let before = &content[..i];
        let ok = before.is_empty()
            || before.ends_with("://")
            || before.ends_with("www.")
            || before.ends_with(|c: char| c.is_whitespace() || matches!(c, '<' | '(' | '['));
        if !ok {
            continue;
        }
        let code: String = rest[prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        if code.len() >= 2 && code != "invite" && !codes.contains(&code) {
            codes.push(code);
        }
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_invite_codes() {
        assert_eq!(invite_codes("join https://discord.gg/abc123 now"), vec!["abc123"]);
        assert_eq!(invite_codes("<https://discord.com/invite/Xy-z> and discord.gg/abc123"), vec!["Xy-z", "abc123"]);
        assert_eq!(invite_codes("notdiscord.gg/abc"), Vec::<String>::new());
        assert_eq!(invite_codes("discord.gg/a discord.gg/aa discord.gg/aa"), vec!["aa"]);
    }

    #[test]
    fn reads_an_invite() {
        let json = r#"{"code":"abc","guild":{"id":"5","name":"pzs","icon":"h","banner":null,"splash":null},
            "approximate_member_count":120,"approximate_presence_count":30}"#;
        let invite: Invite = serde_json::from_str(json).unwrap();
        assert_eq!(invite.guild.as_ref().unwrap().name, "pzs");
        assert_eq!(invite.approximate_presence_count, Some(30));
        assert!(invite.guild.unwrap().icon_url(64).unwrap().contains("/icons/5/h.png"));
    }
}
