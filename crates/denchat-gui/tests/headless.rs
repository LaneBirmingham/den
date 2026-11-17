//! The real app, headless, driven the way `den ui …` drives it: find
//! widgets, click, type, read the result, screenshot, quit. No window, GPU or
//! network beyond loopback.

use std::time::Duration;

use denchat_core::config::{AppConfig, NetConfig};
use denchat_core::paths::Paths;
use denchat_core::session::{HostOptions, Session, SessionOptions};
use denchat_gui::Mode;
use denchat_gui::panels::Registry;
use serde_json::{Value, json};

async fn call(s: &Session, method: &str, params: Value) -> Value {
    for _ in 0..100 {
        match s.ui().call_frontend(method, params.clone()) {
            Ok(rx) => return tokio::time::timeout(Duration::from_secs(20), rx).await.expect("answer in time").expect("answered").unwrap(),
            // The frontend attaches on its first frame.
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("no frontend attached");
}

async fn tree(s: &Session) -> String {
    call(s, "ui.tree", json!({"format": "text"})).await["text"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_the_app_headless() {
    let home = std::env::temp_dir().join(format!("den-gui-headless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let config = AppConfig { nick: Some("alice".into()), bind: "localhost".into(), net: NetConfig::direct_only(), ..AppConfig::default() };
    let session = Session::start(SessionOptions { paths: Paths::under(&home), config, client_name: "test".into() }).await.unwrap();
    session.host(HostOptions { name: "test room".into(), open: false });

    let rt = tokio::runtime::Handle::current();
    let app = {
        let session = session.clone();
        std::thread::spawn(move || {
            let _guard = rt.enter();
            denchat_gui::run_mode(session, Registry::with_builtins(), Mode::Headless)
        })
    };

    // In the room, the built-in panels name their widgets.
    // `button:`: the lobby's hint ("… room menu → Copy invite link") also
    // contains the words, and shows until the room is up.
    let found = call(&session, "ui.wait", json!({"target": "button:Room menu", "timeout_ms": 15_000})).await;
    assert!(found["found"].as_str().unwrap().contains("test room"), "{found}");
    let t = tree(&session).await;
    for want in ["core.channels", "button \"#general\"", "textbox \"Message #general\"", "item \"alice (you) · Online\""] {
        assert!(t.contains(want), "{want} missing from:\n{t}");
    }

    // Type into the composer and send.
    call(&session, "ui.type", json!({"text": "hello from a test", "enter": true})).await;
    call(&session, "ui.wait", json!({"target": "you: hello from a test", "timeout_ms": 10_000})).await;
    assert!(session.state().messages("general").iter().any(|m| m.text == "hello from a test"));

    // Open the room menu, copy the invite: the copy lands in the store, not the user's clipboard.
    call(&session, "ui.click", json!({"target": "Room menu"})).await;
    call(&session, "ui.click", json!({"target": "Copy invite link"})).await;
    call(&session, "ui.wait", json!({"target": "Copy invite link", "gone": true, "timeout_ms": 5_000})).await;
    let copied = session.ui().copied().expect("something copied").text;
    assert!(copied.starts_with("den://"), "{copied}");

    // Hide a panel by clicking its toggle: the button relabels, the panel goes.
    call(&session, "ui.click", json!({"target": "Hide people"})).await;
    call(&session, "ui.wait", json!({"target": "Show people", "timeout_ms": 5_000})).await;
    assert!(!tree(&session).await.contains("core.members"));

    // Screenshots: whole window and one panel, drawn on the CPU.
    let shot = home.join("shot.png");
    let r = call(&session, "ui.screenshot", json!({"path": shot})).await;
    assert_eq!((r["width"].as_u64(), r["height"].as_u64(), r["renderer"].as_str()), (Some(1100), Some(720), Some("cpu")), "{r}");
    assert!(std::fs::metadata(&shot).unwrap().len() > 10_000);
    let r = call(&session, "ui.screenshot", json!({"path": home.join("panel.png"), "panel": "core.channels"})).await;
    assert!(r["width"].as_u64().unwrap() < 1100, "{r}");

    // Resize, then quit like the close button.
    let w = call(&session, "ui.window", json!({"size": "800x600"})).await;
    assert_eq!(w["size"], json!([800.0, 600.0]), "{w}");
    call(&session, "app.quit", json!({})).await;
    tokio::task::spawn_blocking(move || app.join()).await.unwrap().unwrap().unwrap();
    assert!(session.ui().frontend_kind().is_none());
    session.close().await;
    let _ = std::fs::remove_dir_all(&home);
}
