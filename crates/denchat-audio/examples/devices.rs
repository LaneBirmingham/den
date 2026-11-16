//! Print the audio devices Den can use: `cargo run -p denchat-audio --example devices`.

fn main() -> anyhow::Result<()> {
    let list = denchat_audio::list_devices()?;
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
    println!("Codecs: {:?}", denchat_audio::supported_codecs());
    Ok(())
}
