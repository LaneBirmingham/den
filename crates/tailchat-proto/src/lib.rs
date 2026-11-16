//! Wire protocol for tailchat.
//!
//! Two channels run between a client and a room server, both over the tailnet:
//!
//! * **Control** — a TCP stream of length-prefixed JSON frames ([`ClientMsg`] /
//!   [`ServerMsg`]). Text chat, membership, voice signalling and mod-defined
//!   `app` messages all travel here.
//! * **Voice** — UDP datagrams on the same port number carrying codec frames
//!   (see [`voice`]). The server relays them without decoding, so a headless
//!   server needs no audio stack.
//!
//! Everything here is plain data: no I/O, no async runtime. Any frontend or
//! alternative implementation can speak the protocol from this crate alone.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod voice;

/// Bumped on breaking wire changes. Servers reject mismatched majors.
pub const PROTOCOL_VERSION: u32 = 1;
/// Default TCP (control) and UDP (voice) port. "TAIL" on a phone keypad.
pub const DEFAULT_PORT: u16 = 8245;
/// Largest control frame either side will accept.
pub const MAX_FRAME: usize = 1 << 20;
/// Longest chat message accepted by the server, in bytes.
pub const MAX_MESSAGE_LEN: usize = 4000;

/// Session-scoped member id assigned by the server on connect.
pub type MemberId = u32;
/// Stable channel slug, e.g. `general`.
pub type ChannelId = String;

/// Who a member is, as far as the server can tell.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Identity {
    /// Tailnet login (e.g. `alice@example.com`) when verified via `tailscale whois`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Tailnet display name when verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Machine name on the tailnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// True when the server resolved the peer through Tailscale.
    #[serde(default)]
    pub verified: bool,
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
    /// Monotonic per server, survives restarts when history is persisted.
    pub id: u64,
    pub channel: ChannelId,
    /// Unix time in milliseconds (server clock).
    pub ts: i64,
    /// Session id of the author while they are connected; `None` for server/system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_id: Option<MemberId>,
    /// Nick at the time of sending.
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_login: Option<String>,
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
}

/// Where and how to send voice datagrams for this session.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VoiceSetup {
    pub udp_port: u16,
    /// Random per-session token that authenticates UDP packets.
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
    },
    /// Ask for public room info and close. Used by tailnet discovery.
    Probe,
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
    fn nick_and_channel_cleaning() {
        assert_eq!(clean_nick("  lane \t b  "), "lane b");
        assert_eq!(clean_nick("\u{7}"), "anon");
        assert_eq!(clean_channel_id("#Dev Chat!"), "dev-chat-");
    }
}
