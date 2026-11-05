//! The app's one handle on Discord: REST, the gateway session, login and the
//! saved token. Async methods run on cord-core's tokio runtime, so callers can
//! await them from any executor, GPUI's included.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::Id;
use crate::auth::remote::{self, REMOTE_AUTH_URL, RemoteAuth};
use crate::auth::token_store::{TokenStore, TokenStoreError};
use crate::auth::{self, AuthError, CaptchaSolution, LoginOutcome, MfaMethod};
use crate::build_number;
use crate::event::CoreEvent;
use crate::gateway::{self, GATEWAY_URL, GatewayConfig, GatewayHandle};
use crate::http::{Http, HttpError};
use crate::identity::Identity;
use crate::model::{Channel, Message, ReactionEmoji};
use crate::profile::UserProfile;

pub const API_BASE: &str = "https://discord.com/api/v9";

#[derive(Clone)]
pub struct CoreConfig {
    pub api_base: String,
    pub gateway_url: String,
    pub remote_auth_url: String,
    /// Where the last READY is kept, to show the app at once on the next start,
    /// for `READY_TTL`. None: not at all.
    pub ready_file: Option<std::path::PathBuf>,
    /// Where profiles are kept between looks, for `PROFILE_TTL`. None: not at all.
    pub profile_dir: Option<std::path::PathBuf>,
}

/// How long a profile on disk is good for: the same person looked at again
/// within this costs Discord no request, across restarts too.
const PROFILE_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// How old a kept READY may be and still be shown while the real one arrives.
/// Mere seconds of showing it: the real one then replaces all of it, so a
/// channel made or deleted meanwhile is put right at once.
const READY_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

impl Default for CoreConfig {
    fn default() -> Self {
        CoreConfig {
            api_base: API_BASE.into(),
            gateway_url: GATEWAY_URL.into(),
            remote_auth_url: REMOTE_AUTH_URL.into(),
            profile_dir: None,
            ready_file: None,
        }
    }
}

struct Session {
    gateway: GatewayHandle,
    events: mpsc::UnboundedSender<CoreEvent>,
}

pub struct Core {
    http: Http,
    rt: Handle,
    config: CoreConfig,
    tokens: Arc<dyn TokenStore>,
    session: Mutex<Option<Session>>,
    /// Profiles fetched and not yet forgotten, so each is asked for once.
    profiles: Mutex<HashMap<(Id, Option<Id>), Arc<UserProfile>>>,
    /// Profile frames' artwork by shop item; small, and it never changes.
    frames: Mutex<HashMap<String, Option<crate::profile::ProfileFrame>>>,
    effects: Mutex<HashMap<String, Option<crate::profile::ProfileEffect>>>,
}

impl Core {
    pub fn new(identity: Identity, config: CoreConfig, tokens: Arc<dyn TokenStore>, rt: Handle) -> Arc<Core> {
        // reqwest and tungstenite both use rustls; pick its crypto once.
        let _ = rustls::crypto::ring::default_provider().install_default();
        if let Some(path) = &config.ready_file {
            let _ = crate::event::READY_FILE.set(path.clone());
        }
        Arc::new(Core {
            http: Http::new(identity, &config.api_base),
            rt,
            config,
            tokens,
            session: Mutex::new(None),
            profiles: Mutex::default(),
            frames: Mutex::default(),
            effects: Mutex::default(),
        })
    }

    pub fn runtime(&self) -> &Handle {
        &self.rt
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    pub fn saved_token(&self) -> Option<String> {
        self.tokens.load()
    }

    pub fn save_token(&self, token: &str) -> Result<(), TokenStoreError> {
        self.tokens.save(token)
    }

    pub fn start(&self, token: String) -> mpsc::UnboundedReceiver<CoreEvent> {
        self.http.set_token(Some(token.clone()));
        self.profiles.lock().unwrap().clear();
        let (tx, rx) = mpsc::unbounded_channel();
        let _guard = self.rt.enter();
        let config = GatewayConfig::new(self.config.gateway_url.clone(), token, self.http.identity().clone());
        let gateway = gateway::spawn(config, tx.clone());
        *self.session.lock().unwrap() = Some(Session { gateway, events: tx });
        rx
    }

    pub fn sign_out(&self) {
        // Someone else's account shouldn't read this one's view of people.
        if let Some(path) = &self.config.ready_file {
            let _ = std::fs::remove_file(path);
        }
        if let Some(dir) = &self.config.profile_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        self.session.lock().unwrap().take();
        self.profiles.lock().unwrap().clear();
        self.http.set_token(None);
        self.tokens.clear();
    }

    /// Sends a raw payload on the gateway; it waits for the session to be ready.
    /// Without a session it is dropped.
    pub fn send_gateway(&self, payload: Value) {
        if let Some(s) = self.session.lock().unwrap().as_ref() {
            s.gateway.send(payload);
        }
    }

    /// Asks the gateway for up to 10 members of `guild` whose names start
    /// with `query`; they arrive as `MembersChunk`.
    pub fn search_members(&self, guild: Id, query: &str) {
        self.send_gateway(serde_json::json!({
            "op": 8,
            "d": { "guild_id": [guild.to_string()], "query": query, "limit": 10, "presences": false }
        }));
    }

    /// Asks the gateway for these members of `guild` (they arrive as
    /// `MembersChunk`): messages fetched over REST don't say who has what role.
    pub fn request_members(&self, guild: Id, users: &[Id]) {
        for ids in users.chunks(100) {
            let ids: Vec<String> = ids.iter().map(Id::to_string).collect();
            self.send_gateway(serde_json::json!({
                "op": 8,
                "d": { "guild_id": [guild.to_string()], "user_ids": ids, "presences": false }
            }));
        }
    }

    async fn on_rt<T: Send + 'static>(&self, fut: impl Future<Output = T> + Send + 'static) -> T {
        self.rt.spawn(fut).await.expect("cord-core task panicked")
    }

    /// A rejected token ends the session the same way a gateway 4004 does.
    fn watch<T>(&self, result: Result<T, HttpError>) -> Result<T, HttpError> {
        if matches!(result, Err(HttpError::Unauthorized)) {
            if let Some(s) = self.session.lock().unwrap().as_ref() {
                let _ = s.events.send(CoreEvent::AuthFailed);
            }
        }
        result
    }

    pub async fn messages(&self, channel: Id, limit: u8) -> Result<Vec<Message>, HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move { http.get(&format!("/channels/{channel}/messages?limit={limit}")).await })
            .await;
        self.watch(result)
    }

    /// The page of messages just before `before`, newest first.
    pub async fn messages_before(&self, channel: Id, before: Id, limit: u8) -> Result<Vec<Message>, HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move { http.get(&format!("/channels/{channel}/messages?before={before}&limit={limit}")).await })
            .await;
        self.watch(result)
    }

    /// Sends a message, as a reply to `reply` when given.
    pub async fn send_message(
        &self,
        channel: Id,
        content: String,
        nonce: String,
        reply: Option<(Id, bool)>,
        files: Vec<crate::http::Upload>,
    ) -> Result<Message, HttpError> {
        let http = self.http.clone();
        let mut body = json!({"content": content, "nonce": nonce, "enforce_nonce": true, "tts": false, "flags": 0});
        // `.1`: whether the reply pings who it answers.
        if let Some((reply, ping)) = reply {
            body["message_reference"] = json!({"channel_id": channel.to_string(), "message_id": reply.to_string()});
            body["allowed_mentions"] = json!({"parse": ["users", "roles", "everyone"], "replied_user": ping});
        }
        let result = self
            .on_rt(async move { http.post_files(&format!("/channels/{channel}/messages"), &body, &files).await })
            .await;
        self.watch(result)
    }

    pub async fn edit_message(&self, channel: Id, message: Id, content: String) -> Result<(), HttpError> {
        let http = self.http.clone();
        let body = json!({"content": content});
        let result = self
            .on_rt(async move {
                let _: Value = http.patch(&format!("/channels/{channel}/messages/{message}"), &body).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    pub async fn delete_message(&self, channel: Id, message: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.delete(&format!("/channels/{channel}/messages/{message}")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Server mute, server deafen, or moving someone (`channel_id`, null to disconnect).
    pub async fn update_member_voice(&self, guild: Id, user: Id, change: Value) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.patch(&format!("/guilds/{guild}/members/{user}"), &change).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    pub async fn kick_member(&self, guild: Id, user: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.delete(&format!("/guilds/{guild}/members/{user}")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Where a custom emoji's from. An error is what a private server looks like.
    pub async fn emoji_source(&self, emoji: Id) -> Result<Value, HttpError> {
        let http = self.http.clone();
        let result = self.on_rt(async move { http.get(&format!("/emojis/{emoji}/source")).await }).await;
        self.watch(result)
    }

    /// Where a sticker's from, likewise.
    pub async fn sticker_source(&self, sticker: Id) -> Result<Value, HttpError> {
        let http = self.http.clone();
        let result = self.on_rt(async move { http.get(&format!("/stickers/{sticker}/guild")).await }).await;
        self.watch(result)
    }

    /// Adds someone to a group chat.
    pub async fn add_recipient(&self, channel: Id, user: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.put(&format!("/channels/{channel}/recipients/{user}")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    pub async fn unpin_message(&self, channel: Id, message: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.delete(&format!("/channels/{channel}/pins/{message}")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    pub async fn pin_message(&self, channel: Id, message: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.put(&format!("/channels/{channel}/pins/{message}")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Marks `channel` read up to `message`, as the official client does when
    /// a channel is viewed. Other sessions hear of it through MESSAGE_ACK.
    pub async fn ack(&self, channel: Id, message: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let body = json!({"token": null});
        let result = self
            .on_rt(async move {
                let _: Value = http.post(&format!("/channels/{channel}/messages/{message}/ack"), &body).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Marks many channels read at once, as (channel, newest message).
    pub async fn ack_bulk(&self, channels: Vec<(Id, Id)>) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                for part in channels.chunks(100) {
                    let states: Vec<Value> = part
                        .iter()
                        .map(|(channel, message)| json!({"channel_id": channel.to_string(), "message_id": message.to_string(), "read_state_type": 0}))
                        .collect();
                    let _: Value = http.post("/read-states/ack-bulk", &json!({"read_states": states})).await?;
                }
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Adds (`on`) or removes the signed-in user's reaction to a message.
    pub async fn set_reaction(&self, channel: Id, message: Id, emoji: &ReactionEmoji, on: bool) -> Result<(), HttpError> {
        let http = self.http.clone();
        let path = format!("/channels/{channel}/messages/{message}/reactions/{}/@me", emoji.path_segment());
        let result = self
            .on_rt(async move {
                let _: Value = if on { http.put(&path).await? } else { http.delete(&path).await? };
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// The kept READY as an event, if there's a young enough one: servers and
    /// channels to show until the gateway's real READY replaces them.
    pub fn kept_ready(&self) -> Option<crate::event::CoreEvent> {
        let path = self.config.ready_file.as_ref()?;
        let age = std::fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()?;
        if age > READY_TTL {
            let _ = std::fs::remove_file(path);
            return None;
        }
        let ready = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        Some(crate::event::CoreEvent::Ready(Box::new(ready)))
    }

    /// `user`'s profile if it's at hand, without asking anyone.
    pub fn cached_profile(&self, user: Id, guild: Option<Id>) -> Option<Arc<UserProfile>> {
        if let Some(profile) = self.profiles.lock().unwrap().get(&(user, guild)) {
            return Some(profile.clone());
        }
        let profile = Arc::new(self.profile_from_disk(user, guild)?);
        self.profiles.lock().unwrap().insert((user, guild), profile.clone());
        Some(profile)
    }

    fn profile_file(&self, user: Id, guild: Option<Id>) -> Option<std::path::PathBuf> {
        let place = guild.map_or_else(|| "me".to_string(), |g| g.to_string());
        Some(self.config.profile_dir.as_ref()?.join(format!("{user}_{place}.json")))
    }

    /// The profile on disk, if it's young enough.
    fn profile_from_disk(&self, user: Id, guild: Option<Id>) -> Option<UserProfile> {
        let path = self.profile_file(user, guild)?;
        let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
        if age > PROFILE_TTL {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let mut profile: UserProfile = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        profile.seen_from(guild);
        Some(profile)
    }

    fn profile_to_disk(&self, user: Id, guild: Option<Id>, raw: &Value) {
        let Some(path) = self.profile_file(user, guild) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(&path, raw.to_string()) {
            tracing::warn!(%error, "couldn't keep a profile");
        }
    }

    /// Lets go of `user`'s profile; the next look fetches it again.
    pub fn forget_profile(&self, user: Id, guild: Option<Id>) {
        self.profiles.lock().unwrap().remove(&(user, guild));
    }

    /// A profile frame's artwork if it's at hand.
    pub fn cached_frame(&self, sku: &str) -> Option<crate::profile::ProfileFrame> {
        self.frames.lock().unwrap().get(sku).cloned().flatten()
    }

    /// `user`'s profile, as seen from `guild` when given. Fetched once per
    /// session; later calls answer from memory.
    pub async fn user_profile(&self, user: Id, guild: Option<Id>) -> Result<Arc<UserProfile>, HttpError> {
        if let Some(p) = self.cached_profile(user, guild) {
            return Ok(p);
        }
        self.fetch_profile(user, guild).await
    }

    /// The profile as it is now: opening someone's card shows the kept one at
    /// once, and this replaces it, so a change they made shows straight away.
    pub async fn fetch_profile(&self, user: Id, guild: Option<Id>) -> Result<Arc<UserProfile>, HttpError> {
        let http = self.http.clone();
        let mut path = format!("/users/{user}/profile?with_mutual_guilds=true&with_mutual_friends=true&with_mutual_friends_count=true");
        if let Some(guild) = guild {
            path.push_str(&format!("&guild_id={guild}"));
        }
        let result = self.on_rt(async move { http.get::<Value>(&path).await }).await;
        let raw = self.watch(result)?;
        log_cosmetics(user, &raw);
        self.profile_to_disk(user, guild, &raw);
        let mut profile: UserProfile = serde_json::from_value(raw).map_err(|e| HttpError::Decode(e.to_string()))?;
        profile.seen_from(guild);
        let profile = Arc::new(profile);
        self.profiles.lock().unwrap().insert((user, guild), profile.clone());
        Ok(profile)
    }

    /// What an invite code leads to, with member counts.
    pub async fn invite(&self, code: String) -> Result<crate::invite::Invite, HttpError> {
        let http = self.http.clone();
        let path = format!("/invites/{code}?with_counts=true&with_expiration=true");
        let result = self.on_rt(async move { http.get(&path).await }).await;
        self.watch(result)
    }

    /// The artwork of the profile frame sold as `sku`, if that's what it is.
    /// A profile effect's pictures and timings; remembered once fetched.
    pub async fn profile_effect(&self, sku: String) -> Result<Option<crate::profile::ProfileEffect>, HttpError> {
        if let Some(effect) = self.effects.lock().unwrap().get(&sku) {
            return Ok(effect.clone());
        }
        let http = self.http.clone();
        let path = format!("/collectibles-products/{sku}");
        let result = self.on_rt(async move { http.get::<Value>(&path).await }).await;
        let effect = crate::profile::effect_from_product(self.watch(result)?);
        self.effects.lock().unwrap().insert(sku, effect.clone());
        Ok(effect)
    }

    pub async fn profile_frame(&self, sku: String) -> Result<Option<crate::profile::ProfileFrame>, HttpError> {
        if let Some(frame) = self.frames.lock().unwrap().get(&sku) {
            return Ok(frame.clone());
        }
        let http = self.http.clone();
        let path = format!("/collectibles-products/{sku}");
        let result = self.on_rt(async move { http.get::<Value>(&path).await }).await;
        let frame = crate::profile::frame_from_product(self.watch(result)?);
        self.frames.lock().unwrap().insert(sku, frame.clone());
        Ok(frame)
    }

    /// The first `limit` people to react with `emoji`.
    pub async fn reaction_users(&self, channel: Id, message: Id, emoji: &ReactionEmoji, limit: u8) -> Result<Vec<crate::model::User>, HttpError> {
        let http = self.http.clone();
        let path = format!("/channels/{channel}/messages/{message}/reactions/{}?limit={limit}&type=0", emoji.path_segment());
        let result = self.on_rt(async move { http.get(&path).await }).await;
        self.watch(result)
    }

    /// Asks for a server's voice channel statuses and start times; they
    /// arrive as `ChannelInfo`. The official client does this on opening one.
    pub fn request_channel_info(&self, guild: Id) {
        self.send_gateway(json!({"op": 43, "d": {"guild_id": guild.to_string(), "fields": ["status", "voice_start_time"]}}));
    }

    /// A built-in sticker's Lottie animation, as JSON. A public file on the
    /// CDN: no token goes with it.
    pub async fn sticker_lottie(&self, id: Id) -> Result<String, reqwest::Error> {
        let url = format!("{}/stickers/{id}.json", crate::cdn::CDN);
        self.on_rt(async move { reqwest::get(url).await?.error_for_status()?.text().await }).await
    }

    /// Takes a DM off the list. Nothing is deleted; it comes back with the next message.
    pub async fn close_dm(&self, channel: Id) -> Result<(), HttpError> {
        let http = self.http.clone();
        let result = self
            .on_rt(async move {
                let _: Value = http.delete(&format!("/channels/{channel}?silent=false")).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// Makes `answers` our votes on a poll; none takes the vote back.
    pub async fn vote_poll(&self, channel: Id, message: Id, answers: Vec<u32>) -> Result<(), HttpError> {
        let http = self.http.clone();
        let path = format!("/channels/{channel}/polls/{message}/answers/@me");
        let body = json!({"answer_ids": answers.iter().map(u32::to_string).collect::<Vec<_>>()});
        let result = self
            .on_rt(async move {
                let _: Value = http.put_json(&path, &body).await?;
                Ok(())
            })
            .await;
        self.watch(result)
    }

    /// A public CDN file, fetched with no account attached.
    pub async fn cdn_bytes(&self, url: String) -> Result<Vec<u8>, reqwest::Error> {
        self.on_rt(async move { reqwest::get(url).await?.error_for_status()?.bytes().await.map(|b| b.to_vec()) }).await
    }

    /// Opens (or finds) the direct message channel with `user`.
    pub async fn open_dm(&self, user: Id) -> Result<Channel, HttpError> {
        let http = self.http.clone();
        let body = json!({"recipient_id": user.to_string()});
        let result = self.on_rt(async move { http.post("/users/@me/channels", &body).await }).await;
        self.watch(result)
    }

    pub async fn login(
        &self,
        login: String,
        password: String,
        captcha: Option<CaptchaSolution>,
    ) -> Result<LoginOutcome, AuthError> {
        let http = self.http.clone();
        self.on_rt(async move { auth::login(&http, &login, &password, captcha.as_ref()).await }).await
    }

    pub async fn submit_mfa(&self, ticket: String, method: MfaMethod, code: String) -> Result<String, AuthError> {
        let http = self.http.clone();
        self.on_rt(async move { auth::submit_mfa(&http, &ticket, method, &code).await }).await
    }

    pub fn remote_auth(&self) -> RemoteAuth {
        let _guard = self.rt.enter();
        remote::start(&self.config.remote_auth_url, self.http.clone())
    }
}

/// The client identity for this run. A build number fetched in the last day
/// is reused, so a normal start-up makes no extra requests.
/// Logs which cosmetic fields a profile came with (profile frames aren't
/// decoded yet: this is how to find out what Discord calls them).
fn log_cosmetics(user: Id, raw: &Value) {
    let interesting = |key: &str| ["effect", "frame", "collectible", "decoration", "theme"].iter().any(|w| key.contains(w));
    let mut found = Vec::new();
    for (section, value) in [("", raw), ("user", &raw["user"]), ("user_profile", &raw["user_profile"]), ("guild_member_profile", &raw["guild_member_profile"])] {
        for (key, v) in value.as_object().into_iter().flatten().filter(|(k, _)| interesting(k)) {
            if !v.is_null() {
                found.push(format!("{section}.{key}={v}"));
            }
        }
    }
    tracing::info!(%user, cosmetics = found.join(" | "), "profile fetched");
}

pub async fn startup_identity(cache_dir: &Path) -> Identity {
    let file = cache_dir.join("build_number");
    let cached: Option<u32> = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse().ok());
    let fresh = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(24 * 3600));
    if let (Some(n), true) = (cached, fresh) {
        return Identity::new(n);
    }
    let _ = std::fs::create_dir_all(cache_dir);
    let client = reqwest::Client::builder()
        .user_agent(Identity::new(0).user_agent())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");
    // An old number still works: start with it, and have a new one for next time.
    // The file's rewritten either way, so a failure isn't retried on every launch.
    if let Some(n) = cached {
        tokio::spawn(async move {
            let latest = build_number::fetch(&client).await.unwrap_or(n);
            let _ = std::fs::write(&file, latest.to_string());
        });
        return Identity::new(n);
    }
    Identity::new(build_number::resolve(&client, &file).await)
}
