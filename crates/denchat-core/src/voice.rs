//! Glue between a session and the audio engine: the client's voice link.
//!
//! The engine (in `denchat-audio`) knows devices and codecs; this module
//! knows the wire. Voice rides the same connection as chat, as unreliable
//! QUIC datagrams: a receive task feeds RELAY packets to the engine, and the
//! engine's packets go straight out as datagrams.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use denchat_audio::{Codec, PacketSink, TxMode, VoiceEngine, VoiceSettings};
use denchat_proto::voice::Packet;
use iroh::endpoint::Connection;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::config::VoiceConfig;

pub fn tx_mode(mode: &str) -> TxMode {
    match mode {
        "push_to_talk" | "ptt" => TxMode::PushToTalk,
        "open" | "always" => TxMode::Open,
        _ => TxMode::VoiceActivity,
    }
}

pub fn settings(cfg: &VoiceConfig) -> VoiceSettings {
    VoiceSettings {
        input_device: cfg.input_device.clone(),
        output_device: cfg.output_device.clone(),
        mode: tx_mode(&cfg.mode),
        vad_threshold: cfg.vad_threshold,
        bitrate: cfg.bitrate,
        input_gain: cfg.input_gain,
        output_volume: cfg.output_volume,
    }
}

struct DatagramSink {
    link: Connection,
    token: u64,
    buf: Mutex<Vec<u8>>,
    sent: AtomicU64,
}

impl PacketSink for DatagramSink {
    fn send_voice(&self, seq: u16, codec: Codec, payload: &[u8]) {
        let mut buf = self.buf.lock();
        Packet::Audio { token: self.token, seq, codec, payload }.write(&mut buf);
        if self.link.send_datagram(Bytes::copy_from_slice(&buf)).is_ok() {
            self.sent.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct VoiceLink {
    engine: Arc<VoiceEngine>,
    registered: Arc<AtomicBool>,
    rx: tokio::task::JoinHandle<()>,
}

impl VoiceLink {
    /// Open audio devices and start talking over `link` (the connection to
    /// the room's host). Needs a Tokio runtime (`rt`) for the receive task.
    pub fn start(link: Connection, token: u64, settings: VoiceSettings, rt: &Handle) -> anyhow::Result<VoiceLink> {
        let sink = Arc::new(DatagramSink { link: link.clone(), token, buf: Mutex::new(Vec::with_capacity(1500)), sent: AtomicU64::new(0) });
        let engine = Arc::new(VoiceEngine::start(settings, sink).context("starting audio")?);
        let registered = Arc::new(AtomicBool::new(false));

        let rx_engine = engine.clone();
        let rx_reg = registered.clone();
        let rx = rt.spawn(async move {
            // Say we're listening until the host answers (datagrams can drop).
            let register = Bytes::from(Packet::Register { token }.to_vec());
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    d = link.read_datagram() => {
                        let Ok(d) = d else { break };
                        match Packet::parse(&d) {
                            Some(Packet::Relay { speaker, seq, codec, payload }) => rx_engine.receive(speaker, seq, codec, payload),
                            Some(Packet::Ack) => rx_reg.store(true, Ordering::Relaxed),
                            _ => {}
                        }
                    }
                    _ = tick.tick(), if !rx_reg.load(Ordering::Relaxed) => {
                        let _ = link.send_datagram(register.clone());
                    }
                }
            }
        });

        Ok(VoiceLink { engine, registered, rx })
    }

    pub fn engine(&self) -> &VoiceEngine {
        &self.engine
    }

    /// The host has answered our voice packets.
    pub fn registered(&self) -> bool {
        self.registered.load(Ordering::Relaxed)
    }
}

impl Drop for VoiceLink {
    fn drop(&mut self) {
        self.rx.abort();
    }
}
