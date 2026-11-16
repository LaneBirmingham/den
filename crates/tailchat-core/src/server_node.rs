//! A headless room: server + mods + control API, no UI.
//!
//! `tailchat serve` wraps this with a stdin admin console. Server-side mods
//! are bots: their hooks see every room event and `output = "send"` posts as
//! the server.
//!
//! The node is the room's always-on host. It runs a room save like any
//! member would, and when it starts while someone else is hosting the room
//! (because the server was down), it claims the room back: everyone is sent
//! over and their copies fill in what the server missed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tailchat_proto::NoticeLevel;
use tokio::sync::broadcast;

use crate::api::{self, ApiFuture, ApiHandler, ApiServer, MethodDoc, doc, param_str, ready, require_str};
use crate::config::ServerConfig;
use crate::events::{Event, names};
use crate::mods::{ModHost, ModRuntime};
use crate::paths::Paths;
use crate::rooms::{self, RoomSave};
use crate::server::{self, ServerEvent, ServerHandle, ServerMode, ServerOptions};
use crate::tailscale::Tailscale;
use crate::ui::UiStore;

pub struct NodeOptions {
    pub api: bool,
    pub mods: bool,
    pub tailscale_bin: Option<PathBuf>,
}

struct NodeInner {
    server: ServerHandle,
    save: RoomSave,
    /// Where the room was being hosted when we took it back.
    claimed_from: Option<String>,
    paths: Paths,
    events: broadcast::Sender<Event>,
    ui: Arc<UiStore>,
    mods: RwLock<Arc<ModRuntime>>,
    mods_enabled: bool,
    api: Mutex<Option<ApiServer>>,
    api_env: RwLock<Vec<(String, String)>>,
}

#[derive(Clone)]
pub struct ServerNode {
    inner: Arc<NodeInner>,
}

impl ServerNode {
    /// Host `save` with the settings in `cfg`. If someone is hosting the
    /// room right now, take it over from them.
    pub async fn start(cfg: ServerConfig, mut save: RoomSave, paths: Paths, opts: NodeOptions) -> anyhow::Result<ServerNode> {
        let tailscale = Tailscale::locate(opts.tailscale_bin.as_deref());
        let bind = server::resolve_bind(&cfg.bind, cfg.port, tailscale.as_ref()).await?;
        let live = rooms::find_live(&save.id, &save.addrs, cfg.port, tailscale.as_ref()).await;
        let term = save.term.max(live.as_ref().map(|l| l.server.term).unwrap_or(0)) + 1;
        let port = cfg.port;
        save.apply_config(&cfg);
        let server = server::start(ServerOptions {
            config: cfg,
            mode: ServerMode::Headless,
            bind,
            history_dir: Some(rooms::history_dir(&paths, &save.id)),
            tailscale: tailscale.clone(),
            room_id: save.id.clone(),
            term,
        })
        .await?;
        let share = rooms::share_addrs(server.local_addr(), tailscale.as_ref()).await;
        save.term = term;
        save.addrs = share.clone();
        rooms::save(&paths, &save)?;
        let mut claimed_from = None;
        if let Some(live) = live {
            match rooms::claim(&live.addr, port, &save.id, term, share).await {
                Ok(()) => {
                    tracing::info!("took the room back from {}", live.addr);
                    claimed_from = Some(live.addr);
                }
                Err(e) => tracing::warn!("{} is hosting this room and didn't hand it over: {e:#}", live.addr),
            }
        }
        let (events, _) = broadcast::channel(1024);
        let inner = Arc::new(NodeInner {
            server: server.clone(),
            save,
            claimed_from,
            paths: paths.clone(),
            events: events.clone(),
            ui: Arc::new(UiStore::default()),
            mods: RwLock::new(Arc::new(ModRuntime::empty(&paths))),
            mods_enabled: opts.mods,
            api: Mutex::new(None),
            api_env: RwLock::new(Vec::new()),
        });

        let mut rx = server.subscribe();
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let e = match ev {
                    ServerEvent::MemberJoined { member } => Event::new(names::MEMBER_JOINED, json!({"member": member})),
                    ServerEvent::MemberLeft { member, reason } => {
                        Event::new(names::MEMBER_LEFT, json!({"member": member, "reason": reason}))
                    }
                    ServerEvent::MemberUpdated { member } => Event::new(names::MEMBER_UPDATED, json!({"member": member})),
                    ServerEvent::Message { message } => Event::new(names::MESSAGE_CREATED, json!({"message": message})),
                    ServerEvent::App { from, ns, channel, data } => {
                        Event::new(names::APP_MESSAGE, json!({"from": from, "ns": ns, "channel": channel, "data": data}))
                    }
                    ServerEvent::Moved { to, term } => Event::new(names::ROOM_MOVED, json!({"to": to, "term": term})),
                };
                let _ = events.send(e);
            }
        });

        let node = ServerNode { inner };
        if opts.api {
            let handler: Arc<dyn ApiHandler> = Arc::new(NodeApi(Arc::downgrade(&node.inner)));
            match ApiServer::start(handler, &paths).await {
                Ok(s) => {
                    *node.inner.api_env.write() = s.env();
                    *node.inner.api.lock() = Some(s);
                }
                Err(e) => tracing::warn!("control API disabled: {e:#}"),
            }
        }
        node.reload_mods();
        Ok(node)
    }

    pub fn server(&self) -> &ServerHandle {
        &self.inner.server
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner.server.local_addr()
    }

    /// The room save this node hosts (as of start-up).
    pub fn save(&self) -> &RoomSave {
        &self.inner.save
    }

    /// Where the room was hosted when this node took it over, if it did.
    pub fn claimed_from(&self) -> Option<&str> {
        self.inner.claimed_from.as_deref()
    }

    pub fn api_addr(&self) -> Option<SocketAddr> {
        self.inner.api.lock().as_ref().map(|a| a.addr)
    }

    pub fn mods(&self) -> Arc<ModRuntime> {
        self.inner.mods.read().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn reload_mods(&self) {
        let fresh = Arc::new(ModRuntime::load(&self.inner.paths, "server", self.inner.mods_enabled));
        let old = std::mem::replace(&mut *self.inner.mods.write(), fresh.clone());
        old.stop();
        let host: Arc<dyn ModHost> = Arc::new(NodeHost { inner: Arc::downgrade(&self.inner), ui: self.inner.ui.clone() });
        fresh.start(host, self.subscribe());
    }

    /// Run a mod command from the admin console; output posts as the server.
    pub async fn run_mod_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<()> {
        let host: Arc<dyn ModHost> = Arc::new(NodeHost { inner: Arc::downgrade(&self.inner), ui: self.inner.ui.clone() });
        let channel = self.inner.server.default_text_channel();
        self.mods().run_command(host, name, args, channel, json!({"node": "server"})).await
    }

    pub fn shutdown(&self) {
        self.inner.mods.read().stop();
        self.inner.server.shutdown();
        self.inner.api.lock().take();
    }
}

struct NodeHost {
    inner: Weak<NodeInner>,
    ui: Arc<UiStore>,
}

impl ModHost for NodeHost {
    fn node_kind(&self) -> &'static str {
        "server"
    }

    fn send_text(&self, channel: Option<String>, text: String) {
        if let Some(i) = self.inner.upgrade()
            && let Err(e) = i.server.say(channel.as_deref(), &text)
        {
            tracing::warn!("mod message rejected: {e:#}");
        }
    }

    fn notice(&self, level: NoticeLevel, text: String) {
        match level {
            NoticeLevel::Info => tracing::info!(target: "tailchat::mod", "{text}"),
            _ => tracing::warn!(target: "tailchat::mod", "{text}"),
        }
    }

    fn env(&self) -> Vec<(String, String)> {
        let Some(i) = self.inner.upgrade() else { return Vec::new() };
        let mut env = i.api_env.read().clone();
        env.push(("TAILCHAT_ROOM".into(), i.server.info().name));
        env
    }

    fn self_id(&self) -> Option<tailchat_proto::MemberId> {
        None
    }

    fn ui(&self) -> &UiStore {
        &self.ui
    }
}

struct NodeApi(Weak<NodeInner>);

const NODE_METHODS: &[MethodDoc] = &[
    doc("state.get", "{}", "Server info, members, channels"),
    doc("members.list", "{}", "Connected members"),
    doc("channels.list", "{}", "Channels"),
    doc("messages.list", "{channel?, limit?}", "Recent history"),
    doc("message.send", "{text, channel?}", "Post as the server"),
    doc("notice.send", "{text}", "Show a notice to everyone"),
    doc("member.kick", "{id, reason?}", "Disconnect a member"),
    doc("app.send", "{ns, data, channel?}", "Broadcast a mod-defined payload (from id 0)"),
    doc("motd.set", "{motd?}", "Change the message of the day for new joins"),
    doc("mods.list", "{}", "Server-side mods"),
    doc("mods.reload", "{}", "Rescan and restart server-side mods"),
];

impl ApiHandler for NodeApi {
    fn kind(&self) -> &'static str {
        "server"
    }

    fn label(&self) -> String {
        self.0.upgrade().map(|i| i.server.info().name).unwrap_or_default()
    }

    fn events(&self) -> broadcast::Receiver<Event> {
        match self.0.upgrade() {
            Some(i) => i.events.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    fn methods(&self) -> Vec<MethodDoc> {
        NODE_METHODS.to_vec()
    }

    fn call(&self, method: &str, p: Value) -> ApiFuture {
        let Some(inner) = self.0.upgrade() else {
            return ready(Err("server stopped".into()));
        };
        let node = ServerNode { inner };
        let srv = node.server().clone();
        let to_val = |v: serde_json::Result<Value>| v.map_err(|e| e.to_string());
        let res: api::ApiResult = match method {
            "state.get" => Ok(json!({
                "server": srv.info(),
                "room": node.save(),
                "listen": srv.local_addr(),
                "members": srv.members(),
                "channels": srv.channels(),
                "mods": node.mods().active().map(|m| m.manifest.id.clone()).collect::<Vec<_>>(),
            })),
            "members.list" => to_val(serde_json::to_value(srv.members())),
            "channels.list" => to_val(serde_json::to_value(srv.channels())),
            "messages.list" => {
                let ch = param_str(&p, "channel").or_else(|| srv.default_text_channel()).unwrap_or_default();
                let limit = p["limit"].as_u64().unwrap_or(50) as usize;
                to_val(serde_json::to_value(srv.history(&ch, limit)))
            }
            "message.send" => require_str(&p, "text").and_then(|t| {
                srv.say(param_str(&p, "channel").as_deref(), &t).map(|m| json!({"message": m})).map_err(|e| format!("{e:#}"))
            }),
            "notice.send" => require_str(&p, "text").map(|t| {
                srv.notice_all(&t);
                json!({"sent": true})
            }),
            "member.kick" => match p["id"].as_u64() {
                Some(id) => {
                    Ok(json!({"kicked": srv.kick(id as u32, &param_str(&p, "reason").unwrap_or_else(|| "kicked by admin".into()))}))
                }
                None => Err("missing numeric param `id`".into()),
            },
            "app.send" => require_str(&p, "ns").map(|ns| {
                srv.send_app(&ns, param_str(&p, "channel"), p.get("data").cloned().unwrap_or(Value::Null));
                json!({"sent": true})
            }),
            "motd.set" => {
                srv.set_motd(param_str(&p, "motd"));
                Ok(json!({"motd": srv.info().motd}))
            }
            "mods.list" => {
                let m = node.mods();
                Ok(json!({"mods": m.mods(), "problems": m.problems()}))
            }
            "mods.reload" => {
                node.reload_mods();
                Ok(json!({"mods": node.mods().active().count()}))
            }
            other => Err(format!("unknown method {other} (see api.methods)")),
        };
        ready(res)
    }
}
