//! A headless room: server + mods + control API, no UI.
//!
//! `den serve` wraps this with a stdin admin console. Server-side mods
//! are bots: their hooks see every room event and `output = "send"` posts as
//! the server.
//!
//! The node is the room's always-on host. It runs a room save like any
//! member would, and when it starts while someone else is hosting the room
//! (because the server was down), it claims the room back: everyone is sent
//! over and their copies fill in what the server missed.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context;

use denchat_proto::NoticeLevel;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::api::{self, ApiFuture, ApiHandler, ApiServer, MethodDoc, doc, param_str, ready, require_str};
use crate::config::ServerConfig;
use crate::events::{Event, names};
use crate::mods::{ModHost, ModRuntime};
use crate::net::{self, Net};
use crate::paths::Paths;
use crate::relay::{self, LocalRelay};
use crate::rooms::{self, RoomSave};
use crate::server::{self, HandoffState, ServerEvent, ServerHandle, ServerMode, ServerOptions};
use crate::ui::UiStore;

pub struct NodeOptions {
    pub api: bool,
    pub mods: bool,
}

struct NodeInner {
    /// Our endpoint key; released once the endpoint is closed.
    key_lock: Mutex<Option<net::KeyLock>>,
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
    /// The relay we run for the room's members, if any.
    relay: Mutex<Option<LocalRelay>>,
}

#[derive(Clone)]
pub struct ServerNode {
    inner: Arc<NodeInner>,
}

impl ServerNode {
    /// Host `save` with the settings in `cfg`. If someone is hosting the
    /// room right now, take it over from them.
    pub async fn start(cfg: ServerConfig, mut save: RoomSave, paths: Paths, opts: NodeOptions) -> anyhow::Result<ServerNode> {
        // Its own endpoint key, so an app on the same machine (same data
        // dir) is a different peer.
        let key_lock = net::lock_key(&paths.data.join("serve.key"))?;
        let key = net::load_or_create_key(&paths.data.join("serve.key"))?;
        let relay = match cfg.relay.enabled {
            true => {
                let port = if cfg.relay.port == 0 { cfg.port } else { cfg.relay.port };
                let token = relay::load_token(&paths.data)?;
                Some(LocalRelay::start(net::bind_ip(&cfg.bind)?, port, token).await.with_context(|| {
                    format!("is something else using TCP port {port}? (set [relay] port, or enabled = false, in server.toml)")
                })?)
            }
            false => None,
        };
        let mut net_cfg = cfg.net.clone();
        if let Some(r) = &relay {
            net_cfg.extra_relays.push(r.local_url.to_string());
        }
        let net = Net::bind(key, &cfg.bind, cfg.port, true, &net_cfg)
            .await
            .with_context(|| format!("is another `den serve` already using UDP port {}?", cfg.port))?;
        let relay = match relay {
            Some(r) => use_own_relay(&net, r, &cfg).await?,
            None => None,
        };
        let live = rooms::find_live(&net, &save.id, &save.places()).await;
        let term = save.term.max(live.as_ref().map(|l| l.server.term).unwrap_or(0)) + 1;
        save.apply_config(&cfg);
        let server = server::start(ServerOptions {
            config: cfg.clone(),
            mode: ServerMode::Headless,
            net: net.clone(),
            history_dir: Some(rooms::history_dir(&paths, &save.id)),
            room_id: save.id.clone(),
            term,
            invite: save.invite.clone(),
            profiles: save.profiles.clone(),
        })
        .await?;
        let addr = server.addrs().await;
        save.term = term;
        save.addrs = addr.clone();
        rooms::save(&paths, &save)?;
        let mut claimed_from = None;
        if let Some(live) = live {
            let member = rooms::member_key(&paths, &save.id)?;
            match rooms::claim(&net, &live.addr, &save, &member, term, addr).await {
                Ok(()) => {
                    tracing::info!("took over the room from {}", live.addr);
                    claimed_from = Some(live.addr);
                }
                Err(e) => tracing::warn!("{} is hosting this room and didn't hand it over: {e:#}", live.addr),
            }
        }
        let (events, _) = broadcast::channel(1024);
        let inner = Arc::new(NodeInner {
            key_lock: Mutex::new(Some(key_lock)),
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
            relay: Mutex::new(relay),
        });

        let mut rx = server.subscribe();
        let (save_paths, room_id) = (paths.clone(), inner.save.id.clone());
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if let ServerEvent::MemberJoined { member } | ServerEvent::MemberUpdated { member } = &ev {
                    // Keep the room's profiles and members' addresses, like an app does.
                    if let Ok(mut s) = rooms::load(&save_paths, &room_id) {
                        let before = (s.profiles.clone(), s.peers.clone());
                        s.note_member(member);
                        if (s.profiles.clone(), s.peers.clone()) != before
                            && let Err(e) = rooms::save(&save_paths, &s)
                        {
                            tracing::warn!("saving room: {e:#}");
                        }
                    }
                }
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
                    ServerEvent::Moved { to, term } => {
                        // Remember where it went, so the next start finds it there and claims it back.
                        if let Ok(mut s) = rooms::load(&save_paths, &room_id) {
                            s.term = s.term.max(term);
                            s.addrs = to.clone();
                            s.updated = crate::now_ms();
                            if let Err(e) = rooms::save(&save_paths, &s) {
                                tracing::warn!("saving room: {e:#}");
                            }
                        }
                        Event::new(names::ROOM_MOVED, json!({"to": to, "term": term}))
                    }
                    ServerEvent::ChannelsUpdated { channels } => {
                        // The room's save is what the next start (or host) serves.
                        if let Ok(mut s) = rooms::load(&save_paths, &room_id) {
                            s.channels = channels.clone();
                            s.updated = crate::now_ms();
                            if let Err(e) = rooms::save(&save_paths, &s) {
                                tracing::warn!("saving room: {e:#}");
                            }
                        }
                        Event::new(names::CHANNELS_UPDATED, json!({"channels": channels}))
                    }
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

    /// The invite link for this room (where it's hosted now).
    pub async fn invite_link(&self) -> net::Link {
        self.inner.server.link().await
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

    /// Hand the room to a member whose app can host (waiting a few seconds at
    /// most), then stop. Members aren't stranded, and the next start claims
    /// the room back. Stops straight away if there's nobody to hand it to.
    pub async fn close(&self) {
        let srv = &self.inner.server;
        match srv.hand_off(None, None) {
            Ok(m) => {
                tracing::info!("shutting down: handing the room to {}", m.nick);
                match srv.wait_handoff(Duration::from_secs(6)).await {
                    HandoffState::Moved { to, .. } => tracing::info!("handed the room to {}", to.first().map_or("?", |a| a.as_str())),
                    other => tracing::warn!("handoff didn't finish ({other:?}); stopping anyway"),
                }
                // Let the handoff frames reach everyone.
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(e) => tracing::info!("shutting down: {e:#}"),
        }
        self.stop();
        self.inner.server.net().close().await;
        self.inner.key_lock.lock().take();
        let relay = self.inner.relay.lock().take();
        if let Some(r) = relay {
            r.shutdown().await;
        }
    }

    /// The relay URL members are told about (this server's own relay), if
    /// any, without its token (that only goes out in invite links).
    pub fn relay_url(&self) -> Option<String> {
        self.inner.relay.lock().as_ref()?;
        self.inner.server.net().advertised_relays()?.first().map(relay::without_token)
    }

    /// Stop now, without handing the room on. The endpoint closes in the
    /// background (its key is this node's: a restart must not find it open).
    pub fn shutdown(&self) {
        self.stop();
        let net = self.inner.server.net().clone();
        let relay = self.inner.relay.lock().take();
        let key_lock = self.inner.key_lock.lock().take();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                net.close().await;
                drop(key_lock);
                if let Some(r) = relay {
                    r.shutdown().await;
                }
            });
        }
    }

    fn stop(&self) {
        self.inner.mods.read().stop();
        self.inner.server.shutdown();
        self.inner.api.lock().take();
    }
}

/// Make the server's own relay the way members reach it: work out the URL
/// they should use (config, else the public IP), tell peers that (with the
/// relay's token, so invite links carry it), and drop
/// the other relays (they were only there to learn the public IP). Without a
/// usable URL, the relay is stopped and the configured relays stay.
async fn use_own_relay(net: &Net, relay: LocalRelay, cfg: &ServerConfig) -> anyhow::Result<Option<LocalRelay>> {
    let url = match cfg.relay.url.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(u) => Some(relay::parse_url(u)?),
        None => relay::discover_public_url(net, relay.addr.port(), Duration::from_secs(5)).await,
    };
    let Some(url) = url else {
        if !net.direct_enabled() && cfg.net.effective_relays().is_empty() {
            relay.shutdown().await;
            anyhow::bail!(
                "direct paths are off ([net] direct = false), and without a public address for the room's relay \
                 nothing could reach this server: set [relay] url in server.toml (or pass --relay-url), or add [net] relays"
            );
        }
        tracing::warn!(
            "couldn't work out this machine's public address, so the room's relay is off; set [relay] url in server.toml \
             (e.g. \"http://box.example.com:{}\") to turn it on",
            relay.addr.port()
        );
        net.endpoint().remove_relay(&relay.local_url).await;
        relay.shutdown().await;
        return Ok(None);
    };
    for other in net::relay_map(&cfg.net)?.urls::<Vec<_>>() {
        if other != relay.local_url {
            net.endpoint().remove_relay(&other).await;
        }
    }
    net.advertise_relays(vec![relay.url(&url)]);
    tracing::info!("members reach this room's relay at {url} (TCP {} must be reachable)", relay.addr.port());
    Ok(Some(relay))
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
            NoticeLevel::Info => tracing::info!(target: "den::mod", "{text}"),
            _ => tracing::warn!(target: "den::mod", "{text}"),
        }
    }

    fn env(&self) -> Vec<(String, String)> {
        let Some(i) = self.inner.upgrade() else { return Vec::new() };
        let mut env = i.api_env.read().clone();
        env.push(("DEN_ROOM".into(), i.server.info().name));
        env
    }

    fn self_id(&self) -> Option<denchat_proto::MemberId> {
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
                "endpoint": srv.net().id().to_z32(),
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
