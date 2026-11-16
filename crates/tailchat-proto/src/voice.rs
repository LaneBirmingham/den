//! Binary UDP voice packets.
//!
//! All integers are big-endian. Frames are mono, 48 kHz, 20 ms.
//!
//! ```text
//! client → server
//!   REGISTER  [0x02][token:u64]                       keepalive + address learning
//!   AUDIO     [0x01][token:u64][seq:u16][codec:u8][payload…]
//! server → client
//!   ACK       [0x04]                                  reply to REGISTER
//!   RELAY     [0x03][speaker:u32][seq:u16][codec:u8][payload…]
//! ```
//!
//! The server authenticates packets by token *and* by checking the source IP
//! matches the member's control connection, then relays AUDIO to everyone else
//! in the same voice channel as RELAY. It never decodes payloads.

use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_MS: u32 = 20;
/// Samples per mono frame at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE / 1000 * FRAME_MS) as usize;
/// Largest payload we ever put in a datagram (well under tailnet MTU).
pub const MAX_PAYLOAD: usize = 1200;

const KIND_AUDIO: u8 = 0x01;
const KIND_REGISTER: u8 = 0x02;
const KIND_RELAY: u8 = 0x03;
const KIND_ACK: u8 = 0x04;

/// Voice codec identifiers. Values are wire-stable.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// Opus, 48 kHz mono, 20 ms frames.
    Opus,
    /// Uncompressed 16-bit little-endian PCM at 16 kHz (fallback when Opus is not built).
    Pcm16,
}

impl Codec {
    pub fn to_byte(self) -> u8 {
        match self {
            Codec::Opus => 1,
            Codec::Pcm16 => 2,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Codec::Opus),
            2 => Some(Codec::Pcm16),
            _ => None,
        }
    }

    /// Preference order when choosing a transmit codec (best first).
    pub fn preference() -> &'static [Codec] {
        &[Codec::Opus, Codec::Pcm16]
    }
}

/// Pick the best codec that every listener can decode, falling back to PCM.
pub fn negotiate<'a>(mine: &[Codec], listeners: impl IntoIterator<Item = &'a [Codec]>) -> Codec {
    let listeners: Vec<&[Codec]> = listeners.into_iter().collect();
    for c in Codec::preference() {
        if mine.contains(c) && listeners.iter().all(|l| l.contains(c)) {
            return *c;
        }
    }
    Codec::Pcm16
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet<'a> {
    Audio { token: u64, seq: u16, codec: Codec, payload: &'a [u8] },
    Register { token: u64 },
    Relay { speaker: u32, seq: u16, codec: Codec, payload: &'a [u8] },
    Ack,
}

impl<'a> Packet<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Packet<'a>> {
        let (&kind, rest) = buf.split_first()?;
        match kind {
            KIND_AUDIO if rest.len() >= 11 => Some(Packet::Audio {
                token: u64::from_be_bytes(rest[0..8].try_into().ok()?),
                seq: u16::from_be_bytes(rest[8..10].try_into().ok()?),
                codec: Codec::from_byte(rest[10])?,
                payload: &rest[11..],
            }),
            KIND_REGISTER if rest.len() >= 8 => Some(Packet::Register { token: u64::from_be_bytes(rest[0..8].try_into().ok()?) }),
            KIND_RELAY if rest.len() >= 7 => Some(Packet::Relay {
                speaker: u32::from_be_bytes(rest[0..4].try_into().ok()?),
                seq: u16::from_be_bytes(rest[4..6].try_into().ok()?),
                codec: Codec::from_byte(rest[6])?,
                payload: &rest[7..],
            }),
            KIND_ACK => Some(Packet::Ack),
            _ => None,
        }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.clear();
        match self {
            Packet::Audio { token, seq, codec, payload } => {
                out.push(KIND_AUDIO);
                out.extend_from_slice(&token.to_be_bytes());
                out.extend_from_slice(&seq.to_be_bytes());
                out.push(codec.to_byte());
                out.extend_from_slice(payload);
            }
            Packet::Register { token } => {
                out.push(KIND_REGISTER);
                out.extend_from_slice(&token.to_be_bytes());
            }
            Packet::Relay { speaker, seq, codec, payload } => {
                out.push(KIND_RELAY);
                out.extend_from_slice(&speaker.to_be_bytes());
                out.extend_from_slice(&seq.to_be_bytes());
                out.push(codec.to_byte());
                out.extend_from_slice(payload);
            }
            Packet::Ack => out.push(KIND_ACK),
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(16);
        self.write(&mut v);
        v
    }
}

/// Rewrite an AUDIO datagram in place-ish into a RELAY datagram for `speaker`.
pub fn audio_to_relay(speaker: u32, seq: u16, codec: Codec, payload: &[u8], out: &mut Vec<u8>) {
    Packet::Relay { speaker, seq, codec, payload }.write(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_kinds() {
        let payload = [1u8, 2, 3, 4];
        let cases = [
            Packet::Audio { token: 0xdead_beef_cafe_f00d, seq: 65535, codec: Codec::Opus, payload: &payload },
            Packet::Register { token: 42 },
            Packet::Relay { speaker: 9, seq: 1, codec: Codec::Pcm16, payload: &payload },
            Packet::Ack,
        ];
        for p in cases {
            let bytes = p.to_vec();
            assert_eq!(Packet::parse(&bytes), Some(p));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Packet::parse(&[]), None);
        assert_eq!(Packet::parse(&[0x01, 0, 0]), None);
        assert_eq!(Packet::parse(&[0x09; 20]), None);
        // unknown codec byte
        let mut bad = Packet::Audio { token: 1, seq: 1, codec: Codec::Opus, payload: &[] }.to_vec();
        bad[11] = 99;
        assert_eq!(Packet::parse(&bad), None);
    }

    #[test]
    fn negotiation_prefers_opus_when_everyone_has_it() {
        let both = [Codec::Opus, Codec::Pcm16];
        let pcm = [Codec::Pcm16];
        assert_eq!(negotiate(&both, [&both[..], &both[..]]), Codec::Opus);
        assert_eq!(negotiate(&both, [&both[..], &pcm[..]]), Codec::Pcm16);
        assert_eq!(negotiate(&pcm, [&both[..]]), Codec::Pcm16);
        assert_eq!(negotiate(&both, std::iter::empty()), Codec::Opus);
    }
}
