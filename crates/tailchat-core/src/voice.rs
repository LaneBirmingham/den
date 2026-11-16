//! Glue between a session and the audio engine: the client's UDP voice link.
//!
//! The engine (in `tailchat-audio`) knows devices and codecs; this module
//! knows the wire. One UDP socket per voice session, connected to the room
//! server's voice port. A receive thread feeds RELAY packets to the engine,
//! a keepalive thread sends REGISTER so the server learns our address (and
//! NAT/firewall state stays warm) even while we are silent.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use parking_lot::Mutex;
use tailchat_audio::{Codec, PacketSink, TxMode, VoiceEngine, VoiceSettings};
use tailchat_proto::voice::Packet;

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

struct UdpSink {
    sock: UdpSocket,
    token: u64,
    buf: Mutex<Vec<u8>>,
}

impl PacketSink for UdpSink {
    fn send_voice(&self, seq: u16, codec: Codec, payload: &[u8]) {
        let mut buf = self.buf.lock();
        Packet::Audio { token: self.token, seq, codec, payload }.write(&mut buf);
        let _ = self.sock.send(&buf);
    }
}

pub struct VoiceLink {
    engine: Arc<VoiceEngine>,
    stop: Arc<AtomicBool>,
    registered: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl VoiceLink {
    /// Open audio devices and start talking to `server` (the room's voice port).
    pub fn start(server: SocketAddr, token: u64, settings: VoiceSettings) -> anyhow::Result<VoiceLink> {
        let bind = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let sock = UdpSocket::bind(bind).context("binding voice socket")?;
        sock.connect(server).with_context(|| format!("connecting voice socket to {server}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let sink = Arc::new(UdpSink { sock: sock.try_clone()?, token, buf: Mutex::new(Vec::with_capacity(1500)) });
        let engine = Arc::new(VoiceEngine::start(settings, sink).context("starting audio")?);
        let stop = Arc::new(AtomicBool::new(false));
        let registered = Arc::new(AtomicBool::new(false));

        let rx_sock = sock.try_clone()?;
        let rx_engine = engine.clone();
        let rx_stop = stop.clone();
        let rx_reg = registered.clone();
        let rx = std::thread::Builder::new().name("tailchat-voice-rx".into()).spawn(move || {
            let mut buf = [0u8; 2048];
            while !rx_stop.load(Ordering::Relaxed) {
                let n = match rx_sock.recv(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue, // timeouts, and ICMP resets on Windows
                };
                match Packet::parse(&buf[..n]) {
                    Some(Packet::Relay { speaker, seq, codec, payload }) => rx_engine.receive(speaker, seq, codec, payload),
                    Some(Packet::Ack) => rx_reg.store(true, Ordering::Relaxed),
                    _ => {}
                }
            }
        })?;

        let ka_stop = stop.clone();
        let ka_reg = registered.clone();
        let register = Packet::Register { token }.to_vec();
        let ka = std::thread::Builder::new().name("tailchat-voice-keepalive".into()).spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(60);
            while !ka_stop.load(Ordering::Relaxed) {
                // Fast until the server acknowledges us, then a slow heartbeat.
                let every = if ka_reg.load(Ordering::Relaxed) { Duration::from_secs(10) } else { Duration::from_millis(500) };
                if last.elapsed() >= every {
                    let _ = sock.send(&register);
                    last = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })?;

        Ok(VoiceLink { engine, stop, registered, threads: vec![rx, ka] })
    }

    pub fn engine(&self) -> &VoiceEngine {
        &self.engine
    }

    /// The server has acknowledged our UDP address.
    pub fn registered(&self) -> bool {
        self.registered.load(Ordering::Relaxed)
    }
}

impl Drop for VoiceLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
