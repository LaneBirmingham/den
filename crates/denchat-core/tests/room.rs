//! End-to-end tests: real servers, real sessions, iroh endpoints on loopback
//! (direct connections only: no relays, no discovery).

use std::path::PathBuf;
use std::time::Duration;

use denchat_core::api::ApiClient;
use denchat_core::config::{AppConfig, NetConfig, RelayServeConfig, ServerConfig};
use denchat_core::net::{self, Link, Net, SecretKey, read_frame, write_frame};
use denchat_core::paths::Paths;
use denchat_core::proto::voice::{Codec, Packet};
use denchat_core::proto::{ClientMsg, FEATURE_HOST, MessageKind, PROTOCOL_VERSION, Reach, ServerMsg};
use denchat_core::rooms;
use denchat_core::server::{self, ServerHandle, ServerMode, ServerOptions};
use denchat_core::server_node::{NodeOptions, ServerNode};
use denchat_core::session::{ConnState, HostOptions, Session, SessionOptions};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde_json::json;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("den-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_server(history: Option<PathBuf>) -> ServerHandle {
    start_room(history, "test-room", 1).await
}

/// An endpoint on loopback, direct connections only.
async fn test_net(key: SecretKey, port: u16) -> Net {
    Net::bind(key, "localhost", port, true, &NetConfig::direct_only()).await.unwrap()
}

async fn start_room(history: Option<PathBuf>, room_id: &str, term: u64) -> ServerHandle {
    let config = ServerConfig { name: "test room".into(), rate_limit: 100, ..ServerConfig::default() };
    start_with(config, test_net(SecretKey::generate(), 0).await, history, room_id, term, None).await
}

async fn start_with(
    config: ServerConfig,
    net: Net,
    history: Option<PathBuf>,
    room_id: &str,
    term: u64,
    invite: Option<String>,
) -> ServerHandle {
    server::start(ServerOptions {
        config,
        mode: ServerMode::Headless,
        net,
        history_dir: history,
        room_id: room_id.into(),
        term,
        invite,
        profiles: Vec::new(),
    })
    .await
    .unwrap()
}

/// The invite link to a running room, as members paste it.
async fn link(srv: &ServerHandle) -> String {
    srv.link().await.to_string()
}

async fn session(name: &str, home: &std::path::Path) -> Session {
    let config = AppConfig {
        nick: Some(name.into()),
        // Loopback only, and never the public relays.
        bind: "localhost".into(),
        port: 0,
        net: NetConfig::direct_only(),
        ..AppConfig::default()
    };
    Session::start(SessionOptions { paths: Paths::under(home), config, client_name: "test".into() }).await.unwrap()
}

/// One process per profile: a second session on the same data folder would
/// share the endpoint key, and the relay keeps only the newest of the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_per_profile() {
    let home = tmp("profile-lock");
    let alice = session("alice", &home).await;
    let config = AppConfig { net: NetConfig::direct_only(), ..AppConfig::default() };
    let second = Session::start(SessionOptions { paths: Paths::under(&home), config: config.clone(), client_name: "test".into() }).await;
    let err = second.err().expect("second session on one profile");
    assert!(err.downcast_ref::<net::AlreadyRunning>().is_some(), "{err:#}");
    // Free again once the first has closed.
    alice.close().await;
    Session::start(SessionOptions { paths: Paths::under(&home), config, client_name: "test".into() }).await.unwrap().shutdown();
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
    let addr = link(&srv).await;
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    alice.join(&addr);
    bob.join(&addr);
    wait_for("both connected", || alice.state().conn.is_connected() && bob.state().conn.is_connected()).await;
    wait_for("alice sees bob", || alice.state().members.len() == 2).await;
    // The first ping reports how the link travels: loopback with no relays is direct.
    wait_for("route known", || bob.state().latency_ms.is_some()).await;
    assert_eq!(bob.state().via, Some(net::Via::Direct));
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
    carol.join(link(&srv2).await);
    wait_for("history replayed", || has_text(&carol, "general", "hello bob")).await;
    assert!(has_text(&carol, "general", "maintenance at 5"));
    srv2.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_mentions_and_new_divider() {
    use denchat_core::config::ChannelSpec;
    use denchat_core::proto::ChannelKind;
    let home = tmp("unread");
    let mut config = ServerConfig { name: "test room".into(), rate_limit: 100, ..ServerConfig::default() };
    config.channels.insert(1, ChannelSpec { id: "random".into(), name: None, kind: ChannelKind::Text, topic: None });
    let srv = start_with(config, test_net(SecretKey::generate(), 0).await, None, "unread", 1, None).await;
    let addr = link(&srv).await;
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
    let addr = link(&srv).await;
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    alice.join(&addr);
    bob.join(&addr);
    wait_for("connected", || alice.state().conn.is_connected() && bob.state().conn.is_connected()).await;

    let env: std::collections::HashMap<_, _> = alice.api_env().into_iter().collect();
    let mut api = ApiClient::connect(&env["DEN_API"], &env["DEN_API_TOKEN"]).await.unwrap();
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
    let mut bad = ApiClient::connect(&env["DEN_API"], "nope").await.unwrap();
    assert!(bad.call("state.get", json!({})).await.is_err());

    // Event stream.
    let sub = ApiClient::connect(&env["DEN_API"], &env["DEN_API_TOKEN"]).await.unwrap();
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
        mod_dir.join("den-mod.toml"),
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
command = ["sh", "-c", "echo '[{\"type\":\"kv\",\"key\":\"node\",\"value\":\"'$DEN_NODE'\"}]'"]
"#,
    )
    .unwrap();
    let srv = start_server(None).await;
    let addr = link(&srv).await;
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
async fn raw_join(net: &Net, room: &Link, nick: &str) -> (Connection, SendStream, RecvStream, u32, u64) {
    raw_join_as(net, room, nick, vec![], None, Reach::Unknown).await
}

/// [`raw_join`], saying more about ourselves in `hello`.
async fn raw_join_as(
    net: &Net,
    room: &Link,
    nick: &str,
    features: Vec<String>,
    addr: Option<String>,
    reach: Reach,
) -> (Connection, SendStream, RecvStream, u32, u64) {
    let conn = net.connect(room.addr.clone(), Duration::from_secs(5)).await.unwrap();
    let (mut wr, mut rd) = conn.open_bi().await.unwrap();
    let id = room.room.clone().unwrap();
    let member = SecretKey::generate();
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nick: nick.into(),
        client: "raw".into(),
        codecs: vec![Codec::Opus, Codec::Pcm16],
        features,
        room: Some(id.clone()),
        proof: net::prove(&conn, &id, Some(&member), None).unwrap(),
        addr,
        reach,
    };
    write_frame(&mut wr, &hello).await.unwrap();
    match read_frame::<_, ServerMsg>(&mut rd).await.unwrap().unwrap() {
        ServerMsg::Welcome { you, voice, .. } => (conn, wr, rd, you, voice.token),
        other => panic!("expected welcome, got {other:?}"),
    }
}

async fn datagram(c: &Connection, wait: Duration) -> Option<Vec<u8>> {
    tokio::time::timeout(wait, c.read_datagram()).await.ok()?.ok().map(|b| b.to_vec())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_relay_and_moderation() {
    let srv = start_server(None).await;
    let room = srv.link().await;
    let (na, nb) = (test_net(SecretKey::generate(), 0).await, test_net(SecretKey::generate(), 0).await);
    let (ca, mut a, mut ar, a_id, a_tok) = raw_join(&na, &room, "a").await;
    let (cb, mut b, _br, _b_id, b_tok) = raw_join(&nb, &room, "b").await;
    write_frame(&mut a, &ClientMsg::VoiceJoin { channel: "lounge".into() }).await.unwrap();
    write_frame(&mut b, &ClientMsg::VoiceJoin { channel: "lounge".into() }).await.unwrap();
    // Register both and wait for ACKs.
    for (c, tok) in [(&ca, a_tok), (&cb, b_tok)] {
        c.send_datagram(Packet::Register { token: tok }.to_vec().into()).unwrap();
        let d = datagram(c, Duration::from_secs(2)).await.expect("ack");
        assert_eq!(Packet::parse(&d), Some(Packet::Ack));
    }
    // Give the server a moment to process both VoiceJoins.
    wait_for("both in voice", || srv.members().iter().all(|m| m.voice.as_deref() == Some("lounge"))).await;

    let payload = [7u8; 40];
    let audio = |token, seq| Packet::Audio { token, seq, codec: Codec::Opus, payload: &payload }.to_vec();
    ca.send_datagram(audio(a_tok, 1).into()).unwrap();
    let d = datagram(&cb, Duration::from_secs(2)).await.expect("relay");
    match Packet::parse(&d) {
        Some(Packet::Relay { speaker, seq, codec, payload: p }) => {
            assert_eq!((speaker, seq, codec), (a_id, 1, Codec::Opus));
            assert_eq!(p, payload);
        }
        other => panic!("expected relay, got {other:?}"),
    }

    // Another member's token on our connection is dropped silently.
    ca.send_datagram(audio(b_tok, 2).into()).unwrap();
    assert!(datagram(&cb, Duration::from_millis(300)).await.is_none());

    // Muted speakers are not relayed.
    write_frame(&mut a, &ClientMsg::VoiceState { muted: true, deafened: false }).await.unwrap();
    wait_for("muted", || srv.members().iter().any(|m| m.id == a_id && m.muted)).await;
    ca.send_datagram(audio(a_tok, 3).into()).unwrap();
    assert!(datagram(&cb, Duration::from_millis(300)).await.is_none());

    // Kick sends Bye and closes.
    assert!(srv.kick(a_id, "testing"));
    let bye = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match read_frame::<_, ServerMsg>(&mut ar).await.unwrap() {
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
    let info = net::probe(&na, &room, room.room.as_deref(), Duration::from_secs(2)).await.unwrap();
    assert!(matches!(info, ServerMsg::ProbeInfo { members: 1, .. }));
    // A room this endpoint doesn't host says so.
    let other = room.clone().room("no-such-room");
    let none = net::probe(&na, &other, Some("no-such-room"), Duration::from_secs(2)).await.unwrap();
    assert!(matches!(none, ServerMsg::Error { ref code, .. } if code == "no_room"), "{none:?}");
    drop((b, cb));
    srv.shutdown();
}

fn term_of(s: &Session) -> u64 {
    s.state().server.as_ref().map(|x| x.term).unwrap_or(0)
}

fn on_term(s: &Session, term: u64) -> bool {
    s.state().conn.is_connected() && term_of(s) == term
}

/// Host a new room from `s` and return its invite link.
async fn host(s: &Session, name: &str) -> String {
    s.host(HostOptions { name: name.into(), open: false });
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
    alice.join(link(&srv).await);
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
    alice.join(link(&srv2).await);
    wait_for("host got alice's copy", || srv2.history("general", 50).iter().any(|m| m.text == "said on the first host")).await;
    wait_for("alice got the host's", || has_text(&alice, "general", "only the new host has this")).await;
    let bob = session("bob", &home.join("b")).await;
    bob.join(link(&srv2).await);
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
    let addr = link(&srv).await;
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
    let node = ServerNode::start(save.server_config(local_base(0)), save, Paths::under(&home.join("srv")), node_opts()).await.unwrap();
    assert!(node.claimed_from().is_some());
    wait_for("everyone on the server", || on_term(&alice, 2) && on_term(&bob, 2)).await;
    assert_eq!(alice.state().server.as_ref().unwrap().mode, "server");
    wait_for("server has the app's history", || node.server().history("general", 50).iter().any(|m| m.text == "made in the app")).await;
    wait_for("alice stops hosting", || alice.state().hosting.is_none()).await;
    // A stale term can't take it back.
    let net = test_net(SecretKey::generate(), 0).await;
    let member = rooms::member_key(&Paths::under(&home.join("x")), &node.save().id).unwrap();
    let addr = node.invite_link().await.without_invite().to_string();
    let err = rooms::claim(&net, &addr, node.save(), &member, 2, vec![addr.clone()]).await.unwrap_err();
    assert!(err.to_string().contains("term"), "{err}");
    // Without the invite, no claim at all.
    let mut uninvited = node.save().clone();
    uninvited.invite = Some(net::new_invite());
    let err = rooms::claim(&net, &addr, &uninvited, &member, 9, vec![addr.clone()]).await.unwrap_err();
    assert!(err.to_string().contains("invite"), "{err}");
    node.shutdown();
}

fn node_opts() -> NodeOptions {
    NodeOptions { api: false, mods: false }
}

fn local_base(port: u16) -> ServerConfig {
    ServerConfig {
        bind: "localhost".into(),
        port,
        rate_limit: 100,
        net: NetConfig::direct_only(),
        // Most tests don't need the room's relay (see `serve_relay_gets_members_in`).
        relay: RelayServeConfig { enabled: false, ..RelayServeConfig::default() },
        ..ServerConfig::default()
    }
}

/// A UDP port that is free right now.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// BUG-A: messages a member uploads because the host lacked them show up in
/// that member's own view, not only on disk and on the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploaded_messages_show_for_the_uploader() {
    let home = tmp("upload-view");
    let srv = start_room(None, "room-up", 1).await;
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    alice.join(link(&srv).await);
    bob.join(link(&srv).await);
    wait_for("both in", || alice.state().members.len() == 2).await;
    alice.submit(None, "everyone saw this");
    wait_for("bob has it", || has_text(&bob, "general", "everyone saw this")).await;
    bob.leave();
    // Only alice is around for these.
    for i in 1..=3 {
        alice.submit(None, &format!("solo {i}"));
    }
    wait_for("alice's copy", || {
        let f = rooms::history_dir(&Paths::under(&home.join("a")), "room-up").join("general.jsonl");
        std::fs::read_to_string(f).is_ok_and(|t| t.contains("solo 3"))
    })
    .await;
    alice.leave();
    srv.shutdown();

    // Bob hosts from his older copy; alice comes back to it.
    bob.open_room("room-up");
    wait_for("bob hosts", || bob.state().hosting.is_some() && on_term(&bob, 2)).await;
    alice.join(bob.state().hosting.as_ref().unwrap().share[0].clone());
    wait_for("bob got alice's messages", || has_text(&bob, "general", "solo 3")).await;
    wait_for("alice sees her own uploaded messages", || (1..=3).all(|i| has_text(&alice, "general", &format!("solo {i}")))).await;
    assert!(has_text(&alice, "general", "everyone saw this"));
    let texts: Vec<String> = alice.state().messages("general").iter().filter(|m| m.id != 0).map(|m| m.text.clone()).collect();
    assert_eq!(texts.iter().filter(|t| t.as_str() == "solo 1").count(), 1, "no duplicates: {texts:?}");
    // History files never carry per-host member ids.
    let file = rooms::history_dir(&Paths::under(&home.join("b")), "room-up").join("general.jsonl");
    wait_for("bob's file has them", || std::fs::read_to_string(&file).is_ok_and(|t| t.contains("solo 3"))).await;
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains("author_id\":") || text.contains("author_id\":null"), "{text}");
    bob.shutdown();
    alice.shutdown();
}

/// BUG-C: closing twice at once (a signal while the window closes) hands the
/// room on once and returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_hands_off_once_even_when_called_twice() {
    let home = tmp("close-twice");
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let addr = host(&alice, "den").await;
    bob.join(&addr);
    wait_for("both in", || alice.state().members.len() == 2).await;
    let started = std::time::Instant::now();
    tokio::join!(alice.close(), alice.close());
    assert!(started.elapsed() < Duration::from_secs(7), "close is bounded");
    wait_for("bob took over", || on_term(&bob, 2) && bob.state().hosting.is_some()).await;
    assert!(has_text(&bob, "general", "you're hosting den now"), "a planned handoff says so, like a takeover");
    assert!(bob.state().is_host());
    // Nobody to hand to: close returns promptly.
    let started = std::time::Instant::now();
    bob.close().await;
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// BUG-E: `serve` going down hands the room to a member who can host; when
/// it comes back, it claims the room back from wherever it went.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_hands_off_on_shutdown_and_claims_back() {
    let home = tmp("serve-close");
    let srv_paths = Paths::under(&home.join("srv"));
    let save = rooms::RoomSave::new(&ServerConfig { name: "always on".into(), ..ServerConfig::default() });
    let id = save.id.clone();
    let node = ServerNode::start(save.server_config(local_base(0)), save, srv_paths.clone(), node_opts()).await.unwrap();
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let invite = node.invite_link().await.to_string();
    alice.join(&invite);
    bob.join(&invite);
    wait_for("both in", || node.server().members().len() == 2 && on_term(&alice, 1) && on_term(&bob, 1)).await;
    alice.submit(None, "while the server ran");
    wait_for("bob has it", || has_text(&bob, "general", "while the server ran")).await;

    node.close().await;
    wait_for("a member hosts", || {
        on_term(&alice, 2) && on_term(&bob, 2) && (alice.state().hosting.is_some() || bob.state().hosting.is_some())
    })
    .await;
    let host_addr = [&alice, &bob].iter().find_map(|s| s.state().hosting.as_ref().map(|h| h.addr.clone())).unwrap();
    // The server remembers where the room went.
    let saved = rooms::load(&srv_paths, &id).unwrap();
    assert_eq!((saved.term, &saved.addrs), (2, &vec![host_addr]));

    let node2 = ServerNode::start(saved.server_config(local_base(0)), saved, srv_paths.clone(), node_opts()).await.unwrap();
    assert!(node2.claimed_from().is_some());
    wait_for("everyone back on the server", || on_term(&alice, 3) && on_term(&bob, 3)).await;
    assert_eq!(alice.state().server.as_ref().unwrap().mode, "server");
    wait_for("server kept the history", || node2.server().history("general", 50).iter().any(|m| m.text == "while the server ran")).await;
    node2.shutdown();
    // Nobody can host after the apps leave: close is quick.
    alice.shutdown();
    bob.shutdown();
}

/// `serve` and an app on one machine (same data folder) are different peers:
/// serve takes the room from the app, and the app moves over like anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_takes_a_room_from_an_app_on_the_same_machine() {
    let home = tmp("same-machine");
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let addr = host(&alice, "den").await;
    bob.join(&addr);
    wait_for("both in", || alice.state().members.len() == 2).await;

    let paths = Paths::under(&home.join("a"));
    let save = rooms::list(&paths).remove(0);
    let node = ServerNode::start(save.server_config(local_base(0)), save, paths, node_opts()).await.unwrap();
    assert_ne!(Some(node.server().net().id().to_z32()), alice.net().map(|n| n.id().to_z32()), "serve has its own key");
    assert!(node.claimed_from().is_some());
    wait_for("everyone on the server", || on_term(&alice, 2) && on_term(&bob, 2)).await;
    assert_eq!(bob.state().server.as_ref().unwrap().mode, "server");
    node.shutdown();
    alice.shutdown();
    bob.shutdown();
}

/// A host that refuses us is alive: no "Host this room" (takeover would only
/// be refused again).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_rejoin_offers_no_takeover() {
    let home = tmp("refused");
    let (key, port) = (SecretKey::generate(), free_port());
    let config = ServerConfig { name: "test room".into(), ..ServerConfig::default() };
    let srv = start_with(config.clone(), test_net(key.clone(), port).await, None, "room-strict", 1, None).await;
    let alice = session("alice", &home.join("a")).await;
    alice.join(link(&srv).await);
    wait_for("in", || alice.state().conn.is_connected()).await;
    srv.shutdown();
    srv.net().close().await;
    drop(srv);
    // The room comes back at the same endpoint with an allow list alice isn't on.
    let config = ServerConfig { allow: vec!["notakey!".into()], ..config };
    let mut net = None;
    for _ in 0..40 {
        match Net::bind(key.clone(), "localhost", port, true, &NetConfig::direct_only()).await {
            Ok(n) => {
                net = Some(n);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let strict = start_with(config, net.expect("the old endpoint let go of its port"), None, "room-strict", 2, None).await;
    wait_for("refused", || matches!(alice.state().conn, ConnState::Failed { refused: true, .. })).await;
    assert!(alice.state().room_id().is_some());
    assert!(!alice.state().host_lost(), "no takeover offer when the host refused us");
    strict.shutdown();
}

/// Invite-only rooms (the default): the link gets you in, the bare address
/// doesn't, and your copy keeps the invite so you can come back without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invites_and_room_profiles() {
    let home = tmp("invites");
    let alice = session("alice", &home.join("a")).await;
    let bob = session("bob", &home.join("b")).await;
    let mallory = session("mallory", &home.join("m")).await;
    let invite = host(&alice, "den").await;
    let parsed = Link::parse(&invite).unwrap();
    assert!(parsed.invite.is_some() && parsed.room.is_some(), "{invite}");

    // No invite, no entry.
    mallory.join(parsed.without_invite().to_string());
    wait_for("refused", || matches!(mallory.state().conn, ConnState::Failed { refused: true, .. })).await;

    bob.join(&invite);
    wait_for("bob in", || alice.state().members.len() == 2 && bob.state().conn.is_connected()).await;
    let bob_key = bob.state().me().and_then(|m| m.identity.key.clone()).expect("bob signed in with his key");
    assert!(bob.state().me().unwrap().identity.verified);
    // The host remembers bob's profile; so does alice's copy.
    let a_paths = Paths::under(&home.join("a"));
    let id = parsed.room.clone().unwrap();
    wait_for("profile saved", || rooms::load(&a_paths, &id).is_ok_and(|s| s.profiles.iter().any(|p| p.key == bob_key && p.nick == "bob")))
        .await;

    // While bob is away, someone else still can't take his nick in this room.
    bob.leave();
    wait_for("bob gone", || alice.state().members.len() == 1).await;
    mallory.join(&invite);
    wait_for("mallory in", || mallory.state().conn.is_connected()).await;
    mallory.submit(None, "/nick bob");
    wait_for("nick decided", || alice.state().members.values().any(|m| m.nick.starts_with("bob ("))).await;

    // Bob's copy kept the invite: the bare address is enough to come back.
    let b_save = rooms::load(&Paths::under(&home.join("b")), &id).unwrap();
    assert_eq!(b_save.invite, parsed.invite);
    bob.join(parsed.without_invite().to_string());
    wait_for("bob back", || bob.state().conn.is_connected()).await;
    assert_eq!(bob.state().me().and_then(|m| m.identity.key.clone()), Some(bob_key), "same key for the same room");
    assert_eq!(bob.state().me().map(|m| m.nick.clone()).as_deref(), Some("bob"));
    alice.shutdown();
    bob.shutdown();
    mallory.shutdown();
}

/// `serve` runs a relay for its room and puts it in the invite link, so a
/// member who can't reach the server directly gets in through the server's
/// own relay (here: a link with no direct addresses at all, and a member
/// whose own configured relay is dead).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_relay_gets_members_in() {
    let home = tmp("serve-relay");
    let relay_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let relay_url = format!("http://127.0.0.1:{relay_port}/");
    let mut base = local_base(0);
    base.relay = RelayServeConfig { enabled: true, port: relay_port, url: Some(relay_url.clone()) };
    let save = rooms::RoomSave::new(&ServerConfig { name: "relayed".into(), ..ServerConfig::default() });
    let node = ServerNode::start(save.server_config(base), save, Paths::under(&home.join("srv")), node_opts()).await.unwrap();
    assert_eq!(node.relay_url().as_deref(), Some(relay_url.as_str()));
    let invite = node.invite_link().await;
    let relays: Vec<String> = invite.addr.relay_urls().map(|u| u.to_string()).collect();
    assert_eq!(relays.len(), 1, "the link names only the server's own relay: {relays:?}");
    assert!(relays[0].starts_with(&format!("{relay_url}?token=")), "with its token: {relays:?}");
    // The link survives being written out and read back.
    assert_eq!(Link::parse(&invite.to_string()).unwrap().addr, invite.addr);

    // Strangers can't use the relay: no token, or a wrong one, gets them
    // nowhere; the link's token does.
    let stranger =
        Net::bind(SecretKey::generate(), "localhost", 0, false, &NetConfig::relays(["http://127.0.0.1:1".to_string()])).await.unwrap();
    let via = |url: &str| iroh::EndpointAddr::from_parts(invite.addr.id, [iroh::TransportAddr::Relay(url.parse().unwrap())]);
    for bad in [relay_url.clone(), format!("{relay_url}?token=nope")] {
        assert!(stranger.connect(via(&bad), Duration::from_secs(3)).await.is_err(), "{bad} let a stranger in");
    }
    stranger.connect(via(&relays[0]), Duration::from_secs(10)).await.expect("the link's token opens the relay");
    stranger.close().await;

    // Only the relay: no direct addresses to try.
    let relay_only =
        Link::new(iroh::EndpointAddr::from_parts(invite.addr.id, invite.addr.relay_urls().cloned().map(iroh::TransportAddr::Relay)))
            .room(invite.room.clone().unwrap())
            .invite(invite.invite.clone());
    let config = AppConfig {
        nick: Some("alice".into()),
        bind: "localhost".into(),
        // A relay that isn't there: the room's relay must come from the link.
        net: NetConfig::relays(["http://127.0.0.1:1".to_string()]),
        ..AppConfig::default()
    };
    let alice = Session::start(SessionOptions { paths: Paths::under(&home.join("a")), config, client_name: "test".into() }).await.unwrap();
    alice.join(relay_only.to_string());
    wait_for("alice in through the relay", || alice.state().conn.is_connected() && node.server().members().len() == 1).await;
    alice.submit(None, "hello via relay");
    wait_for("server has it", || node.server().history("general", 10).iter().any(|m| m.text == "hello via relay")).await;
    alice.shutdown();
    node.close().await;
}

/// The host checks how reachable each member's app is (a fresh endpoint
/// connects to it directly), and a handoff goes to the best placed member,
/// not just the one who's been here longest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handoff_prefers_members_others_can_reach() {
    let home = tmp("reach");
    let srv = start_room(None, "room-reach", 1).await;
    let room = Link::parse(&link(&srv).await).unwrap();
    let reach_of = |nick: &str| srv.members().into_iter().find(|m| m.nick == nick).map(|m| m.reach);

    // Ann joins first and claims a public address, but gives one nobody
    // answers on: the check caps her at `nat`.
    let ann_net = test_net(SecretKey::generate(), 0).await;
    let nowhere = Link::new(iroh::EndpointAddr::from_parts(
        SecretKey::generate().public(),
        [iroh::TransportAddr::Ip("127.0.0.1:1".parse().unwrap())],
    ));
    let (_c, mut ann_wr, _rd, _, _) =
        raw_join_as(&ann_net, &room, "ann", vec![FEATURE_HOST.into()], Some(nowhere.to_string()), Reach::Public).await;
    assert_eq!(reach_of("ann"), Some(Reach::Public), "her claim stands until it's checked");
    wait_for("ann checked", || reach_of("ann") == Some(Reach::Nat)).await;

    // Bob's app answers on its address: `open`.
    let bob = session("bob", &home.join("b")).await;
    bob.join(room.to_string());
    wait_for("bob checked", || reach_of("bob") == Some(Reach::Open)).await;
    wait_for("bob sees it", || bob.state().members.values().any(|m| m.nick == "bob" && m.reach == Reach::Open)).await;

    let picked = srv.hand_off(None, None).unwrap();
    assert_eq!(picked.nick, "bob", "the member others can reach hosts next, though ann was here first");

    // Ann's network changes: her real address is checked, and works.
    let real = ann_net.link().await.to_string();
    write_frame(&mut ann_wr, &ClientMsg::Reach { reach: Reach::Nat, addr: Some(real.clone()) }).await.unwrap();
    wait_for("ann checked again", || reach_of("ann") == Some(Reach::Open)).await;
    let ann = srv.members().into_iter().find(|m| m.nick == "ann").unwrap();
    assert_eq!(ann.addr.as_deref(), Some(real.as_str()));
    bob.shutdown();
    srv.shutdown();
}
