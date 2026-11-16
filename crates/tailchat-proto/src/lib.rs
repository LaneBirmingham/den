//! Wire protocol for tailchat.
//!
//! A client and a room server talk over one encrypted, peer-to-peer QUIC
//! connection (iroh: dialed by public key, hole-punched, relayed as a last
//! resort; ALPN [`ALPN`]). It carries two things:
//!
//! * **Control** — one bidirectional stream of length-prefixed JSON frames
//!   ([`ClientMsg`] / [`ServerMsg`]). Text chat, membership, voice signalling
//!   and mod-defined `app` messages all travel here.
//! * **Voice** — unreliable QUIC datagrams carrying codec frames (see
//!   [`voice`]). The server relays them without decoding, so a headless
//!   server needs no audio stack.
//!
//! **No accounts.** Each member makes a key pair per room and proves it on
//! join by signing the connection ([`Proof`]); the room remembers the public
//! half next to the member's nick ([`Profile`]). Rooms with an invite key
//! only admit people who hold it (it travels in invite links).
//!
//! Everything here is plain data: no I/O, no async runtime. Any frontend or
//! alternative implementation can speak the protocol from this crate alone.
//!
//! **Rooms move.** A room has a stable id and a `term` that goes up each time
//! another member takes over hosting. Every member keeps a copy of the room
//! (settings + recent history) and swaps message ids with the host on join
//! ([`ClientMsg::Sync`]), so any of them can host it later. A planned change
//! of host is [`ServerMsg::HostRequest`] → [`ClientMsg::HostReady`] →
//! [`ServerMsg::Handoff`]; a peer that already hosts the room with a higher
//! term takes it over with [`ClientMsg::Claim`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod voice;

/// Bumped on breaking wire changes. Servers reject mismatched majors.
pub const PROTOCOL_VERSION: u32 = 3;
/// QUIC application protocol name for tailchat connections.
pub const ALPN: &[u8] = b"tailchat/3";
/// Default UDP port for `tailchat serve` (apps pick a free one). "TAIL" on a phone keypad.
pub const DEFAULT_PORT: u16 = 8245;
/// Largest control frame either side will accept.
pub const MAX_FRAME: usize = 1 << 20;
/// Longest chat message accepted by the server, in bytes.
pub const MAX_MESSAGE_LEN: usize = 4000;
/// Most messages in one `history`, `backfill` or `upload` frame (keeps frames under [`MAX_FRAME`]).
pub const MAX_BATCH: usize = 100;
/// `hello` feature: this client can host the room if asked.
pub const FEATURE_HOST: &str = "host";

/// How easily others can connect to a member's app directly, worst to best.
/// The room's host should be someone others can reach: everyone connects to
/// it, so a reachable host needs no relay. Members report what they measure
/// (at most `public`); only the host's check makes someone `open`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reach {
    /// UDP is blocked, or the NAT picks a new port for every destination:
    /// direct connections rarely work, so peers go through a relay.
    Strict,
    /// Not measured yet (or an app that doesn't say).
    #[default]
    Unknown,
    /// Behind a router or firewall: connecting out works, and connections
    /// in usually need hole-punching.
    Nat,
    /// Has a public address (not checked yet).
    Public,
    /// The host connected to it directly, from a fresh endpoint, with no
    /// relay and no hole-punching.
    Open,
}

impl Reach {
    pub fn as_str(self) -> &'static str {
        match self {
            Reach::Strict => "strict",
            Reach::Unknown => "unknown",
            Reach::Nat => "nat",
            Reach::Public => "public",
            Reach::Open => "open",
        }
    }

    /// A few words, for status lines.
    pub fn short(self) -> &'static str {
        match self {
            Reach::Strict => "strict network",
            Reach::Unknown => "measuring the network…",
            Reach::Nat => "behind a router",
            Reach::Public => "public address",
            Reach::Open => "reachable directly",
        }
    }

    /// For people.
    pub fn describe(self) -> &'static str {
        match self {
            Reach::Strict => "strict network: others may need a relay to reach it",
            Reach::Unknown => "connection not measured yet",
            Reach::Nat => "behind a router: fine to join from, so-so to host from",
            Reach::Public => "public address (not checked yet)",
            Reach::Open => "others can connect to it directly: a good host",
        }
    }
}

impl Serialize for Reach {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Reach {
    /// Unknown words (from newer apps) read as `unknown`.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "strict" => Reach::Strict,
            "nat" => Reach::Nat,
            "public" => Reach::Public,
            "open" => Reach::Open,
            _ => Reach::Unknown,
        })
    }
}

/// Session-scoped member id assigned by the server on connect.
pub type MemberId = u32;
/// Stable channel slug, e.g. `general`.
pub type ChannelId = String;

/// Who a member is, as far as the server can tell.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Identity {
    /// The member's public key for this room (hex). Members make one key pair
    /// per room; there are no global accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The member proved they hold `key` (signed this connection with it).
    #[serde(default)]
    pub verified: bool,
}

impl Identity {
    /// The first 8 characters of the key, for showing people.
    pub fn short(&self) -> Option<&str> {
        self.key.as_deref().map(|k| &k[..k.len().min(8)])
    }
}

/// Proof of keys, signed over the connection it travels on (so it can't be
/// replayed elsewhere). See the `tailchat-core` `net` module for the exact bytes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Proof {
    /// The member's public key for this room (hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Signature by `key` (hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    /// Signature by the room's invite key (hex): proves the sender was invited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
}

/// A member as the room remembers them: the public half of their key and the
/// nick it belongs to. Every member's copy of the room keeps the list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub key: String,
    pub nick: String,
    /// Unix ms the room first saw this key.
    #[serde(default)]
    pub since: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Member {
    pub id: MemberId,
    pub nick: String,
    #[serde(default)]
    pub identity: Identity,
    /// Voice channel the member is in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<ChannelId>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub deafened: bool,
    /// Free-form client name/version, e.g. `tailchat-gui/0.1.0`.
    #[serde(default)]
    pub client: String,
    /// Voice codecs this member can decode.
    #[serde(default)]
    pub codecs: Vec<voice::Codec>,
    /// The member's app can take over hosting (sent `features: ["host"]`).
    #[serde(default)]
    pub can_host: bool,
    /// Where the member's app can be reached (an address link, see `hello`),
    /// so members can find the room again whoever ends up hosting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// How easily others can connect to the member's app (see [`Reach`]).
    #[serde(default)]
    pub reach: Reach,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Written by a member.
    #[default]
    User,
    /// Join/leave and other automatic notices.
    System,
    /// Sent by the server operator, a server-side mod, or the admin console.
    Server,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatMessage {
    /// Unique and time-ordered across hosts: `(unix ms << 10) | random`, and
    /// always above the previous id in the room. `0` for local-only lines.
    pub id: u64,
    pub channel: ChannelId,
    /// Unix time in milliseconds (server clock).
    pub ts: i64,
    /// Session id of the author while they are connected; `None` for server/system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_id: Option<MemberId>,
    /// Nick at the time of sending.
    pub author: String,
    /// The author's room key (see [`Identity::key`]) when the host verified it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_key: Option<String>,
    pub text: String,
    #[serde(default)]
    pub kind: MessageKind,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct ServerInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motd: Option<String>,
    /// Server software version.
    pub version: String,
    /// `"host"` for a peer hosting from the app, `"server"` for a headless server.
    #[serde(default)]
    pub mode: String,
    /// Stable room id, the same whoever hosts it.
    #[serde(default)]
    pub room_id: String,
    /// Bumped on every change of host. The highest term is the live room.
    #[serde(default)]
    pub term: u64,
}

/// Who may join (besides holding the invite key, if the room has one).
/// Travels with the room so the next host applies the same rules.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Access {
    /// If non-empty, only these member keys may join: a full key, or a prefix
    /// of at least 8 characters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl Access {
    /// Whether `key` passes the allow list.
    pub fn allows(&self, key: Option<&str>) -> bool {
        self.allow.is_empty()
            || key.is_some_and(|k| {
                self.allow.iter().any(|a| {
                    let a = a.trim();
                    a.len() >= 8 && k.len() >= a.len() && k[..a.len()].eq_ignore_ascii_case(a)
                })
            })
    }
}

/// How voice works for this session: datagrams on the same connection.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VoiceSetup {
    /// Unused since protocol 3 (voice shares the connection); kept for the format.
    #[serde(default)]
    pub udp_port: u16,
    /// Per-session token carried in voice packets.
    pub token: u64,
    /// Fixed frame duration in milliseconds.
    pub frame_ms: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// Client → server control messages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First frame of a session.
    Hello {
        version: u32,
        nick: String,
        client: String,
        #[serde(default)]
        codecs: Vec<voice::Codec>,
        /// Optional capability flags for forward compatibility.
        #[serde(default)]
        features: Vec<String>,
        /// The room to join. One app can host several rooms on one address;
        /// without it, the host's only room.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room: Option<String>,
        /// The member's room key and, for rooms with one, the invite.
        #[serde(default)]
        proof: Proof,
        /// Where the member's own app can be reached (shared with the room).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
        /// How reachable the member's app thinks it is.
        #[serde(default)]
        reach: Reach,
    },
    /// Ask for public room info and close. Used to find a room's live host.
    Probe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room: Option<String>,
    },
    /// First frame, instead of `hello`: "I host this room now, at a higher
    /// term; send everyone to `addrs`". Answered with `handoff` (accepted) or
    /// `error`, then closed. The claimant must hold the invite and pass the
    /// room's [`Access`].
    Claim {
        room: String,
        term: u64,
        addrs: Vec<String>,
        #[serde(default)]
        proof: Proof,
    },
    Send {
        channel: ChannelId,
        text: String,
    },
    History {
        channel: ChannelId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<u64>,
        limit: u32,
    },
    SetNick {
        nick: String,
    },
    VoiceJoin {
        channel: ChannelId,
    },
    VoiceLeave,
    VoiceState {
        muted: bool,
        deafened: bool,
    },
    /// Mod-defined payload relayed to other members (or one member via `to`).
    /// The server never interprets `data`; `ns` namespaces it (e.g. `dice`).
    App {
        ns: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<ChannelId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<MemberId>,
        data: Value,
    },
    Ping {
        nonce: u64,
    },
    /// "These are the message ids I hold for `channel`." The server answers
    /// with `backfill` (what we lack) and `want` (what it lacks).
    Sync {
        channel: ChannelId,
        have: Vec<u64>,
    },
    /// Messages the server asked for with `want`.
    Upload {
        channel: ChannelId,
        messages: Vec<ChatMessage>,
    },
    /// Answer to `host_request`: we now host the room at `term` on `addrs`.
    HostReady {
        term: u64,
        addrs: Vec<String>,
    },
    /// Answer to `host_request`: we could not host.
    HostFailed {
        term: u64,
        error: String,
    },
    /// Our network changed: how reachable we think we are now, and our
    /// address. The host checks it again.
    Reach {
        reach: Reach,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
    },
}

/// Server → client control messages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        version: u32,
        server: ServerInfo,
        you: MemberId,
        voice: VoiceSetup,
        channels: Vec<Channel>,
        members: Vec<Member>,
        /// The room's join rules, so members can save and re-apply them.
        #[serde(default)]
        access: Access,
        /// Every member the room knows: key → nick. Members save it, so the
        /// next host keeps nicks with their owners.
        #[serde(default)]
        profiles: Vec<Profile>,
    },
    ProbeInfo {
        server: ServerInfo,
        members: u32,
        channels: Vec<Channel>,
    },
    Message {
        message: ChatMessage,
    },
    History {
        channel: ChannelId,
        messages: Vec<ChatMessage>,
        /// True when there is nothing older on the server.
        complete: bool,
    },
    MemberJoined {
        member: Member,
    },
    MemberLeft {
        id: MemberId,
    },
    MemberUpdated {
        member: Member,
    },
    ChannelsUpdated {
        channels: Vec<Channel>,
    },
    App {
        from: MemberId,
        ns: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<ChannelId>,
        data: Value,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    Pong {
        nonce: u64,
    },
    Error {
        code: String,
        message: String,
    },
    /// Sent right before the server closes the connection.
    Bye {
        reason: String,
    },
    /// Messages for `channel` that the receiver lacks (answer to `sync`, or
    /// history another member just uploaded). Not new: don't count as unread.
    Backfill {
        channel: ChannelId,
        messages: Vec<ChatMessage>,
    },
    /// Message ids the server lacks; answer with `upload`.
    Want {
        channel: ChannelId,
        ids: Vec<u64>,
    },
    /// "Please host this room from your copy at `term`"; answer `host_ready` or `host_failed`.
    HostRequest {
        term: u64,
    },
    /// The room moved: reconnect to the first of `to` that works.
    Handoff {
        to: Vec<String>,
        term: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("invalid frame json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode a message as `u32 big-endian length || json`.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a frame body (without the length prefix).
pub fn decode_body<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(body)?)
}

/// Normalise a user-supplied nick: trims, collapses whitespace, caps length.
pub fn clean_nick(raw: &str) -> String {
    let joined: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut nick: String = joined.chars().filter(|c| !c.is_control()).take(32).collect();
    if nick.is_empty() {
        nick = "anon".into();
    }
    nick
}

/// Normalise a channel slug: lowercase ascii, digits, `-` and `_`.
pub fn clean_channel_id(raw: &str) -> ChannelId {
    raw.trim()
        .trim_start_matches('#')
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' })
        .take(32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let msg = ClientMsg::Send { channel: "general".into(), text: "hello".into() };
        let frame = encode_frame(&msg).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let back: ClientMsg = decode_body(&frame[4..]).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn json_shape_is_tagged() {
        let v = serde_json::to_value(ServerMsg::MemberLeft { id: 7 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "member_left", "id": 7}));
    }

    #[test]
    fn room_messages_shape() {
        let v = serde_json::to_value(ServerMsg::Handoff { to: vec!["tailchat://abc".into()], term: 3 }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "handoff", "to": ["tailchat://abc"], "term": 3}));
        // A bare probe still parses.
        let probe: ClientMsg = serde_json::from_value(serde_json::json!({"type": "probe"})).unwrap();
        assert_eq!(probe, ClientMsg::Probe { room: None });
        let back: ClientMsg = serde_json::from_value(serde_json::json!({"type": "sync", "channel": "general", "have": [1, 2]})).unwrap();
        assert_eq!(back, ClientMsg::Sync { channel: "general".into(), have: vec![1, 2] });
        // Older peers' server info still parses.
        let info: ServerInfo = serde_json::from_value(serde_json::json!({"name": "den", "version": "0.1.0"})).unwrap();
        assert_eq!((info.room_id.as_str(), info.term), ("", 0));
    }

    #[test]
    fn allow_list_matches_key_prefixes() {
        let key = "0123456789abcdef0123456789abcdef";
        assert!(Access::default().allows(None));
        let a = Access { allow: vec!["01234567".into()] };
        assert!(a.allows(Some(key)));
        assert!(!a.allows(None));
        assert!(!a.allows(Some("ffffffffffffffff")));
        // Too-short entries never match (they'd admit too many keys).
        assert!(!Access { allow: vec!["0123".into()] }.allows(Some(key)));
    }

    #[test]
    fn nick_and_channel_cleaning() {
        assert_eq!(clean_nick("  lane \t b  "), "lane b");
        assert_eq!(clean_nick("\u{7}"), "anon");
        assert_eq!(clean_channel_id("#Dev Chat!"), "dev-chat-");
    }
}
