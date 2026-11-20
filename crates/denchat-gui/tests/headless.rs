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

/// The skeuomorphic look: the `studio` theme in the `console` layout. Panels
/// keep their names under the new drawing (engraved labels, pressed-in rows),
/// and the CPU renderer tiles the brushed-metal canvas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn studio_theme_in_the_console_layout() {
    let home = std::env::temp_dir().join(format!("den-gui-studio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let mut config =
        AppConfig { nick: Some("alice".into()), bind: "localhost".into(), net: NetConfig::direct_only(), ..AppConfig::default() };
    config.ui.theme = "studio".into();
    config.ui.layout = "console".into();
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

    call(&session, "ui.wait", json!({"target": "button:Room menu", "timeout_ms": 15_000})).await;
    let t = tree(&session).await;
    for want in ["core.status", "heading \"Text channels\"", "button \"#general\" on", "textbox \"Message #general\""] {
        assert!(t.contains(want), "{want} missing from:\n{t}");
    }
    call(&session, "ui.type", json!({"text": "on the console", "enter": true})).await;
    call(&session, "ui.wait", json!({"target": "you: on the console", "timeout_ms": 10_000})).await;

    // The window's left edge is bare canvas (the gap beside the left card):
    // brushed metal streaks run along rows, so going down the column the
    // brightness zig-zags. A plain fill, or the soft light gradient alone,
    // would change smoothly. The rows are in the second 256 px tile, so this
    // also needs the texture to repeat (clamped, they'd all be its last row).
    let shot = home.join("studio.png");
    let r = call(&session, "ui.screenshot", json!({"path": shot})).await;
    assert_eq!(r["renderer"].as_str(), Some("cpu"), "{r}");
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&shot).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let channels = info.line_size / info.width as usize;
    let luma: Vec<i32> = (260..500)
        .map(|y| {
            let i = y * info.line_size + 4 * channels;
            buf[i] as i32 + buf[i + 1] as i32 + buf[i + 2] as i32
        })
        .collect();
    let turns = luma.windows(3).filter(|w| (w[1] - w[0]) * (w[2] - w[1]) < 0).count();
    assert!(turns > 40, "canvas looks untextured ({turns} turns): {:?}", &luma[..20]);

    call(&session, "app.quit", json!({})).await;
    tokio::task::spawn_blocking(move || app.join()).await.unwrap().unwrap().unwrap();
    session.close().await;
    let _ = std::fs::remove_dir_all(&home);
}

/// The lobby holds the View menu on its screen: View → Panels must not hide
/// it, or the window goes blank with nothing left to bring it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lobby_cannot_be_hidden_from_its_own_menu() {
    let home = std::env::temp_dir().join(format!("den-gui-lobby-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let config = AppConfig { nick: Some("alice".into()), bind: "localhost".into(), net: NetConfig::direct_only(), ..AppConfig::default() };
    let session = Session::start(SessionOptions { paths: Paths::under(&home), config, client_name: "test".into() }).await.unwrap();
    let rt = tokio::runtime::Handle::current();
    let app = {
        let session = session.clone();
        std::thread::spawn(move || {
            let _guard = rt.enter();
            denchat_gui::run_mode(session, Registry::with_builtins(), Mode::Headless)
        })
    };

    call(&session, "ui.wait", json!({"target": "button:View", "timeout_ms": 15_000})).await;
    call(&session, "ui.click", json!({"target": "button:View"})).await;
    call(&session, "ui.hover", json!({"target": "Panels"})).await;
    call(&session, "ui.wait", json!({"target": "checkbox:Lobby", "timeout_ms": 5_000})).await;
    // Clicking it (disabled or not) must leave the lobby, and its View button, on screen.
    let _ = try_click(&session, "checkbox:Lobby").await;
    call(&session, "ui.key", json!({"key": "Escape"})).await;
    let t = tree(&session).await;
    assert!(t.contains("core.lobby") && t.contains("button \"View\""), "lobby hidden:\n{t}");

    call(&session, "app.quit", json!({})).await;
    tokio::task::spawn_blocking(move || app.join()).await.unwrap().unwrap().unwrap();
    session.close().await;
    let _ = std::fs::remove_dir_all(&home);
}

/// A click that may be refused (a disabled widget): the error is fine.
async fn try_click(s: &Session, target: &str) -> Result<Value, String> {
    let rx = s.ui().call_frontend("ui.click", json!({"target": target}))?;
    tokio::time::timeout(Duration::from_secs(20), rx).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?
}

/// A 1x1 PNG.
const DOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63,
    0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The room picture from the room menu and your own from Settings → Profile.
/// Headless there's no file dialog: the app asks for a path instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pictures_from_the_room_menu_and_settings() {
    let home = std::env::temp_dir().join(format!("den-gui-pictures-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let image = home.join("dot.png");
    std::fs::write(&image, DOT).unwrap();
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
    let path = image.to_string_lossy().to_string();

    call(&session, "ui.wait", json!({"target": "button:Room menu", "timeout_ms": 15_000})).await;
    call(&session, "ui.click", json!({"target": "Room menu"})).await;
    call(&session, "ui.click", json!({"target": "Change room picture…"})).await;
    call(&session, "ui.wait", json!({"target": "button:Use picture", "timeout_ms": 5_000})).await;
    call(&session, "ui.type", json!({"text": path, "target": "textbox:Image file", "enter": true})).await;
    until("the room picture", || session.state().room_picture().is_some()).await;
    call(&session, "ui.wait", json!({"target": "button:Use picture", "gone": true, "timeout_ms": 5_000})).await;
    until("the change line", || session.state().messages("general").iter().any(|m| m.text == "alice changed the room picture")).await;

    call(&session, "ui.click", json!({"target": "Room menu"})).await;
    call(&session, "ui.click", json!({"target": "Settings"})).await;
    call(&session, "ui.wait", json!({"target": "tab:Profile", "timeout_ms": 5_000})).await;
    call(&session, "ui.click", json!({"target": "tab:Profile"})).await;
    call(&session, "ui.wait", json!({"target": "Change picture…", "timeout_ms": 5_000})).await;
    assert!(!tree(&session).await.contains("Remove picture"), "nothing to remove yet");
    call(&session, "ui.click", json!({"target": "Change picture…"})).await;
    call(&session, "ui.type", json!({"text": path, "target": "textbox:Image file", "enter": true})).await;
    until("your picture", || session.state().picture.is_some()).await;
    until("it reaches the room", || session.state().me().is_some_and(|m| m.picture == session.state().picture)).await;
    call(&session, "ui.click", json!({"target": "Remove picture"})).await;
    until("removed", || session.state().picture.is_none() && session.state().me().is_some_and(|m| m.picture.is_none())).await;

    call(&session, "app.quit", json!({})).await;
    tokio::task::spawn_blocking(move || app.join()).await.unwrap().unwrap().unwrap();
    session.close().await;
    let _ = std::fs::remove_dir_all(&home);
}
