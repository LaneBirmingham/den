//! Line-based terminal client. Proof that the core has no UI assumptions:
//! the same `Session` the GUI uses, rendered as plain lines.

use std::io::Write;

use denchat_core::events::Event;
use denchat_core::proto::{ChatMessage, Member, MessageKind};
use denchat_core::session::Session;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast::error::RecvError;

pub fn clock(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts).map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default()
}

/// Messages of the focused channel printed when joining a room.
const HISTORY_LINES: usize = 20;

pub async fn run(session: Session) -> anyhow::Result<()> {
    let mut events = session.subscribe();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut history = HistoryPrint::default();
    println!("den {} — type /help for commands, /quit to exit", denchat_core::VERSION);
    let signal = crate::serve::wait_for_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(l) if matches!(l.trim(), "/quit" | "/exit" | "/q") => break,
                Some(l) => session.submit(None, &l),
                None => break,
            },
            ev = events.recv() => match ev {
                Ok(ev) => {
                    history.on_event(&session, &ev);
                    print_event(&session, &ev);
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            _ = &mut signal => {
                tracing::info!("signal received; closing");
                break;
            }
        }
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

/// Print recent history once per room, when it first arrives after joining.
/// (Reconnects and handoffs keep the same room, so they don't repeat it.)
#[derive(Default)]
struct HistoryPrint {
    printed_room: Option<String>,
    pending: bool,
}

impl HistoryPrint {
    fn on_event(&mut self, session: &Session, ev: &Event) {
        match ev.event.as_str() {
            "connection.changed" if ev.data["state"] == "connected" => {
                let room = session.state().room_id().map(str::to_owned);
                self.pending = room.is_some() && room != self.printed_room;
            }
            "history.loaded" if self.pending => {
                let st = session.state();
                if st.focus.as_deref() != ev.data["channel"].as_str() {
                    return;
                }
                let msgs = st.messages(st.focus.as_deref().unwrap_or_default());
                let recent: Vec<&ChatMessage> = msgs.iter().filter(|m| m.kind != MessageKind::System).collect();
                let recent = &recent[recent.len().saturating_sub(HISTORY_LINES)..];
                if !recent.is_empty() {
                    println!("== last {} message(s) in #{}", recent.len(), recent[0].channel);
                    for m in recent {
                        print_message(m, "");
                    }
                }
                self.printed_room = st.room_id().map(str::to_owned);
                self.pending = false;
            }
            _ => {}
        }
    }
}

fn print_message(m: &ChatMessage, chan: &str) {
    let t = clock(m.ts);
    let (head, text) = match (m.kind, m.emote()) {
        (MessageKind::User, Some(action)) => (format!("[{t}] {chan}* {} ", m.author), action),
        (MessageKind::User, None) => (format!("[{t}] {chan}<{}> ", m.author), m.text.as_str()),
        (MessageKind::Server, _) => (format!("[{t}] {chan}** {}: ", m.author), m.text.as_str()),
        (MessageKind::System, _) => (format!("[{t}] {chan}-- "), m.text.as_str()),
    };
    // Later lines of a multi-line message line up under the first.
    let pad = format!("\n{}", " ".repeat(head.chars().count()));
    println!("{head}{}", text.lines().collect::<Vec<_>>().join(&pad));
}

fn print_event(session: &Session, ev: &Event) {
    let d = &ev.data;
    match ev.event.as_str() {
        "message.created" => {
            let Ok(m) = serde_json::from_value::<ChatMessage>(d["message"].clone()) else { return };
            let focus = session.state().focus.clone();
            let chan = if focus.as_deref() == Some(m.channel.as_str()) { String::new() } else { format!("#{} ", m.channel) };
            print_message(&m, &chan);
        }
        "notice" => {
            for line in d["text"].as_str().unwrap_or_default().lines() {
                println!("-- {line}");
            }
        }
        "member.joined" | "member.left" => {
            if let Ok(m) = serde_json::from_value::<Member>(d["member"].clone()) {
                let verb = if ev.event == "member.joined" { "joined" } else { "left" };
                let who = m.identity.short().map(|k| format!(" (key {k})")).unwrap_or_default();
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
                    denchat_core::net::short_addr(d["addr"].as_str().unwrap_or("")),
                    text.join(" "),
                    if voice.is_empty() { "-".into() } else { voice.join(" ") },
                    people.join(", ")
                );
            }
            Some("connecting") => println!("== reconnecting to {}…", denchat_core::net::short_addr(d["addr"].as_str().unwrap_or(""))),
            Some("failed") => println!("== {}", d["error"].as_str().unwrap_or("connection failed")),
            _ => {}
        },
        "hosting.changed" => {
            if let Some(share) = d["hosting"]["share"].as_array() {
                let addrs: Vec<&str> = share.iter().filter_map(|v| v.as_str()).collect();
                println!(
                    "== hosting \"{}\" — friends join with: den join {}",
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
