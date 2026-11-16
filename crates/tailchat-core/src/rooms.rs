//! Saved rooms: every member's copy of a room.
//!
//! A room works like a shared save game. Whoever hosts runs it from their
//! copy; everyone else keeps theirs up to date while they're in it, so any of
//! them can host it next time. On disk:
//!
//! ```text
//! <data>/rooms/<room id>/room.toml            name, channels, join rules, term, last host
//! <data>/rooms/<room id>/history/<chan>.jsonl recent messages, one JSON object per line
//! ```
//!
//! The folder is self-contained: copy it to another machine (for example one
//! running `tailchat serve --room <id>`) and that machine can host the room.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tailchat_proto::{Access, Channel, ChannelId, ChannelKind, ChatMessage, ClientMsg, ServerInfo, ServerMsg, clean_channel_id};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::config::{self, ChannelSpec, ServerConfig};
use crate::net::{read_frame, resolve, write_frame};
use crate::paths::Paths;
use crate::tailscale::Tailscale;

/// Save format version. Bump on incompatible changes.
pub const FORMAT: u32 = 1;
/// Messages a member keeps per channel.
pub const KEEP: usize = 500;

/// `room.toml`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RoomSave {
    pub format: u32,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motd: Option<String>,
    /// Highest term seen. The next host uses a higher one.
    #[serde(default)]
    pub term: u64,
    /// Where the room was last hosted: what to try first when coming back.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// Unix ms of the last change.
    #[serde(default)]
    pub updated: i64,
    #[serde(default)]
    pub access: Access,
    #[serde(default)]
    pub channels: Vec<Channel>,
}

impl RoomSave {
    /// A new room (fresh id, term 0) with the settings in `cfg`.
    pub fn new(cfg: &ServerConfig) -> RoomSave {
        let mut save = RoomSave::empty(&new_id());
        save.apply_config(cfg);
        save
    }

    /// A save we know only the id of yet.
    pub fn empty(id: &str) -> RoomSave {
        RoomSave {
            format: FORMAT,
            id: id.to_owned(),
            name: "tailchat".into(),
            motd: None,
            term: 0,
            addrs: Vec::new(),
            updated: crate::now_ms(),
            access: Access::default(),
            channels: Vec::new(),
        }
    }

    /// Take name, motd, channels and join rules from server settings.
    pub fn apply_config(&mut self, cfg: &ServerConfig) {
        self.name = cfg.name.clone();
        self.motd = cfg.motd.clone();
        self.channels = cfg.channel_list();
        self.access = Access { require_tailnet: cfg.require_tailnet, allow: cfg.allow.clone() };
        self.updated = crate::now_ms();
    }

    /// Settings for hosting this room: limits from `base` (server.toml), the
    /// room's own name, channels and join rules.
    pub fn server_config(&self, base: ServerConfig) -> ServerConfig {
        let channels = if self.channels.is_empty() { base.channels.clone() } else { self.channels.iter().map(ChannelSpec::from).collect() };
        ServerConfig {
            name: self.name.clone(),
            motd: self.motd.clone(),
            channels,
            require_tailnet: self.access.require_tailnet,
            allow: self.access.allow.clone(),
            ..base
        }
    }

    /// Copy what a host told us about the room.
    pub fn update_from(&mut self, server: &ServerInfo, channels: &[Channel], access: Option<&Access>) {
        self.name = server.name.clone();
        self.motd = server.motd.clone();
        self.term = self.term.max(server.term);
        let safe: Vec<Channel> = channels.iter().filter(|c| safe_channel(&c.id)).cloned().collect();
        if !safe.is_empty() {
            self.channels = safe;
        }
        if let Some(a) = access {
            self.access = a.clone();
        }
        self.updated = crate::now_ms();
    }

    pub fn text_channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.iter().filter(|c| c.kind == ChannelKind::Text)
    }
}

pub fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// Room ids come from the network and name a folder: keep them boring.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn safe_channel(id: &str) -> bool {
    !id.is_empty() && clean_channel_id(id) == id
}

pub fn dir(paths: &Paths, id: &str) -> PathBuf {
    paths.rooms_dir().join(id)
}

pub fn history_dir(paths: &Paths, id: &str) -> PathBuf {
    dir(paths, id).join("history")
}

pub fn load(paths: &Paths, id: &str) -> anyhow::Result<RoomSave> {
    if !valid_id(id) {
        bail!("bad room id {id:?}");
    }
    let path = dir(paths, id).join("room.toml");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let save: RoomSave = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if save.format > FORMAT {
        bail!("{} was written by a newer tailchat (format {})", path.display(), save.format);
    }
    Ok(save)
}

pub fn save(paths: &Paths, save: &RoomSave) -> anyhow::Result<()> {
    if !valid_id(&save.id) {
        bail!("bad room id {:?}", save.id);
    }
    config::save(&dir(paths, &save.id).join("room.toml"), save)
}

/// Every saved room, most recently used first.
pub fn list(paths: &Paths) -> Vec<RoomSave> {
    let Ok(entries) = std::fs::read_dir(paths.rooms_dir()) else { return Vec::new() };
    let mut out: Vec<RoomSave> = entries.filter_map(|e| e.ok()).filter_map(|e| load(paths, e.file_name().to_str()?).ok()).collect();
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.name.cmp(&b.name)));
    out
}

/// A saved room by id, or by name (case-insensitive, most recent wins).
pub fn find(paths: &Paths, key: &str) -> Option<RoomSave> {
    let key = key.trim();
    if valid_id(key)
        && let Ok(s) = load(paths, key)
    {
        return Some(s);
    }
    list(paths).into_iter().find(|s| s.name.eq_ignore_ascii_case(key))
}

/// Delete a saved room and its history.
pub fn forget(paths: &Paths, id: &str) -> anyhow::Result<()> {
    if !valid_id(id) {
        bail!("bad room id {id:?}");
    }
    let d = dir(paths, id);
    std::fs::remove_dir_all(&d).with_context(|| format!("removing {}", d.display()))
}

/// Copy `<chan>.jsonl` files from an old-style history folder into a room.
pub fn import_history(from: &Path, room_history: &Path) -> anyhow::Result<usize> {
    let Ok(entries) = std::fs::read_dir(from) else { return Ok(0) };
    std::fs::create_dir_all(room_history)?;
    let mut n = 0;
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") && !room_history.join(e.file_name()).exists() {
            std::fs::copy(&p, room_history.join(e.file_name()))?;
            n += 1;
        }
    }
    Ok(n)
}

// ----- history files -------------------------------------------------------

pub fn history_file(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{channel}.jsonl"))
}

/// Recent history per text channel, sorted by id with duplicates removed,
/// plus the highest id seen. Files that grew well past `limit` are compacted.
///
/// Member ids are per host, so `author_id` is cleared on the way in.
pub fn load_history(dir: &Path, channels: &[Channel], limit: usize) -> anyhow::Result<(HashMap<ChannelId, VecDeque<ChatMessage>>, u64)> {
    let mut out = HashMap::new();
    let mut max_id = 0;
    for ch in channels.iter().filter(|c| c.kind == ChannelKind::Text && safe_channel(&c.id)) {
        let path = history_file(dir, &ch.id);
        let Ok(file) = std::fs::File::open(&path) else { continue };
        let mut lines = 0usize;
        let mut all: Vec<ChatMessage> = std::io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .inspect(|_| lines += 1)
            .filter_map(|l| serde_json::from_str::<ChatMessage>(&l).ok())
            .filter(|m| m.id != 0 && m.channel == ch.id)
            .map(|mut m| {
                m.author_id = None;
                m
            })
            .collect();
        all.sort_by_key(|m| m.id);
        all.dedup_by_key(|m| m.id);
        max_id = all.last().map(|m| m.id).unwrap_or(0).max(max_id);
        let keep = all.split_off(all.len().saturating_sub(limit.max(1)));
        if lines > keep.len() * 4 + 100 {
            let tmp = path.with_extension("jsonl.tmp");
            let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            for m in &keep {
                serde_json::to_writer(&mut f, m)?;
                f.write_all(b"\n")?;
            }
            f.flush()?;
            drop(f);
            std::fs::rename(&tmp, &path)?;
        }
        out.insert(ch.id.clone(), keep.into());
    }
    Ok((out, max_id))
}

/// Append messages to `<dir>/<channel>.jsonl` in the background.
pub fn spawn_persister(dir: PathBuf) -> mpsc::UnboundedSender<ChatMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChatMessage>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if !safe_channel(&msg.channel) {
                continue;
            }
            let path = history_file(&dir, &msg.channel);
            let line = match serde_json::to_string(&msg) {
                Ok(l) => l + "\n",
                Err(_) => continue,
            };
            let res = async {
                tokio::fs::create_dir_all(&dir).await?;
                let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
                f.write_all(line.as_bytes()).await?;
                f.flush().await
            }
            .await;
            if let Err(e) = res {
                tracing::warn!("persisting history to {}: {e}", path.display());
            }
        }
    });
    tx
}

/// A member's live copy of the room it's in: recent messages in memory, new
/// ones appended to disk.
pub struct Copy {
    pub id: String,
    messages: HashMap<ChannelId, BTreeMap<u64, ChatMessage>>,
    disk: mpsc::UnboundedSender<ChatMessage>,
}

impl Copy {
    /// Open (or start) the copy of `save`. Needs a Tokio runtime.
    pub fn open(paths: &Paths, save: &RoomSave) -> anyhow::Result<Copy> {
        let dir = history_dir(paths, &save.id);
        let (loaded, _) = load_history(&dir, &save.channels, KEEP)?;
        let messages = loaded.into_iter().map(|(ch, list)| (ch, list.into_iter().map(|m| (m.id, m)).collect())).collect();
        Ok(Copy { id: save.id.clone(), messages, disk: spawn_persister(dir) })
    }

    /// Ids we hold for `channel`, oldest first.
    pub fn ids(&self, channel: &str) -> Vec<u64> {
        self.messages.get(channel).map(|m| m.keys().copied().collect()).unwrap_or_default()
    }

    pub fn get(&self, channel: &str, ids: &[u64]) -> Vec<ChatMessage> {
        let Some(m) = self.messages.get(channel) else { return Vec::new() };
        ids.iter().filter_map(|id| m.get(id).cloned()).collect()
    }

    /// Remember a message; append it to disk too unless `write` is false
    /// (when our own server is already writing the same files).
    pub fn add(&mut self, m: &ChatMessage, write: bool) {
        if m.id == 0 || !safe_channel(&m.channel) {
            return;
        }
        let list = self.messages.entry(m.channel.clone()).or_default();
        if list.insert(m.id, m.clone()).is_some() {
            return;
        }
        while list.len() > KEEP {
            list.pop_first();
        }
        if write {
            let _ = self.disk.send(m.clone());
        }
    }
}

// ----- finding and taking over a live room ----------------------------------

/// A host currently running a room.
#[derive(Clone, Debug)]
pub struct LiveHost {
    /// What to connect to.
    pub addr: String,
    pub server: ServerInfo,
    pub members: u32,
}

/// Look for a running host of room `id`: probe `addrs` (where it was last
/// seen) and, with Tailscale, every online peer on `port`. Highest term wins.
pub async fn find_live(id: &str, addrs: &[String], port: u16, tailscale: Option<&Tailscale>) -> Option<LiveHost> {
    let mut set = tokio::task::JoinSet::new();
    for a in addrs.iter().cloned() {
        set.spawn(async move {
            let sock = resolve(&a, port).await.ok()?;
            match crate::net::probe(sock, Duration::from_millis(900)).await {
                Ok(ServerMsg::ProbeInfo { server, members, .. }) => Some(LiveHost { addr: a, server, members }),
                _ => None,
            }
        });
    }
    let mut found: Vec<LiveHost> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(h)) = r {
            found.push(h);
        }
    }
    if let Some(ts) = tailscale
        && let Ok(status) = ts.status().await
    {
        for r in crate::session::discover_rooms(&status, port).await {
            found.push(LiveHost { addr: r.addr, server: r.server, members: r.members });
        }
    }
    found.into_iter().filter(|h| h.server.room_id == id).max_by_key(|h| h.server.term)
}

/// Ask the host at `addr` to move everyone to us (we host the room at a
/// higher `term` on `ours`).
pub async fn claim(addr: &str, port: u16, room: &str, term: u64, ours: Vec<String>) -> anyhow::Result<()> {
    let sock = resolve(addr, port).await?;
    let talk = async {
        let mut s = TcpStream::connect(sock).await?;
        write_frame(&mut s, &ClientMsg::Claim { room: room.to_owned(), term, addrs: ours }).await?;
        read_frame::<_, ServerMsg>(&mut s).await
    };
    match tokio::time::timeout(Duration::from_secs(8), talk).await.context("timed out")?? {
        Some(ServerMsg::Handoff { .. }) => Ok(()),
        Some(ServerMsg::Error { message, .. }) => bail!("{message}"),
        other => bail!("unexpected reply {other:?}"),
    }
}

/// A save for the room hosted at `addr`, made from its probe answer (merged
/// into an existing save of the same room). Join rules aren't public, so they
/// come from the existing save or stay open.
pub async fn adopt(paths: &Paths, addr: &str, port: u16) -> anyhow::Result<RoomSave> {
    let sock = resolve(addr, port).await?;
    let ServerMsg::ProbeInfo { server, channels, .. } = crate::net::probe(sock, Duration::from_secs(3)).await? else {
        bail!("{addr} did not answer like a tailchat room");
    };
    if !valid_id(&server.room_id) {
        bail!("{addr} runs a tailchat without room ids; upgrade it first");
    }
    let mut save = load(paths, &server.room_id).unwrap_or_else(|_| RoomSave::empty(&server.room_id));
    save.update_from(&server, &channels, None);
    save.addrs = vec![addr.to_owned()];
    Ok(save)
}

/// Addresses friends can use to reach a room listening on `listen`, best first.
pub async fn share_addrs(listen: SocketAddr, tailscale: Option<&Tailscale>) -> Vec<String> {
    let mut share = Vec::new();
    if let Some(ts) = tailscale
        && let Ok(st) = ts.status().await
        && let Some(me) = &st.me
    {
        let suffix = |h: String| if listen.port() == tailchat_proto::DEFAULT_PORT { h } else { format!("{h}:{}", listen.port()) };
        share.push(suffix(me.short_name()));
        if let Some(d) = &me.dns_name {
            share.push(suffix(d.clone()));
        }
        if let Some(ip) = me.ipv4() {
            share.push(format!("{ip}:{}", listen.port()));
        }
    }
    if share.is_empty() || !listen.ip().is_unspecified() && !crate::tailscale::is_tailnet_ip(listen.ip()) {
        share.insert(0, listen.to_string());
    }
    share
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64, text: &str) -> ChatMessage {
        ChatMessage {
            id,
            channel: "general".into(),
            ts: id as i64,
            author_id: Some(3),
            author: "a".into(),
            author_login: None,
            text: text.into(),
            kind: Default::default(),
        }
    }

    #[test]
    fn save_roundtrip_and_find() {
        let home = std::env::temp_dir().join(format!("tailchat-rooms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let paths = Paths::under(&home);
        let mut s = RoomSave::new(&ServerConfig { name: "The Den".into(), allow: vec!["a@example.com".into()], ..ServerConfig::default() });
        s.term = 4;
        s.addrs = vec!["box:8245".into()];
        save(&paths, &s).unwrap();
        assert_eq!(load(&paths, &s.id).unwrap(), s);
        assert_eq!(find(&paths, "the den").unwrap().id, s.id);
        assert_eq!(find(&paths, &s.id).unwrap().name, "The Den");
        let cfg = s.server_config(ServerConfig { history_limit: 7, ..ServerConfig::default() });
        assert_eq!((cfg.name.as_str(), cfg.history_limit, cfg.allow.len()), ("The Den", 7, 1));
        forget(&paths, &s.id).unwrap();
        assert!(list(&paths).is_empty());
        assert!(load(&paths, "../etc").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn history_loads_sorted_and_deduped() {
        let dir = std::env::temp_dir().join(format!("tailchat-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lines: Vec<String> =
            [msg(5, "e"), msg(2, "b"), msg(5, "e"), msg(9, "i")].iter().map(|m| serde_json::to_string(m).unwrap()).collect();
        std::fs::write(history_file(&dir, "general"), lines.join("\n") + "\n").unwrap();
        let channels = vec![Channel { id: "general".into(), name: "general".into(), kind: ChannelKind::Text, topic: None }];
        let (h, max) = load_history(&dir, &channels, 2).unwrap();
        let ids: Vec<u64> = h["general"].iter().map(|m| m.id).collect();
        assert_eq!((ids, max), (vec![5, 9], 9));
        assert!(h["general"].iter().all(|m| m.author_id.is_none()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
