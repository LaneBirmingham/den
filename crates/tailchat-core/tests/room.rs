//! End-to-end tests: real server, real sessions, loopback sockets.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tailchat_core::api::ApiClient;
use tailchat_core::config::{AppConfig, ServerConfig};
use tailchat_core::net::{read_frame, write_frame};
use tailchat_core::paths::Paths;
use tailchat_core::proto::voice::{Codec, Packet};
use tailchat_core::proto::{ClientMsg, MessageKind, PROTOCOL_VERSION, ServerMsg};
use tailchat_core::rooms;
use tailchat_core::server::{self, ServerHandle, ServerMode, ServerOptions};
use tailchat_core::server_node::{NodeOptions, ServerNode};
use tailchat_core::session::{HostOptions, Session, SessionOptions};
use tokio::net::{TcpStream, UdpSocket};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tailchat-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_server(history: Option<PathBuf>) -> ServerHandle {
    start_room(history, "test-room", 1).await
}

async fn start_room(history: Option<PathBuf>, room_id: &str, term: u64) -> ServerHandle {
    let config = ServerConfig { name: "test room".into(), rate_limit: 100, ..ServerConfig::default() };
    server::start(ServerOptions {
        config,
        mode: ServerMode::Headless,
        bind: "127.0.0.1:0".parse().unwrap(),
        history_dir: history,
        tailscale: None,
        room_id: room_id.into(),
        term,
    })
    .await
    .unwrap()
}

async fn session(name: &str, home: &std::path::Path) -> Session {
    let config = AppConfig {
        nick: Some(name.into()),
        // Never touch the real tailnet from tests.
        tailscale_bin: Some("/nonexistent/tailscale".into()),
        // When asked to host, pick any free port (on localhost, without Tailscale).
        port: 0,
        ..AppConfig::default()
    };
    Session::start(SessionOptions { paths: Paths::under(home), config, client_name: "test".into() }).await.unwrap()
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn has_text(s: &Session, channel: &str, text: &str) -> bool {
    s.state().messages(channel).iter().any(|m| m.text == text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_between_two_sessions() {
    let home = tmp("chat");
    let srv = start_server(Some(home.join("history"))).await;
    let addr = srv.local_addr().to_string();
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    alice.join(&addr);
    bob.join(&addr);
    wait_for("both connected", || alice.state().conn.is_connected() && bob.state().conn.is_connected()).await;
    wait_for("alice sees bob", || alice.state().members.len() == 2).await;
    assert_eq!(alice.state().server.as_ref().unwrap().name, "test room");
    assert_eq!(alice.state().focus.as_deref(), Some("general"));

    alice.submit(None, "hello bob");
    wait_for("bob gets message", || has_text(&bob, "general", "hello bob")).await;
    let msg = bob.state().messages("general").iter().find(|m| m.text == "hello bob").cloned().unwrap();
    assert_eq!(msg.author, "alice");
    assert_eq!(msg.kind, MessageKind::User);

    // Slash commands are handled locally and change server state.
    bob.submit(None, "/nick bobby");
    wait_for("nick change seen", || alice.state().members.values().any(|m| m.nick == "bobby")).await;
    // `//` sends a literal slash.
    bob.submit(None, "//shrug");
    wait_for("literal slash", || has_text(&alice, "general", "/shrug")).await;

    // Mod-defined app payloads are relayed untouched.
    let mut events = bob.subscribe();
    alice.send_app("dice", Some("general".into()), None, json!({"roll": 6}));
    let ev = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let ev = events.recv().await.unwrap();
            if ev.event == "app.message" {
                break ev;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ev.data["ns"], "dice");
    assert_eq!(ev.data["data"]["roll"], 6);

    // Server-side posts.
    srv.say(None, "maintenance at 5").unwrap();
    wait_for("server message", || has_text(&alice, "general", "maintenance at 5")).await;

    // History survives a server restart.
    alice.leave();
    bob.leave();
    srv.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let srv2 = start_server(Some(home.join("history"))).await;
    let carol = session("carol", &home.join("c")).await;
    carol.join(srv2.local_addr().to_string());
    wait_for("history replayed", || has_text(&carol, "general", "hello bob")).await;
    assert!(has_text(&carol, "general", "maintenance at 5"));
    srv2.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_mentions_and_new_divider() {
    use tailchat_core::config::ChannelSpec;
    use tailchat_core::proto::ChannelKind;
    let home = tmp("unread");
    let mut config = ServerConfig { name: "test room".into(), rate_limit: 100, ..ServerConfig::default() };
    config.channels.insert(1, ChannelSpec { id: "random".into(), name: None, kind: ChannelKind::Text, topic: None });
    let srv = server::start(ServerOptions {
        config,
        mode: ServerMode::Headless,
        bind: "127.0.0.1:0".parse().unwrap(),
        history_dir: None,
        tailscale: None,
        room_id: "unread".into(),
        term: 1,
    })
    .await
    .unwrap();
    let addr = srv.local_addr().to_string();
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    alice.join(&addr);
    bob.join(&addr);
    wait_for("alice sees bob", || alice.state().members.len() == 2 && bob.state().conn.is_connected()).await;
    assert_eq!(alice.state().focus.as_deref(), Some("general"));

    bob.submit(Some("random"), "anyone here?");
    bob.submit(Some("random"), "hey @Alice, look");
    wait_for("alice gets both", || alice.state().messages("random").len() >= 2).await;
    {
        let s = alice.state();
        assert_eq!(s.unread.get("random"), Some(&2));
        assert_eq!(s.mentions.get("random"), Some(&1));
        let first = s.messages("random").iter().find(|m| m.text == "anyone here?").unwrap().ts;
        assert_eq!(s.new_since.get("random"), Some(&first), "divider goes above the first unseen message");
        let ping = s.messages("random").iter().find(|m| m.text.contains("@Alice")).unwrap();
        assert!(s.mentions_me(ping));
        assert!(!bob.state().mentions_me(ping), "your own messages never mention you");
        assert!(!s.unread.contains_key("general"));
    }

    // Opening the channel clears the badges but keeps the divider while you read.
    alice.focus_channel("random");
    {
        let s = alice.state();
        assert!(!s.unread.contains_key("random") && !s.mentions.contains_key("random"));
        assert!(s.new_since.contains_key("random"));
    }
    // Replying, or moving on, clears it.
    alice.focus_channel("general");
    assert!(!alice.state().new_since.contains_key("random"));
    srv.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_api_drives_a_session() {
    let home = tmp("api");
    let srv = start_server(None).await;
    let addr = srv.local_addr().to_string();
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    alice.join(&addr);
    bob.join(&addr);
    wait_for("connected", || alice.state().conn.is_connected() && bob.state().conn.is_connected()).await;

    let env: std::collections::HashMap<_, _> = alice.api_env().into_iter().collect();
    let mut api = ApiClient::connect(&env["TAILCHAT_API"], &env["TAILCHAT_API_TOKEN"]).await.unwrap();
    let methods = api.call("api.methods", json!({})).await.unwrap();
    assert!(methods["methods"].as_array().unwrap().iter().any(|m| m["name"] == "ui.panel.set"));
    let state = api.call("state.get", json!({})).await.unwrap();
    assert_eq!(state["nick"], "alice");
    assert_eq!(state["members"].as_array().unwrap().len(), 2);

    api.call("message.send", json!({"text": "via api"})).await.unwrap();
    wait_for("api message", || has_text(&bob, "general", "via api")).await;

    api.call("ui.panel.set", json!({"panel": "agent/notes", "title": "Notes", "widgets": [{"type": "heading", "text": "hi"}]}))
        .await
        .unwrap();
    assert_eq!(alice.ui().panel("agent/notes").unwrap().title, "Notes");

    // Bad tokens are refused.
    let mut bad = ApiClient::connect(&env["TAILCHAT_API"], "nope").await.unwrap();
    assert!(bad.call("state.get", json!({})).await.is_err());

    // Event stream.
    let sub = ApiClient::connect(&env["TAILCHAT_API"], &env["TAILCHAT_API_TOKEN"]).await.unwrap();
    let mut stream = sub.subscribe(&["message.*".into()]).await.unwrap();
    bob.submit(None, "streamed");
    let ev = tokio::time::timeout(Duration::from_secs(3), stream.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(ev.event, "message.created");
    assert_eq!(ev.data["message"]["text"], "streamed");
    srv.shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mods_commands_and_hooks() {
    let home = tmp("mods");
    let mod_dir = home.join("a/config/mods/test-bot");
    std::fs::create_dir_all(&mod_dir).unwrap();
    std::fs::write(
        mod_dir.join("tailchat-mod.toml"),
        r#"
id = "test.bot"
name = "Test bot"
version = "0.1.0"

[[commands]]
name = "roll"
description = "roll a die"
command = ["sh", "-c", "echo rolled $1", "sh"]
output = "send"

[[hooks]]
on = "message.created"
command = ["sh", "-c", "grep -q '\"text\":\"ping\"' && echo pong; exit 0"]
output = "send"

[[panels]]
id = "status"
title = "Bot status"
command = ["sh", "-c", "echo '[{\"type\":\"kv\",\"key\":\"node\",\"value\":\"'$TAILCHAT_NODE'\"}]'"]
"#,
    )
    .unwrap();
    let srv = start_server(None).await;
    let addr = srv.local_addr().to_string();
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    assert_eq!(alice.mods().active().count(), 1, "problems: {:?}", alice.mods().problems());
    alice.join(&addr);
    bob.join(&addr);
    wait_for("connected", || alice.state().conn.is_connected() && bob.state().conn.is_connected()).await;

    alice.submit(None, "/roll 6");
    wait_for("mod command output sent", || has_text(&bob, "general", "rolled 6")).await;

    bob.submit(None, "ping");
    wait_for("hook replied", || has_text(&bob, "general", "pong")).await;
    // The hook must not answer its own "pong" (no loops): exactly one pong.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let pongs = bob.state().messages("general").iter().filter(|m| m.text == "pong").count();
    assert_eq!(pongs, 1);

    wait_for("panel filled", || alice.ui().panel("test.bot/status").is_some_and(|p| !p.widgets.is_empty())).await;

    // Disabling via mods.toml + reload removes it.
    std::fs::write(home.join("a/config/mods.toml"), "disabled = [\"test.bot\"]\n").unwrap();
    alice.reload_mods();
    assert_eq!(alice.mods().active().count(), 0);
    alice.submit(None, "/roll 3");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!has_text(&bob, "general", "rolled 3"));
    srv.shutdown();
}

/// Minimal raw protocol client for testing the server without a Session.
async fn raw_join(addr: SocketAddr, nick: &str) -> (TcpStream, u32, u64) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nick: nick.into(),
        client: "raw".into(),
        codecs: vec![Codec::Opus, Codec::Pcm16],
        features: vec![],
    };
    write_frame(&mut s, &hello).await.unwrap();
    match read_frame::<_, ServerMsg>(&mut s).await.unwrap().unwrap() {
        ServerMsg::Welcome { you, voice, .. } => (s, you, voice.token),
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_relay_and_moderation() {
    let srv = start_server(None).await;
    let addr = srv.local_addr();
    let (mut a, a_id, a_tok) = raw_join(addr, "a").await;
    let (mut b, _b_id, b_tok) = raw_join(addr, "b").await;
    write_frame(&mut a, &ClientMsg::VoiceJoin { channel: "lounge".into() }).await.unwrap();
    write_frame(&mut b, &ClientMsg::VoiceJoin { channel: "lounge".into() }).await.unwrap();
    let ua = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    ua.connect(addr).await.unwrap();
    ub.connect(addr).await.unwrap();
    // Register both and wait for ACKs.
    let mut buf = [0u8; 1500];
    for (sock, tok) in [(&ua, a_tok), (&ub, b_tok)] {
        sock.send(&Packet::Register { token: tok }.to_vec()).await.unwrap();
        let n = tokio::time::timeout(Duration::from_secs(2), sock.recv(&mut buf)).await.unwrap().unwrap();
        assert_eq!(Packet::parse(&buf[..n]), Some(Packet::Ack));
    }
    // Give the server a moment to process both VoiceJoins.
    wait_for("both in voice", || srv.members().iter().all(|m| m.voice.as_deref() == Some("lounge"))).await;

    let payload = [7u8; 40];
    ua.send(&Packet::Audio { token: a_tok, seq: 1, codec: Codec::Opus, payload: &payload }.to_vec()).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(2), ub.recv(&mut buf)).await.unwrap().unwrap();
    match Packet::parse(&buf[..n]) {
        Some(Packet::Relay { speaker, seq, codec, payload: p }) => {
            assert_eq!((speaker, seq, codec), (a_id, 1, Codec::Opus));
            assert_eq!(p, payload);
        }
        other => panic!("expected relay, got {other:?}"),
    }

    // A forged token is dropped silently.
    ua.send(&Packet::Audio { token: 12345, seq: 2, codec: Codec::Opus, payload: &payload }.to_vec()).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), ub.recv(&mut buf)).await.is_err());

    // Muted speakers are not relayed.
    write_frame(&mut a, &ClientMsg::VoiceState { muted: true, deafened: false }).await.unwrap();
    wait_for("muted", || srv.members().iter().any(|m| m.id == a_id && m.muted)).await;
    ua.send(&Packet::Audio { token: a_tok, seq: 3, codec: Codec::Opus, payload: &payload }.to_vec()).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), ub.recv(&mut buf)).await.is_err());

    // Kick sends Bye and closes.
    assert!(srv.kick(a_id, "testing"));
    let bye = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match read_frame::<_, ServerMsg>(&mut a).await.unwrap() {
                Some(ServerMsg::Bye { reason }) => break reason,
                Some(_) => continue,
                None => panic!("closed without bye"),
            }
        }
    })
    .await
    .unwrap();
    assert!(bye.contains("testing"));
    wait_for("a removed", || srv.members().len() == 1).await;

    // Probe works without joining.
    let info = tailchat_core::net::probe(addr, Duration::from_secs(2)).await.unwrap();
    assert!(matches!(info, ServerMsg::ProbeInfo { members: 1, .. }));
    drop(b);
    srv.shutdown();
}

fn term_of(s: &Session) -> u64 {
    s.state().server.as_ref().map(|x| x.term).unwrap_or(0)
}

fn on_term(s: &Session, term: u64) -> bool {
    s.state().conn.is_connected() && term_of(s) == term
}

/// Host a new room from `s` on localhost and return the address to join.
async fn host(s: &Session, name: &str) -> String {
    s.host(HostOptions { name: name.into(), bind: "localhost".into(), port: 0 });
    wait_for("hosting", || s.state().hosting.is_some() && s.state().conn.is_connected()).await;
    s.state().hosting.as_ref().unwrap().share[0].clone()
}

fn member_id(viewer: &Session, nick: &str) -> u32 {
    viewer.state().members.values().find(|m| m.nick == nick).map(|m| m.id).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_fills_gaps_both_ways() {
    let home = tmp("sync");
    let alice = session("alice", &home.join("a")).await;
    let srv = start_room(None, "room-sync", 1).await;
    alice.join(srv.local_addr().to_string());
    wait_for("connected", || alice.state().conn.is_connected()).await;
    alice.submit(None, "said on the first host");
    wait_for("echo", || has_text(&alice, "general", "said on the first host")).await;
    let file = rooms::history_dir(&Paths::under(&home.join("a")), "room-sync").join("general.jsonl");
    wait_for("alice's copy on disk", || std::fs::read_to_string(&file).is_ok_and(|t| t.contains("said on the first host"))).await;
    let save = rooms::load(&Paths::under(&home.join("a")), "room-sync").unwrap();
    assert_eq!((save.name.as_str(), save.term), ("test room", 1));
    alice.leave();
    srv.shutdown();

    // A later host of the same room that never saw that message.
    let srv2 = start_room(Some(home.join("h2")), "room-sync", 2).await;
    srv2.say(None, "only the new host has this").unwrap();
    alice.join(srv2.local_addr().to_string());
    wait_for("host got alice's copy", || srv2.history("general", 50).iter().any(|m| m.text == "said on the first host")).await;
    wait_for("alice got the host's", || has_text(&alice, "general", "only the new host has this")).await;
    let bob = session("bob", &home.join("b")).await;
    bob.join(srv2.local_addr().to_string());
    wait_for("bob sees both", || {
        has_text(&bob, "general", "said on the first host") && has_text(&bob, "general", "only the new host has this")
    })
    .await;
    // Ids stay time-ordered across hosts.
    let ids: Vec<u64> = srv2.history("general", 50).iter().map(|m| m.id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    srv2.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn planned_handoff_moves_everyone() {
    let home = tmp("handoff");
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let carol = session("carol", &home.join("c")).await;
    let addr = host(&alice, "den").await;
    bob.join(&addr);
    carol.join(&addr);
    wait_for("all in", || alice.state().members.len() == 3).await;
    assert!(alice.state().members.values().all(|m| m.can_host));
    alice.submit(None, "before the move");
    wait_for("seen", || has_text(&carol, "general", "before the move") && has_text(&bob, "general", "before the move")).await;

    let bob_id = member_id(&alice, "bob");
    alice.handoff(Some(bob_id));
    wait_for("bob hosts", || bob.state().hosting.as_ref().is_some_and(|h| h.term == 2)).await;
    wait_for("everyone moved", || on_term(&alice, 2) && on_term(&bob, 2) && on_term(&carol, 2)).await;
    wait_for("all in again", || bob.state().members.len() == 3).await;
    assert_eq!(bob.state().server.as_ref().unwrap().name, "den");
    wait_for("old host stops", || alice.state().hosting.is_none()).await;

    carol.submit(None, "after the move");
    wait_for("alice hears carol", || has_text(&alice, "general", "after the move")).await;
    assert!(has_text(&carol, "general", "before the move"), "history stays on screen across the move");
    // Someone new sees the history from before the move.
    let dave = session("dave", &home.join("d")).await;
    dave.join(bob.state().hosting.as_ref().unwrap().share[0].clone());
    wait_for("dave sees history", || has_text(&dave, "general", "before the move") && has_text(&dave, "general", "after the move")).await;
    // Everyone saved the room with the new term.
    for (who, dir) in [("alice", "a"), ("carol", "c")] {
        let save = rooms::list(&Paths::under(&home.join(dir))).into_iter().next().unwrap_or_else(|| panic!("{who} has no save"));
        assert_eq!((save.name.as_str(), save.term), ("den", 2), "{who}");
    }
    bob.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_host_hands_the_room_on() {
    let home = tmp("leave");
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let addr = host(&alice, "den").await;
    bob.join(&addr);
    wait_for("both in", || alice.state().members.len() == 2).await;
    alice.submit(None, "bye all");
    wait_for("seen", || has_text(&bob, "general", "bye all")).await;
    alice.leave();
    wait_for("bob took over", || on_term(&bob, 2) && bob.state().hosting.is_some()).await;
    assert!(has_text(&bob, "general", "bye all"));
    assert!(alice.state().server.is_none());
    bob.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_this_room_after_the_host_vanishes() {
    let home = tmp("takeover");
    let srv = start_room(Some(home.join("srv")), "room-gone", 1).await;
    let addr = srv.local_addr().to_string();
    let alice = session("alice", &home.join("a")).await;
    let carol = session("carol", &home.join("c")).await;
    alice.join(&addr);
    carol.join(&addr);
    wait_for("both in", || alice.state().members.len() == 2).await;
    alice.submit(None, "remember this");
    wait_for("carol has it", || has_text(&carol, "general", "remember this")).await;

    // The host disappears without handing over.
    srv.shutdown();
    wait_for("carol notices", || carol.state().host_lost()).await;
    carol.open_room("room-gone");
    wait_for("carol hosts", || carol.state().hosting.as_ref().is_some_and(|h| h.term == 2) && on_term(&carol, 2)).await;
    assert!(has_text(&carol, "general", "remember this"));
    alice.join(carol.state().hosting.as_ref().unwrap().share[0].clone());
    wait_for("alice back", || on_term(&alice, 2) && carol.state().members.len() == 2).await;
    carol.submit(None, "still here");
    wait_for("alice hears", || has_text(&alice, "general", "still here")).await;
    assert!(has_text(&alice, "general", "remember this"));
    carol.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_takes_the_room_back() {
    let home = tmp("claim");
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let addr = host(&alice, "den").await;
    bob.join(&addr);
    wait_for("both in", || alice.state().members.len() == 2).await;
    alice.submit(None, "made in the app");
    wait_for("seen", || has_text(&bob, "general", "made in the app")).await;

    // An always-on server starts with (a copy of) the room's save: it finds
    // the room being hosted at its last address and claims it.
    let save = rooms::list(&Paths::under(&home.join("a"))).remove(0);
    let base = ServerConfig { bind: "localhost".into(), port: 0, rate_limit: 100, ..ServerConfig::default() };
    let node = ServerNode::start(
        save.server_config(base),
        save,
        Paths::under(&home.join("srv")),
        NodeOptions { api: false, mods: false, tailscale_bin: Some("/nonexistent/tailscale".into()) },
    )
    .await
    .unwrap();
    assert!(node.claimed_from().is_some());
    wait_for("everyone on the server", || on_term(&alice, 2) && on_term(&bob, 2)).await;
    assert_eq!(alice.state().server.as_ref().unwrap().mode, "server");
    wait_for("server has the app's history", || node.server().history("general", 50).iter().any(|m| m.text == "made in the app")).await;
    wait_for("alice stops hosting", || alice.state().hosting.is_none()).await;
    // A stale term can't take it back.
    let err = rooms::claim(&node.local_addr().to_string(), 0, &node.save().id, 2, vec!["127.0.0.1:1".into()]).await.unwrap_err();
    assert!(err.to_string().contains("term"), "{err}");
    node.shutdown();
}
