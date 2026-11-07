//! The voice gateway and the UDP socket: joining, keeping DAVE's keys up to date, and
//! turning packets into audio to play.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use davey::{DaveSession, MediaType, ProposalsOperationType};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio_tungstenite::tungstenite::Message;

use crate::capture::Outgoing;
use crate::playback::Speakers;
use crate::sounds::Sound;
use crate::transport::{self, Mode};
use crate::video::{self, Pictures};
use crate::{JoinInfo, State, Stats};

const GATEWAY_VERSION: u8 = 8;
/// Opus for silence. Sent as is: there's nothing in it to protect.
const SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// What ends a packet DAVE has encrypted.
const DAVE_MARKER: [u8; 2] = [0xFA, 0xFA];
const UDP_KEEPALIVE: Duration = Duration::from_secs(5);
const HANDSHAKE: Duration = Duration::from_secs(10);
const MOST_RETRIES: u32 = 5;

/// Where packets end up, logged every few seconds: which step audio stops at.
#[derive(Default)]
struct Counts {
    received: u64,
    unreadable: u64,
    plain: u64,
    decrypted: u64,
    unknown_speaker: u64,
    not_ready: u64,
    failed: u64,
    speaking_events: u64,
}

/// Why a connection ended.
enum End {
    Stopped,
    /// Removed from the call, or it's gone: not to be retried.
    Final(String),
    Retry(String),
}

/// Whether a close code from the voice gateway means trying again is pointless.
fn close_is_final(code: u16) -> bool {
    matches!(code, 4004 | 4006 | 4009 | 4011 | 4014 | 4016 | 4017 | 4021 | 4022)
}

/// The DAVE side of a call.
struct Dave {
    session: Option<DaveSession>,
    version: u16,
    pending: HashMap<u16, u16>,
    users: HashSet<u64>,
    user: u64,
    channel: u64,
}

/// What DAVE wants sent back on the gateway.
enum Reply {
    Json(Value),
    Binary(Vec<u8>),
}

impl Dave {
    fn new(user: u64, channel: u64) -> Self {
        Dave { session: None, version: 0, pending: HashMap::new(), users: HashSet::from([user]), user, channel }
    }

    /// Starts over at `self.version`, offering a new key package.
    fn reinit(&mut self, out: &mut Vec<Reply>) {
        let Some(version) = NonZeroU16::new(self.version) else {
            if let Some(session) = &mut self.session {
                let _ = session.reset();
                session.set_passthrough_mode(true, Some(10));
            }
            return;
        };
        let package = match &mut self.session {
            Some(session) => session.reinit(version, self.user, self.channel, None).map_err(|e| e.to_string()).and_then(|()| session.create_key_package().map_err(|e| e.to_string())),
            None => DaveSession::new(version, self.user, self.channel, None).map_err(|e| e.to_string()).and_then(|mut session| {
                let package = session.create_key_package().map_err(|e| e.to_string());
                self.session = Some(session);
                package
            }),
        };
        match package {
            Ok(package) => {
                let mut message = vec![26];
                message.extend_from_slice(&package);
                out.push(Reply::Binary(message));
            }
            Err(error) => tracing::warn!(%error, "voice: couldn't start DAVE"),
        }
    }

    fn execute(&mut self, transition: u16) {
        if let Some(session) = &mut self.session
            && let Err(error) = session.execute_transition(transition)
        {
            tracing::warn!(%error, "voice: DAVE transition");
        }
        let Some(version) = self.pending.remove(&transition) else { return };
        let old = std::mem::replace(&mut self.version, version);
        if transition > 0 && old == 0 && version != 0
            && let Some(session) = &mut self.session
        {
            session.set_passthrough_mode(true, Some(10));
        }
    }

    /// After a commit or a welcome: either ready for the transition, or start over.
    fn settled<E: std::fmt::Display>(&mut self, transition: u16, result: Option<Result<(), E>>, out: &mut Vec<Reply>) {
        match result {
            Some(Ok(())) => {
                // This is what makes the keys of the group just joined usable.
                let version = self.version;
                if let Some(session) = &mut self.session
                    && let Err(error) = session.prepare_transition(transition, version)
                {
                    tracing::warn!(%error, "voice: DAVE transition");
                }
                if transition != 0 {
                    self.pending.insert(transition, version);
                    out.push(Reply::Json(json!({"op": 23, "d": {"transition_id": transition}})));
                }
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "voice: DAVE group trouble, rejoining it");
                out.push(Reply::Json(json!({"op": 31, "d": {"transition_id": transition}})));
                self.reinit(out);
            }
            _ => {}
        }
    }

    fn on_json(&mut self, op: u64, d: &Value, out: &mut Vec<Reply>) {
        let transition = d["transition_id"].as_u64().unwrap_or(0) as u16;
        let version = d["protocol_version"].as_u64().unwrap_or(0) as u16;
        match op {
            21 => {
                self.pending.insert(transition, version);
                if let Some(session) = &mut self.session
                    && let Err(error) = session.prepare_transition(transition, version)
                {
                    tracing::warn!(%error, "voice: DAVE transition");
                }
                if transition == 0 {
                    self.execute(0);
                } else if version == 0 {
                    if let Some(session) = &mut self.session {
                        session.set_passthrough_mode(true, Some(120));
                    }
                    out.push(Reply::Json(json!({"op": 23, "d": {"transition_id": transition}})));
                }
            }
            22 => self.execute(transition),
            24 if d["epoch"].as_u64() == Some(1) => {
                self.version = version;
                self.reinit(out);
            }
            _ => {}
        }
    }

    fn on_binary(&mut self, op: u8, body: &[u8], out: &mut Vec<Reply>) {
        match op {
            25 => {
                if let Some(session) = &mut self.session
                    && let Err(error) = session.set_external_sender(body)
                {
                    tracing::warn!(%error, "voice: DAVE external sender");
                }
            }
            27 => {
                let Some((&kind, proposals)) = body.split_first() else { return };
                let kind = if kind == 0 { ProposalsOperationType::APPEND } else { ProposalsOperationType::REVOKE };
                let users: Vec<u64> = self.users.iter().copied().collect();
                let Some(session) = &mut self.session else { return };
                match session.process_proposals(kind, proposals, Some(&users)) {
                    Ok(Some(result)) => {
                        let mut message = vec![28];
                        message.extend_from_slice(&result.commit);
                        if let Some(welcome) = &result.welcome {
                            message.extend_from_slice(welcome);
                        }
                        out.push(Reply::Binary(message));
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "voice: DAVE proposals"),
                }
            }
            29 | 30 => {
                if body.len() < 2 {
                    return;
                }
                let transition = u16::from_be_bytes([body[0], body[1]]);
                let rest = &body[2..];
                if op == 29 {
                    let result = self.session.as_mut().map(|s| s.process_commit(rest).map(|_| ()).map_err(|e| e.to_string()));
                    self.settled(transition, result, out);
                } else {
                    let users: Vec<u64> = self.users.iter().copied().collect();
                    let result = self.session.as_mut().map(|s| s.process_welcome(rest, Some(&users)).map(|_| ()).map_err(|e| e.to_string()));
                    self.settled(transition, result, out);
                }
            }
            _ => {}
        }
    }

    /// Puts DAVE's layer on an H.264 frame of ours, as `seal` does audio.
    fn seal_video(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        if self.version == 0 {
            return Some(frame.to_vec());
        }
        let session = self.session.as_mut().filter(|s| s.is_ready())?;
        match session.encrypt(MediaType::VIDEO, davey::Codec::H264, frame) {
            Ok(sealed) => Some(sealed.into_owned()),
            Err(error) => {
                tracing::debug!(%error, "voice: couldn't encrypt our video");
                None
            }
        }
    }

    /// Puts DAVE's layer on a frame of ours. `None`: it can't go out yet, the group's keys
    /// not being ready, and sending it as is would be refused by the others.
    fn seal(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        if self.version == 0 {
            return Some(frame.to_vec());
        }
        let session = self.session.as_mut().filter(|s| s.is_ready())?;
        match session.encrypt_opus(frame) {
            Ok(sealed) => Some(sealed.into_owned()),
            Err(error) => {
                tracing::debug!(%error, "voice: couldn't encrypt our audio");
                None
            }
        }
    }

    /// Takes off DAVE's layer, if the packet has it. `None`: can't be played.
    fn open(&mut self, user: Option<u64>, payload: Vec<u8>, media: MediaType, counts: &mut Counts) -> Option<Vec<u8>> {
        if self.version == 0 || payload.len() < 11 || payload[payload.len() - 2..] != DAVE_MARKER {
            counts.plain += 1;
            return Some(payload);
        }
        let Some(session) = self.session.as_mut().filter(|s| s.is_ready()) else {
            counts.not_ready += 1;
            return None;
        };
        let Some(user) = user else {
            counts.unknown_speaker += 1;
            return None;
        };
        match session.decrypt(user, media, &payload) {
            Ok(audio) => {
                counts.decrypted += 1;
                Some(audio)
            }
            Err(error) => {
                if counts.failed == 0 {
                    tracing::warn!(%error, "voice: couldn't decrypt audio");
                }
                counts.failed += 1;
                None
            }
        }
    }
}

/// Where our own audio is up to.
struct Sending {
    seq: u16,
    timestamp: u32,
    /// The nonce: never the same twice for one key.
    counter: u32,
    speaking: bool,
}

impl Sending {
    /// Seals the next 20 ms of ours.
    fn next(&mut self, mode: Mode, key: &[u8; 32], ssrc: u32, payload: &[u8]) -> Option<Vec<u8>> {
        let packet = transport::seal_packet(mode, key, self.seq, self.timestamp, ssrc, self.counter, payload);
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(960);
        self.counter = self.counter.wrapping_add(1);
        packet
    }
}

/// Hangs on to the call until it's over, trying again if the connection drops.
pub async fn run(info: JoinInfo, speakers: Arc<Mutex<Speakers>>, stats: Arc<Stats>, stop: Arc<tokio::sync::Notify>, mut outgoing: tokio::sync::mpsc::Receiver<Outgoing>, pictures: Arc<Pictures>) {
    let mut failures = 0;
    loop {
        stats.set_state(State::Connecting);
        let began = Instant::now();
        match connect(&info, &speakers, &stats, &stop, &mut outgoing, &pictures).await {
            End::Stopped => break,
            End::Final(why) => {
                tracing::info!(%why, "voice: call ended");
                break;
            }
            End::Retry(why) => {
                failures = if began.elapsed() > Duration::from_secs(30) { 1 } else { failures + 1 };
                tracing::warn!(%why, attempt = failures, "voice: connection lost");
                if failures > MOST_RETRIES {
                    break;
                }
                let wait = Duration::from_secs(1 << failures.min(4));
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = stop.notified() => break,
                }
            }
        }
    }
    stats.set_state(State::Closed);
}

fn gateway_url(endpoint: &str) -> String {
    let host = endpoint.trim_start_matches("wss://").trim_end_matches('/').trim_end_matches(":443");
    format!("wss://{host}/?v={GATEWAY_VERSION}")
}

/// IP discovery: asks the server what address our packets come from.
async fn discover(udp: &UdpSocket, ssrc: u32) -> Result<(String, u16), String> {
    let mut packet = [0u8; 74];
    packet[..2].copy_from_slice(&1u16.to_be_bytes());
    packet[2..4].copy_from_slice(&70u16.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    udp.send(&packet).await.map_err(|e| e.to_string())?;
    let size = tokio::time::timeout(HANDSHAKE, udp.recv(&mut packet)).await.map_err(|_| "no reply to IP discovery")?.map_err(|e| e.to_string())?;
    parse_discovery(&packet[..size]).ok_or_else(|| "bad reply to IP discovery".to_string())
}

fn parse_discovery(packet: &[u8]) -> Option<(String, u16)> {
    if packet.len() < 74 || u16::from_be_bytes([packet[0], packet[1]]) != 2 {
        return None;
    }
    let address = &packet[8..72];
    let end = address.iter().position(|b| *b == 0)?;
    Some((std::str::from_utf8(&address[..end]).ok()?.to_string(), u16::from_be_bytes([packet[72], packet[73]])))
}

/// One connection, from joining until it ends.
async fn connect(info: &JoinInfo, speakers: &Mutex<Speakers>, stats: &Stats, stop: &tokio::sync::Notify, outgoing: &mut tokio::sync::mpsc::Receiver<Outgoing>, pictures: &Arc<Pictures>) -> End {
    let (socket, _) = match tokio::time::timeout(HANDSHAKE, tokio_tungstenite::connect_async(gateway_url(&info.endpoint))).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => return End::Retry(error.to_string()),
        Err(_) => return End::Retry("timed out connecting".into()),
    };
    let (mut ws_tx, mut ws_rx) = socket.split();
    let identify = json!({"op": 0, "d": {
        "server_id": info.server_id.to_string(),
        "channel_id": info.channel_id.to_string(),
        "user_id": info.user_id.to_string(),
        "session_id": info.session_id,
        "token": info.token,
        "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION,
        // We take video; what we'd send is settled later, by Opcode 12.
        "video": true,
        "streams": [{"type": "video", "rid": "100", "quality": 100}],
    }});
    if let Err(error) = ws_tx.send(Message::text(identify.to_string())).await {
        return End::Retry(error.to_string());
    }

    let mut dave = Dave::new(info.user_id, info.channel_id);
    let mut users_by_ssrc: HashMap<u32, u64> = HashMap::new();
    let mut last_seq: i64 = -1;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
    let mut heartbeat_sent: Option<(u64, Instant)> = None;
    let mut keepalive = tokio::time::interval(UDP_KEEPALIVE);
    let mut report = tokio::time::interval(Duration::from_secs(5));
    let mut counts = Counts::default();
    let mut ops_seen = std::collections::BTreeSet::new();
    let mut udp: Option<UdpSocket> = None;
    let mut ssrc = 0u32;
    // Ours for video, from Ready, and where our camera's frames are up to.
    let (mut video_ssrc, mut rtx_ssrc) = (0u32, 0u32);
    let mut video_seq: u16 = 0;
    let mut transport_seq: u16 = 0;
    let mut report_logged: Option<Instant> = None;
    let mut video_on = false;
    let mut mode = Mode::Aes;
    let mut key: Option<[u8; 32]> = None;
    let mut buffer = vec![0u8; 2048];
    let mut connected_at: Option<Instant> = None;
    let started = Instant::now();
    // Video: whose each SSRC is, which SSRC each resend SSRC repeats, and frames in progress.
    let mut video_users: HashMap<u32, u64> = HashMap::new();
    let mut rtx_of: HashMap<u32, u32> = HashMap::new();
    let mut inbound_video: HashMap<u32, video::Inbound> = HashMap::new();
    // Our audio: carries on from the silence sent on connecting.
    let mut sent = Sending { seq: 5, timestamp: 5 * 960, counter: 5, speaking: false };

    loop {
        let mut out: Vec<Reply> = Vec::new();
        tokio::select! {
            () = stop.notified() => {
                let _ = ws_tx.close().await;
                return End::Stopped;
            }
            message = ws_rx.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => return End::Retry(error.to_string()),
                    None => return End::Retry("gateway closed".into()),
                };
                match message {
                    Message::Close(frame) => {
                        let code = frame.as_ref().map_or(1000, |f| u16::from(f.code));
                        let why = format!("closed with {code}");
                        return if close_is_final(code) { End::Final(why) } else { End::Retry(why) };
                    }
                    Message::Binary(data) => {
                        if data.len() >= 3 {
                            ops_seen.insert(u64::from(data[2]));
                            last_seq = i64::from(u16::from_be_bytes([data[0], data[1]]));
                            dave.on_binary(data[2], &data[3..], &mut out);
                        }
                    }
                    Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        if let Some(seq) = value["seq"].as_i64() {
                            last_seq = seq;
                        }
                        let op = value["op"].as_u64().unwrap_or(u64::MAX);
                        ops_seen.insert(op);
                        let d = &value["d"];
                        match op {
                            2 => {
                                ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
                                if let Some(stream) = d["streams"].as_array().and_then(|s| s.first()) {
                                    video_ssrc = stream["ssrc"].as_u64().unwrap_or(0) as u32;
                                    rtx_ssrc = stream["rtx_ssrc"].as_u64().unwrap_or(0) as u32;
                                }
                                let offered: Vec<String> = d["modes"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(str::to_string)).collect();
                                let Some(chosen) = Mode::choose(&offered) else { return End::Final("no encryption mode in common".into()) };
                                mode = chosen;
                                let (Some(ip), Some(port)) = (d["ip"].as_str(), d["port"].as_u64()) else { return End::Retry("incomplete ready".into()) };
                                let socket = match UdpSocket::bind("0.0.0.0:0").await {
                                    Ok(socket) => socket,
                                    Err(error) => return End::Retry(error.to_string()),
                                };
                                if let Err(error) = socket.connect((ip, port as u16)).await {
                                    return End::Retry(error.to_string());
                                }
                                let (address, port) = match discover(&socket, ssrc).await {
                                    Ok(found) => found,
                                    Err(error) => return End::Retry(error),
                                };
                                udp = Some(socket);
                                out.push(Reply::Json(json!({"op": 1, "d": {"protocol": "udp", "data": {"address": address, "port": port, "mode": mode.name()}, "codecs": video::codecs()}})));
                            }
                            4 => {
                                let bytes: Vec<u8> = d["secret_key"].as_array().into_iter().flatten().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
                                let Ok(secret) = <[u8; 32]>::try_from(bytes) else { return End::Retry("bad session description".into()) };
                                key = Some(secret);
                                dave.version = d["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
                                // The codecs the server settled on for the call: what we send has to be these.
                                tracing::info!(audio = %d["audio_codec"], video = %d["video_codec"], "voice: codecs");
                                dave.reinit(&mut out);
                                stats.set_state(State::Connected);
                                connected_at = Some(Instant::now());
                                if !info.viewer
                                    && let Ok(mut speakers) = speakers.lock()
                                {
                                    speakers.play_sound(Sound::Connected);
                                }
                                tracing::info!(dave = dave.version, mode = mode.name(), setup_ms = started.elapsed().as_millis() as u64, "voice: connected");
                                // Discord's docs: this comes first, to tie our SSRC to us.
                                let speaking = json!({"op": 5, "d": {"speaking": 0, "delay": 0, "ssrc": ssrc}});
                                if let Err(error) = ws_tx.send(Message::text(speaking.to_string())).await {
                                    return End::Retry(error.to_string());
                                }
                                // Video's announced (none of ours yet) before any can come, and we
                                // ask for everyone's at full quality.
                                out.push(Reply::Json(json!({"op": 12, "d": {"audio_ssrc": ssrc, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []}})));
                                out.push(Reply::Json(json!({"op": 15, "d": {"any": 100}})));
                                // The server starts sending once it's heard from us. Watching a
                                // stream, we never send media: the keepalive is enough.
                                if let Some(socket) = udp.as_ref().filter(|_| !info.viewer) {
                                    for i in 0..5u16 {
                                        if let Some(packet) = transport::seal_packet(mode, &secret, i, u32::from(i) * 960, ssrc, u32::from(i), &SILENCE) {
                                            let _ = socket.send(&packet).await;
                                        }
                                    }
                                }
                            }
                            5 => {
                                counts.speaking_events += 1;
                                if let (Some(who), Some(user)) = (d["ssrc"].as_u64(), d["user_id"].as_str().and_then(|u| u.parse().ok())) {
                                    users_by_ssrc.insert(who as u32, user);
                                    dave.users.insert(user);
                                    if let Ok(mut speakers) = speakers.lock() {
                                        speakers.set_user(who as u32, user);
                                    }
                                }
                            }
                            12 => {
                                // Someone's video, on or off.
                                if let Some(user) = d["user_id"].as_str().and_then(|u| u.parse::<u64>().ok()) {
                                    // It also says they're here, for DAVE's group.
                                    dave.users.insert(user);
                                    let gone: Vec<u32> = video_users.iter().filter(|(_, u)| **u == user).map(|(s, _)| *s).collect();
                                    for old in gone {
                                        video_users.remove(&old);
                                        inbound_video.remove(&old);
                                        rtx_of.retain(|_, primary| *primary != old);
                                    }
                                    let mut any = false;
                                    for stream in d["streams"].as_array().into_iter().flatten() {
                                        let (Some(primary), true) = (stream["ssrc"].as_u64(), stream["active"].as_bool().unwrap_or(true)) else { continue };
                                        video_users.insert(primary as u32, user);
                                        if let Some(rtx) = stream["rtx_ssrc"].as_u64() {
                                            rtx_of.insert(rtx as u32, primary as u32);
                                        }
                                        any = true;
                                    }
                                    if let Some(primary) = d["video_ssrc"].as_u64().filter(|s| *s != 0) {
                                        video_users.insert(primary as u32, user);
                                        any = true;
                                    }
                                    if !any {
                                        pictures.remove(user);
                                    }
                                    tracing::info!(user, on = any, "video: someone's camera");
                                    out.push(Reply::Json(json!({"op": 15, "d": {"any": 100}})));
                                }
                            }
                            6 => {
                                if let Some((nonce, sent)) = heartbeat_sent.take() {
                                    if d["t"].as_u64() == Some(nonce) {
                                        stats.ping_ms.store((sent.elapsed().as_millis() as u32).max(1), std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                            }
                            8 => {
                                let interval = d["heartbeat_interval"].as_f64().unwrap_or(13_750.).max(1000.);
                                heartbeat = tokio::time::interval(Duration::from_secs_f64(interval / 1000.));
                            }
                            11 => {
                                // Those already here are listed as we join: only newcomers ring.
                                let settled = connected_at.is_some_and(|at| at.elapsed() > Duration::from_secs(3));
                                let new_user = d["user_ids"].as_array().into_iter().flatten().filter_map(|u| u.as_str()?.parse::<u64>().ok()).any(|u| u != info.user_id && !dave.users.contains(&u));
                                if settled && new_user && !info.viewer
                                    && let Ok(mut speakers) = speakers.lock()
                                {
                                    speakers.play_sound(Sound::SomeoneJoined);
                                }
                                dave.users.extend(d["user_ids"].as_array().into_iter().flatten().filter_map(|u| u.as_str()?.parse::<u64>().ok()));
                                if let Some(user) = d["user_id"].as_str().and_then(|u| u.parse().ok()) {
                                    dave.users.insert(user);
                                }
                            }
                            13 => {
                                if let Some(user) = d["user_id"].as_str().and_then(|u| u.parse::<u64>().ok()) {
                                    dave.users.remove(&user);
                                    if !info.viewer
                                        && let Ok(mut speakers) = speakers.lock()
                                    {
                                        speakers.play_sound(Sound::SomeoneLeft);
                                    }
                                    let gone: Vec<u32> = users_by_ssrc.iter().filter(|(_, u)| **u == user).map(|(s, _)| *s).collect();
                                    for who in gone {
                                        users_by_ssrc.remove(&who);
                                        if let Ok(mut speakers) = speakers.lock() {
                                            speakers.remove(who);
                                        }
                                    }
                                }
                            }
                            _ => dave.on_json(op, d, &mut out),
                        }
                    }
                    _ => {}
                }
            }
            _ = heartbeat.tick() => {
                let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_millis() as u64);
                heartbeat_sent = Some((nonce, Instant::now()));
                out.push(Reply::Json(json!({"op": 3, "d": {"t": nonce, "seq_ack": last_seq}})));
            }
            _ = report.tick() => {
                let ready = dave.session.as_ref().is_some_and(DaveSession::is_ready);
                let (concealed, played) = stats.frames();
                let (skipped, underruns) = stats.glitches();
                tracing::info!(
                    received = counts.received, unreadable = counts.unreadable, plain = counts.plain,
                    decrypted = counts.decrypted, unknown_speaker = counts.unknown_speaker,
                    not_ready = counts.not_ready, failed = counts.failed, speaking = counts.speaking_events,
                    speakers = users_by_ssrc.len(), group = dave.users.len(), dave = dave.version, ready,
                    played, concealed, skipped, underruns, late = stats.late(),
                    ops = ?ops_seen, "voice: packets"
                );
            }
            item = outgoing.recv() => {
                let (Some(item), Some(socket), Some(secret)) = (item, &udp, &key) else { continue };
                match item {
                    Outgoing::Frame(frame) => {
                        let Some(payload) = dave.seal(&frame) else { continue };
                        if !sent.speaking {
                            sent.speaking = true;
                            // 1: the microphone.
                            out.push(Reply::Json(json!({"op": 5, "d": {"speaking": 1, "delay": 0, "ssrc": ssrc}})));
                        }
                        if let Some(packet) = sent.next(mode, secret, ssrc, &payload) {
                            let _ = socket.send(&packet).await;
                        }
                    }
                    Outgoing::Stop => {
                        if sent.speaking {
                            // Five frames of silence end it cleanly for the others' decoders.
                            for _ in 0..5 {
                                if let Some(packet) = sent.next(mode, secret, ssrc, &SILENCE) {
                                    let _ = socket.send(&packet).await;
                                }
                            }
                            sent.speaking = false;
                            out.push(Reply::Json(json!({"op": 5, "d": {"speaking": 0, "delay": 0, "ssrc": ssrc}})));
                        }
                    }
                    Outgoing::VideoOn if video_ssrc != 0 => {
                        video_on = true;
                        out.push(Reply::Json(json!({"op": 12, "d": {
                            "audio_ssrc": ssrc,
                            "video_ssrc": video_ssrc,
                            "rtx_ssrc": rtx_ssrc,
                            "streams": [{
                                "type": "video", "rid": "100", "ssrc": video_ssrc, "rtx_ssrc": rtx_ssrc, "active": true, "quality": 100,
                                "max_bitrate": crate::video_send::BITRATE, "max_framerate": crate::video_send::FPS,
                                "max_resolution": {"type": "fixed", "width": crate::video_send::WIDTH, "height": crate::video_send::HEIGHT},
                            }],
                        }})));
                        tracing::info!(video_ssrc, "camera: on");
                    }
                    Outgoing::VideoOn => tracing::warn!("camera: no video SSRC from the server"),
                    Outgoing::VideoOff => {
                        if video_on {
                            video_on = false;
                            out.push(Reply::Json(json!({"op": 12, "d": {"audio_ssrc": ssrc, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []}})));
                            tracing::info!("camera: off");
                        }
                    }
                    Outgoing::Video { frame, timestamp, .. } => {
                        if !video_on {
                            continue;
                        }
                        // DAVE's layer on the whole frame, then cut into packets.
                        let Some(sealed) = dave.seal_video(&frame) else {
                            crate::video_send::KEYFRAME_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
                            continue;
                        };
                        let packets = crate::video_send::packetize_h264(&sealed);
                        let last = packets.len().saturating_sub(1);
                        for (i, payload) in packets.iter().enumerate() {
                            let marker = i == last;
                            if let Some(packet) = transport::seal_video_rtp(mode, secret, video::H264, marker, video_seq, timestamp, video_ssrc, sent.counter, transport_seq, "100", payload) {
                                let _ = socket.send(&packet).await;
                            }
                            video_seq = video_seq.wrapping_add(1);
                            transport_seq = transport_seq.wrapping_add(1);
                            sent.counter = sent.counter.wrapping_add(1);
                        }
                    }
                }
            }
            _ = keepalive.tick() => {
                if let Some(socket) = &udp {
                    let mut packet = [0u8; 8];
                    packet[..4].copy_from_slice(&ssrc.to_be_bytes());
                    let _ = socket.send(&packet).await;
                }
            }
            received = async {
                match &udp {
                    Some(socket) => socket.recv(&mut buffer).await,
                    None => std::future::pending().await,
                }
            } => {
                let Ok(size) = received else { continue };
                let Some(secret) = &key else { continue };
                counts.received += 1;
                // RTCP: reports, and someone watching our camera asking for a keyframe. They come
                // bundled, a receiver report usually first and the request after it, so any RTCP
                // is opened and looked through, not only what starts with a request.
                if size > 1 && (200..=206).contains(&buffer[1]) {
                    let opened = transport::open_rtcp(mode, secret, &buffer[..size]);
                    // Whether the server gets our video at all, and how much of it: every few seconds.
                    if let Some(rtcp) = &opened
                        && video_on
                        && let Some((fraction, lost, highest)) = transport::report_about(rtcp, video_ssrc)
                        && report_logged.is_none_or(|at: Instant| at.elapsed() > Duration::from_secs(3))
                    {
                        report_logged = Some(Instant::now());
                        tracing::info!(fraction_lost = fraction, lost, highest_seq = highest, sent_seq = video_seq, "camera: server's report");
                    }
                    if let Some(rtcp) = opened
                        && video_on
                        && transport::keyframe_request(&rtcp) == Some(video_ssrc)
                    {
                        tracing::info!("camera: keyframe asked for");
                        crate::video_send::KEYFRAME_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    continue;
                }
                let Some(rtp) = transport::open_packet(mode, secret, &buffer[..size]) else {
                    counts.unreadable += 1;
                    continue;
                };
                if let Some((codec, resend)) = video::video_payload(rtp.payload_type) {
                    // A resend carries the original sequence number first, on its own SSRC.
                    let (primary, seq, payload) = if resend {
                        let Some(primary) = rtx_of.get(&rtp.ssrc).copied() else { continue };
                        if rtp.payload.len() < 2 {
                            continue;
                        }
                        (primary, u16::from_be_bytes([rtp.payload[0], rtp.payload[1]]), rtp.payload[2..].to_vec())
                    } else {
                        (rtp.ssrc, rtp.seq, rtp.payload)
                    };
                    let Some(user) = video_users.get(&primary).copied() else { continue };
                    // Padding only, sent to probe the line: nothing to put in a frame.
                    if payload.is_empty() {
                        continue;
                    }
                    let inbound = inbound_video.entry(primary).or_insert_with(|| video::Inbound::new(user, codec, pictures.clone()));
                    if let Some(mut frame) = inbound.push(seq, rtp.timestamp, rtp.marker, payload) {
                        match dave.open(Some(user), std::mem::take(&mut frame.data), MediaType::VIDEO, &mut counts) {
                            Some(clear) => {
                                frame.data = clear;
                                inbound.decode(frame);
                            }
                            None => inbound.broken = true,
                        }
                    }
                    if inbound.wants_keyframe()
                        && let Some(packet) = transport::seal_rtcp(mode, secret, sent.counter, &transport::pli(ssrc, primary))
                    {
                        sent.counter = sent.counter.wrapping_add(1);
                        let _ = udp.as_ref().map(|s| s.try_send(&packet));
                    }
                    continue;
                }
                let user = users_by_ssrc.get(&rtp.ssrc).copied();
                if let Some(audio) = dave.open(user, rtp.payload, MediaType::AUDIO, &mut counts)
                    && let Ok(mut speakers) = speakers.lock()
                {
                    speakers.push(rtp.ssrc, rtp.seq, audio);
                }
            }
        }
        for reply in out {
            let sent = match reply {
                Reply::Json(value) => ws_tx.send(Message::text(value.to_string())).await,
                Reply::Binary(data) => ws_tx.send(Message::binary(data)).await,
            };
            if let Err(error) = sent {
                return End::Retry(error.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_addresses_come_out_clean() {
        assert_eq!(gateway_url("c-abc123.discord.media:443"), "wss://c-abc123.discord.media/?v=8");
        assert_eq!(gateway_url("wss://host.discord.media/"), "wss://host.discord.media/?v=8");
    }

    #[test]
    fn ip_discovery_replies_are_read() {
        let mut packet = [0u8; 74];
        packet[..2].copy_from_slice(&2u16.to_be_bytes());
        packet[8..17].copy_from_slice(b"127.0.0.1");
        packet[72..74].copy_from_slice(&5000u16.to_be_bytes());
        assert_eq!(parse_discovery(&packet), Some(("127.0.0.1".to_string(), 5000)));
        packet[1] = 1;
        assert_eq!(parse_discovery(&packet), None, "a request, not a reply");
        assert_eq!(parse_discovery(&[0; 10]), None);
    }

    #[test]
    fn being_removed_is_final_and_a_blip_is_not() {
        assert!(close_is_final(4014), "disconnected on purpose");
        assert!(close_is_final(4017), "DAVE required");
        assert!(!close_is_final(4015), "server crashed: try again");
        assert!(!close_is_final(1006));
    }

    #[test]
    fn packets_without_daves_marker_pass_through() {
        let mut dave = Dave::new(1, 2);
        dave.version = 1;
        let mut counts = Counts::default();
        assert_eq!(dave.open(Some(3), SILENCE.to_vec(), MediaType::AUDIO, &mut counts), Some(SILENCE.to_vec()), "silence stays clear");
        let mut sealed = vec![0u8; 20];
        sealed[18..].copy_from_slice(&DAVE_MARKER);
        assert_eq!(dave.open(Some(3), sealed, MediaType::AUDIO, &mut counts), None, "no keys yet: dropped, not played as noise");
        assert_eq!((counts.plain, counts.not_ready), (1, 1));
    }
}
