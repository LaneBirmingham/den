//! Print the audio devices tailchat can use: `cargo run -p tailchat-audio --example devices`.

fn main() -> anyhow::Result<()> {
    let list = tailchat_audio::list_devices()?;
    let mark = |name: &str, default: &Option<String>| {
        if default.as_deref() == Some(name) { " (default)" } else { "" }
    };
    println!("Inputs:");
    for name in &list.inputs {
        println!("  {name}{}", mark(name, &list.default_input));
    }
    println!("Outputs:");
    for name in &list.outputs {
        println!("  {name}{}", mark(name, &list.default_output));
    }
    println!("Codecs: {:?}", tailchat_audio::supported_codecs());
    Ok(())
}
