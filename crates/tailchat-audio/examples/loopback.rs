//! Local echo test: mic → encode → decode → speakers. Use headphones.
//!
//! `cargo run -p tailchat-audio --example loopback -- [seconds] [opus|pcm16] [volume]`

use std::time::Duration;

use tailchat_audio::{Codec, TxMode, VoiceSettings, loopback_test};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let secs: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(5.0);
    let codec = match args.next().as_deref() {
        Some("pcm16") => Codec::Pcm16,
        _ => tailchat_audio::supported_codecs()[0],
    };
    let output_volume: f32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(1.0);
    let settings = VoiceSettings { mode: TxMode::Open, output_volume, ..Default::default() };
    println!("loopback for {secs}s with {codec:?}...");
    let stats = loopback_test(settings, codec, Duration::from_secs_f64(secs))?;
    println!("{stats:#?}");
    Ok(())
}
