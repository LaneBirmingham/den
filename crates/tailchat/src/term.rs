//! Line-based terminal client. Proof that the core has no UI assumptions:
//! the same `Session` the GUI uses, rendered as plain lines.

use std::io::Write;

use tailchat_core::events::Event;
use tailchat_core::proto::{ChatMessage, Member, MessageKind};
use tailchat_core::session::Session;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast::error::RecvError;

pub fn clock(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts).map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default()
}

pub async fn run(session: Session) -> anyhow::Result<()> {
    let mut events = session.subscribe();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    println!("tailchat {} — type /help for commands, /quit to exit", tailchat_core::VERSION);
    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(l) if matches!(l.trim(), "/quit" | "/exit" | "/q") => break,
                Some(l) => session.submit(None, &l),
                None => break,
            },
            ev = events.recv() => match ev {
                Ok(ev) => print_event(&session, &ev),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

fn print_event(session: &Session, ev: &Event) {
    let d = &ev.data;
    match ev.event.as_str() {
        "message.created" => {
            let Ok(m) = serde_json::from_value::<ChatMessage>(d["message"].clone()) else { return };
            let focus = session.state().focus.clone();
            let chan = if focus.as_deref() == Some(m.channel.as_str()) { String::new() } else { format!("#{} ", m.channel) };
            match m.kind {
                MessageKind::User => println!("[{}] {chan}<{}> {}", clock(m.ts), m.author, m.text),
                MessageKind::Server => println!("[{}] {chan}** {}: {}", clock(m.ts), m.author, m.text),
                MessageKind::System => println!("[{}] {chan}-- {}", clock(m.ts), m.text),
            }
        }
        "notice" => {
            for line in d["text"].as_str().unwrap_or_default().lines() {
                println!("-- {line}");
            }
        }
        "member.joined" | "member.left" => {
            if let Ok(m) = serde_json::from_value::<Member>(d["member"].clone()) {
                let verb = if ev.event == "member.joined" { "joined" } else { "left" };
                let who = m.identity.login.map(|l| format!(" ({l})")).unwrap_or_default();
                println!("-- {}{who} {verb}", m.nick);
            }
        }
        "connection.changed" => match d["state"].as_str() {
            Some("connected") => {
                let st = session.state();
                let text: Vec<String> = st.text_channels().map(|c| format!("#{}", c.id)).collect();
                let voice: Vec<String> = st.voice_channels().map(|c| format!("~{}", c.id)).collect();
                let people: Vec<String> = st.members.values().map(|m| m.nick.clone()).collect();
                println!(
                    "== connected to {} ({}) · channels {} · voice {} · here: {}",
                    d["room"].as_str().unwrap_or("room"),
                    d["addr"].as_str().unwrap_or(""),
                    text.join(" "),
                    if voice.is_empty() { "-".into() } else { voice.join(" ") },
                    people.join(", ")
                );
            }
            Some("connecting") => println!("== reconnecting to {}…", d["addr"].as_str().unwrap_or("")),
            Some("failed") => println!("== {}", d["error"].as_str().unwrap_or("connection failed")),
            _ => {}
        },
        "hosting.changed" => {
            if let Some(share) = d["hosting"]["share"].as_array() {
                let addrs: Vec<&str> = share.iter().filter_map(|v| v.as_str()).collect();
                println!(
                    "== hosting \"{}\" — friends join with: tailchat join {}",
                    d["hosting"]["name"].as_str().unwrap_or(""),
                    addrs.first().copied().unwrap_or("?")
                );
                if addrs.len() > 1 {
                    println!("   (also: {})", addrs[1..].join(", "));
                }
            }
        }
        "room.moved" => println!("== the room moved to {} (term {})", d["to"][0].as_str().unwrap_or("?"), d["term"]),
        "voice.joined" => println!("== in voice ~{}", d["channel"].as_str().unwrap_or("")),
        "voice.left" => println!("== left voice"),
        _ => {}
    }
}
