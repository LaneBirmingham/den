//! Saved rooms: every member's copy of a room.
//!
//! A room works like a shared save game. Whoever hosts runs it from their
//! copy; everyone else keeps theirs up to date while they're in it, so any of
//! them can host it next time. On disk:
//!
//! ```text
//! <data>/rooms/<room id>/room.toml            name, channels, join rules, invite, term,
//!                                             last host, peers, profiles
//! <data>/rooms/<room id>/member.key           this member's key for the room (secret)
//! <data>/rooms/<room id>/history/<chan>.jsonl recent messages, one JSON object per line
//! ```
//!
//! The folder is self-contained: copy it to another machine (for example one
//! running `tailchat serve --room <id>`) and that machine can host the room.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tailchat_proto::{Access, Channel, ChannelId, ChannelKind, ChatMessage, ClientMsg, Member, Profile, ServerInfo, ServerMsg, clean_channel_id};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::config::{self, ChannelSpec, ServerConfig};
use crate::net::{self, Link, Net, SecretKey};
use crate::paths::Paths;

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
    /// The room's invite key: only people holding it can join. Members keep
    /// it so any of them can host and invite. `None`: an open room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// Where members' apps were last seen (newest first): where to look for
    /// the room when its last host is gone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<String>,
    /// Members the room knows: the public half of each one's key and their nick.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
}

/// How many peer addresses a save keeps.
const MAX_PEERS: usize = 16;

impl RoomSave {
    /// A new room (fresh id, term 0, a fresh invite key) with the settings in `cfg`.
    pub fn new(cfg: &ServerConfig) -> RoomSave {
        let mut save = RoomSave::empty(&new_id());
        save.apply_config(cfg);
        save.invite = Some(net::new_invite());
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
            invite: None,
            channels: Vec::new(),
            peers: Vec::new(),
            profiles: Vec::new(),
        }
    }

    /// Everywhere to look for the room: its last host, then members' apps.
    pub fn places(&self) -> Vec<String> {
        let mut v = self.addrs.clone();
        v.extend(self.peers.iter().filter(|p| !self.addrs.contains(p)).cloned());
        v
    }

    /// Merge what a host says about the room's members.
    pub fn merge_profiles(&mut self, profiles: &[Profile]) {
        for p in profiles {
            self.note_profile(&p.key, &p.nick, p.since);
        }
    }

    fn note_profile(&mut self, key: &str, nick: &str, since: i64) {
        if key.is_empty() {
            return;
        }
        match self.profiles.iter_mut().find(|x| x.key == key) {
            Some(x) => {
                x.nick = nick.to_owned();
                x.since = x.since.min(since);
            }
            None => self.profiles.push(Profile { key: key.to_owned(), nick: nick.to_owned(), since }),
        }
    }

    /// Remember a member we saw: their profile, and where their app is (if it can host).
    pub fn note_member(&mut self, m: &Member) {
        if let Some(k) = m.identity.key.as_deref().filter(|_| m.identity.verified) {
            self.note_profile(k, &m.nick, crate::now_ms());
        }
        if let Some(a) = m.addr.as_ref().filter(|_| m.can_host) {
            let id = Link::parse(a).ok().map(|l| l.id());
            self.peers.retain(|p| p != a && (id.is_none() || Link::parse(p).ok().map(|l| l.id()) != id));
            self.peers.insert(0, a.clone());
            self.peers.truncate(MAX_PEERS);
        }
    }

    /// Take name, motd, channels and join rules from server settings.
    pub fn apply_config(&mut self, cfg: &ServerConfig) {
        self.name = cfg.name.clone();
        self.motd = cfg.motd.clone();
        self.channels = cfg.channel_list();
        self.access = Access { allow: cfg.allow.clone() };
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

/// This member's key for room `id`, made on first use. Only its public half
/// ever leaves the machine.
pub fn member_key(paths: &Paths, id: &str) -> anyhow::Result<SecretKey> {
    if !valid_id(id) {
        bail!("bad room id {id:?}");
    }
    net::load_or_create_key(&dir(paths, id).join("member.key"))
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
///
/// History files never carry `author_id`: member ids belong to the host that
/// assigned them, so they mean nothing once the room moves (or restarts).
pub fn spawn_persister(dir: PathBuf) -> mpsc::UnboundedSender<ChatMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChatMessage>();
    tokio::spawn(async move {
        while let Some(mut msg) = rx.recv().await {
            if !safe_channel(&msg.channel) {
                continue;
            }
            msg.author_id = None;
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
    /// What to connect to (a link, without the invite).
    pub addr: String,
    /// The host's endpoint id: the same for everyone who finds it.
    pub id: String,
    pub server: ServerInfo,
    pub members: u32,
}

/// Look for a running host of room `id` at `places` (its last hosts and
/// members' apps), all at once. See [`best_host`] for which one wins.
pub async fn find_live(net: &Net, id: &str, places: &[String]) -> Option<LiveHost> {
    let mut seen = std::collections::HashSet::new();
    let mut set = tokio::task::JoinSet::new();
    for a in places {
        let Ok(link) = Link::parse(a) else { continue };
        if !seen.insert(link.id()) {
            continue;
        }
        let (net, room) = (net.clone(), id.to_owned());
        set.spawn(async move {
            let link = link.without_invite().room(room.clone());
            match net::probe(&net, &link, Some(&room), Duration::from_secs(4)).await {
                Ok(ServerMsg::ProbeInfo { server, members, .. }) => {
                    Some(LiveHost { addr: link.to_string(), id: link.id().to_string(), server, members })
                }
                Ok(other) => {
                    tracing::debug!("{} answered the probe with {other:?}", link.short());
                    None
                }
                Err(e) => {
                    tracing::debug!("no room at {}: {e:#}", link.short());
                    None
                }
            }
        });
    }
    let mut found: Vec<LiveHost> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(h)) = r {
            found.push(h);
        }
    }
    best_host(found.into_iter().filter(|h| h.server.room_id == id))
}

/// The host members should be in when several run the same room: highest
/// term; on equal terms (two members took over at once without seeing each
/// other), an always-on `serve` beats an app, then the lowest endpoint id.
/// Everyone sees the same ids, so they all pick the same host. (Claims stay
/// strict: a claim must carry a higher term than the host's.)
pub fn best_host(hosts: impl IntoIterator<Item = LiveHost>) -> Option<LiveHost> {
    hosts.into_iter().max_by(|a, b| {
        (a.server.term, a.server.mode == "server").cmp(&(b.server.term, b.server.mode == "server")).then_with(|| b.id.cmp(&a.id))
    })
}

/// Ask the host at `addr` to move everyone to us (we host the room at a
/// higher `term` at `ours`). We prove our key and the invite, like a join.
pub async fn claim(net: &Net, addr: &str, save: &RoomSave, member: &SecretKey, term: u64, ours: Vec<String>) -> anyhow::Result<()> {
    let link = Link::parse(addr)?;
    let invite = save.invite.as_deref().map(net::invite_key).transpose()?;
    let room = save.id.clone();
    let reply = net
        .ask(link.addr, Duration::from_secs(8), |conn| {
            let proof = net::prove(conn, &room, Some(member), invite.as_ref())?;
            Ok(ClientMsg::Claim { room: room.clone(), term, addrs: ours, proof })
        })
        .await?;
    match reply {
        ServerMsg::Handoff { .. } => Ok(()),
        ServerMsg::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected reply {other:?}"),
    }
}

/// A save for the room at `addr` (a link), made from its probe answer and
/// merged into an existing save of the same room. Join rules aren't public,
/// so they come from the existing save or stay open; the invite comes from
/// the link or the existing save.
pub async fn adopt(net: &Net, paths: &Paths, addr: &str) -> anyhow::Result<RoomSave> {
    let link = Link::parse(addr)?;
    let ServerMsg::ProbeInfo { server, channels, .. } = net::probe(net, &link, None, Duration::from_secs(5)).await? else {
        bail!("{} did not answer like a tailchat room", link.short());
    };
    if !valid_id(&server.room_id) {
        bail!("{} runs a tailchat without room ids; upgrade it first", link.short());
    }
    let mut save = load(paths, &server.room_id).unwrap_or_else(|_| RoomSave::empty(&server.room_id));
    save.update_from(&server, &channels, None);
    if link.invite.is_some() {
        save.invite = link.invite.clone();
    }
    save.addrs = vec![link.without_invite().room(server.room_id.clone()).to_string()];
    Ok(save)
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
            author_key: None,
            text: text.into(),
            kind: Default::default(),
        }
    }

    #[test]
    fn save_roundtrip_and_find() {
        let home = std::env::temp_dir().join(format!("tailchat-rooms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let paths = Paths::under(&home);
        let mut s = RoomSave::new(&ServerConfig { name: "The Den".into(), allow: vec!["0123456789abcdef".into()], ..ServerConfig::default() });
        assert!(s.invite.is_some(), "new rooms are invite-only");
        s.term = 4;
        s.addrs = vec![Link::new(SecretKey::generate().public().into()).to_string()];
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
    fn best_host_breaks_term_ties_the_same_way_everywhere() {
        let h = |id: &str, term: u64, mode: &str| LiveHost {
            addr: format!("tailchat://{id}"),
            id: id.into(),
            server: ServerInfo { name: "r".into(), motd: None, version: "x".into(), mode: mode.into(), room_id: "r".into(), term },
            members: 1,
        };
        let pick = |v: Vec<LiveHost>| best_host(v).map(|x| x.id);
        assert_eq!(pick(vec![h("9", 3, "host"), h("1", 2, "server")]).as_deref(), Some("9"));
        // Same term: the order they were found in doesn't matter, and the lowest id wins.
        let a = vec![h("9", 3, "host"), h("2", 3, "host")];
        let b = vec![h("2", 3, "host"), h("9", 3, "host")];
        assert_eq!(pick(a.clone()), pick(b));
        assert_eq!(pick(a).as_deref(), Some("2"));
        assert_eq!(pick(vec![h("2", 3, "host"), h("9", 3, "server")]).as_deref(), Some("9"));
        assert!(pick(Vec::new()).is_none());
    }

    #[test]
    fn saves_remember_profiles_and_peers() {
        let mut s = RoomSave::empty("r");
        s.merge_profiles(&[Profile { key: "k1".into(), nick: "ann".into(), since: 5 }]);
        s.merge_profiles(&[Profile { key: "k1".into(), nick: "anna".into(), since: 9 }]);
        assert_eq!(s.profiles, vec![Profile { key: "k1".into(), nick: "anna".into(), since: 5 }]);
        let peer = Link::new(SecretKey::generate().public().into()).to_string();
        let m = Member {
            id: 1,
            nick: "bo".into(),
            identity: tailchat_proto::Identity { key: Some("k2".into()), verified: true },
            voice: None,
            muted: false,
            deafened: false,
            client: String::new(),
            codecs: Vec::new(),
            can_host: true,
            addr: Some(peer.clone()),
            reach: Default::default(),
        };
        s.note_member(&m);
        s.note_member(&m);
        assert_eq!(s.peers, vec![peer.clone()]);
        assert_eq!(s.profiles.len(), 2);
        s.addrs = vec!["tailchat://host".into()];
        assert_eq!(s.places(), vec!["tailchat://host".to_string(), peer]);
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
