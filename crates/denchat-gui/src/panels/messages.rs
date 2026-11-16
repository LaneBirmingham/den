use std::collections::HashSet;
use std::sync::Arc;

use denchat_core::proto::{ChatMessage, MemberId, MessageKind};
use egui::accesskit::Role;
use egui::text::{LayoutJob, TextFormat};
use egui::{Align, CornerRadius, Frame, Galley, Layout, Margin, Rect, RichText, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::{Panel, PanelCx, ShellCmd, nick_color};
use crate::icons;
use crate::kit::{self, Face, Tone, W};
use crate::theme::Palette;

/// The message list for the focused channel.
///
/// Settings (`[panel."core.messages"]`):
/// * `style` — `cozy` (default: avatars, names above grouped messages),
///   `bubbles` (messenger-style, yours on the right), `compact` (one line per
///   message, ordered by `row`) or `irc` (`<nick> text`)
/// * `row` — token order for `compact`: `time`, `author`, `text`
/// * `time_format`, `group`, `show_system`, `header`
pub struct Messages;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Cozy,
    Bubbles,
    Compact,
    Irc,
}

/// What every style needs to know about one message.
struct View<'a> {
    time: String,
    emote: bool,
    text: &'a str,
    mention: bool,
    own: bool,
}

impl<'a> View<'a> {
    fn of(m: &'a ChatMessage, fmt: &str, mention: bool, me: Option<MemberId>) -> View<'a> {
        let time = chrono::DateTime::from_timestamp_millis(m.ts)
            .map(|t| t.with_timezone(&chrono::Local).format(fmt).to_string())
            .unwrap_or_default();
        // `/me waves` arrives as `_nick waves_`.
        let emote_body = m
            .text
            .strip_prefix('_')
            .and_then(|t| t.strip_suffix('_'))
            .and_then(|t| t.strip_prefix(m.author.as_str()))
            .and_then(|t| t.strip_prefix(' '))
            .filter(|_| m.kind == MessageKind::User);
        let (emote, text) = match emote_body {
            Some(rest) => (true, rest),
            None => (false, m.text.as_str()),
        };
        let own = m.kind == MessageKind::User && m.author_id.is_some() && m.author_id == me;
        View { time, emote, text, mention, own }
    }
}

fn is_emote(m: &ChatMessage) -> bool {
    m.text.starts_with('_') && m.text.ends_with('_')
}

/// A message plus where it sits in its group, after dividers are worked out.
enum Item<'a> {
    Day(String),
    New,
    Msg { m: &'a ChatMessage, start: bool, end: bool },
}

/// Group consecutive messages by one author (within five minutes), and put
/// day and "new" dividers in. Emotes and system lines stand alone.
fn plan<'a>(messages: &'a [ChatMessage], show_system: bool, group: bool, new_since: Option<i64>, me: Option<MemberId>) -> Vec<Item<'a>> {
    let mut out: Vec<Item<'a>> = Vec::new();
    let mut day = None;
    let mut new_done = new_since.is_none();
    let mut prev: Option<&ChatMessage> = None;
    let joins = |a: &ChatMessage, b: &ChatMessage| {
        group
            && a.kind == MessageKind::User
            && b.kind == MessageKind::User
            && !is_emote(a)
            && !is_emote(b)
            && a.author == b.author
            && b.ts - a.ts < 5 * 60_000
    };
    for m in messages {
        if m.kind == MessageKind::System && !show_system {
            continue;
        }
        let mut broke = false;
        let d = kit::local_day(m.ts);
        if d.is_some() && d != day {
            day = d;
            out.push(Item::Day(kit::day_label(m.ts)));
            broke = true;
        }
        if !new_done
            && new_since.is_some_and(|t| m.ts >= t)
            && m.kind != MessageKind::System
            && !(m.author_id.is_some() && m.author_id == me)
        {
            new_done = true;
            out.push(Item::New);
            broke = true;
        }
        let start = broke || !prev.is_some_and(|q| joins(q, m));
        if start && let Some(Item::Msg { end, .. }) = out.iter_mut().rev().find(|i| matches!(i, Item::Msg { .. })) {
            *end = true;
        }
        out.push(Item::Msg { m, start, end: false });
        prev = Some(m);
    }
    if let Some(Item::Msg { end, .. }) = out.iter_mut().rev().find(|i| matches!(i, Item::Msg { .. })) {
        *end = true;
    }
    out
}

impl Panel for Messages {
    fn title(&self) -> String {
        "Messages".into()
    }

    fn fill(&self) -> bool {
        true
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        let Some(channel) = cx.state.focus.clone() else {
            ui.centered_and_justified(|ui| ui.label(cx.muted("No channel")));
            return;
        };
        let style = match cx.setting_str("style") {
            Some("bubbles") => Style::Bubbles,
            Some("irc") => Style::Irc,
            Some("compact" | "flat") => Style::Compact,
            _ => Style::Cozy,
        };
        if cx.setting_bool("header", false) {
            header(ui, cx, &channel, style == Style::Bubbles);
        }
        let row = cx.setting_list("row").unwrap_or_else(|| vec!["time".into(), "author".into(), "text".into()]);
        let time_format = cx.setting_str("time_format").unwrap_or("%H:%M").to_owned();
        let group = cx.setting_bool("group", true) && style != Style::Irc;
        let show_system = cx.setting_bool("show_system", true);
        let me = cx.state.me;
        let messages = cx.state.messages(&channel);
        let nicks: HashSet<String> =
            cx.state.members.values().map(|m| m.nick.to_lowercase()).chain([cx.state.nick.to_lowercase()]).collect();
        let items = plan(messages, show_system, group, cx.state.new_since.get(&channel).copied(), me);
        let behind = if cx.in_card { p.bg } else { p.panel };
        ScrollArea::vertical().id_salt(("messages", &channel)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if messages.is_empty() {
                welcome(ui, cx, &channel);
            }
            for item in &items {
                match item {
                    Item::Day(d) if style != Style::Irc && style != Style::Compact => kit::day_divider(ui, d, &p),
                    Item::New if style == Style::Bubbles => kit::new_divider(ui, "New messages", true, &p),
                    Item::New if style == Style::Cozy => kit::new_divider(ui, "New", false, &p),
                    Item::Msg { m, start, end } => {
                        let v = View::of(m, &time_format, cx.state.mentions_me(m), me);
                        match style {
                            Style::Cozy => cozy(ui, m, &v, *start, &p, &nicks, behind),
                            Style::Bubbles => bubble(ui, m, &v, *start, *end, &p, &nicks),
                            Style::Compact => {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                if *start && !std::ptr::eq(*m, &messages[0]) {
                                    ui.add_space(3.0);
                                }
                                compact_row(ui, m, &v, &row, !*start, &p);
                            }
                            Style::Irc => {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                irc_row(ui, m, &v, &p);
                            }
                        }
                    }
                    _ => {}
                }
            }
            ui.add_space(10.0);
        });
    }
}

/// `# general` and the topic, with a people toggle; or, for bubbles, a
/// conversation header with a voice pill.
fn header(ui: &mut Ui, cx: &mut PanelCx<'_>, channel: &str, big: bool) {
    let p = *cx.palette;
    let Some(c) = cx.state.channel(channel).cloned() else { return };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), if big { 58.0 } else { 54.0 }), Sense::hover());
    kit::name(
        &resp,
        Role::Heading,
        match &c.topic {
            Some(t) => format!("#{} · {t}", c.name),
            None => format!("#{}", c.name),
        },
    );
    let y = rect.center().y;
    let mut right = rect.right() - 8.0;
    let members_placed = cx.shell.placed.iter().any(|(id, _)| id == "core.members");
    if members_placed && !big {
        let shown = !cx.shell.hidden.contains("core.members");
        let r = Rect::from_center_size(pos2(right - 17.0, y), vec2(34.0, 34.0));
        let tone = if shown { Tone::On } else { Tone::Plain };
        if kit::icon_button_at(ui, r, "people", icons::USERS, if shown { "Hide people" } else { "Show people" }, tone, &p).clicked() {
            cx.actions.shell(ShellCmd::TogglePanel("core.members".into()));
        }
        right = r.left() - 6.0;
    }
    if big {
        right = voice_pill(ui, cx, right, y);
    }
    let painter = ui.painter();
    let mut x = rect.left() + 14.0;
    if big {
        let tile = Rect::from_min_size(pos2(x, y - 19.0), vec2(38.0, 38.0));
        painter.rect_filled(tile, CornerRadius::same(p.row_radius.saturating_add(2).min(12)), p.accent);
        kit::icon(painter, tile.center(), icons::HASH, 20.0, p.accent_text);
        x = tile.right() + 12.0;
        let g = kit::line(ui, &c.name, kit::font(16.0, W::Semibold), p.text, right - x);
        kit::put(painter, &g, x, y - 9.0, p.text);
        let n = cx.state.members.len();
        let sub = match &c.topic {
            Some(t) => format!("{n} members · {t}"),
            None => format!("{n} members"),
        };
        let g = kit::line(ui, sub, kit::font(12.5, W::Regular), p.muted, right - x);
        kit::put(ui.painter(), &g, x, y + 10.0, p.muted);
    } else {
        kit::icon(painter, pos2(x + 8.0, y - if c.topic.is_some() { 9.0 } else { 0.0 }), icons::HASH, 19.0, p.muted);
        x += 22.0;
        let ty = if c.topic.is_some() { y - 9.0 } else { y };
        let g = kit::line(ui, &c.name, kit::font(17.0, W::Semibold), p.text, right - x);
        kit::put(painter, &g, x, ty, p.text);
        if let Some(t) = &c.topic {
            let g = kit::line(ui, t, kit::font(13.0, W::Regular), p.muted, right - rect.left() - 14.0);
            kit::put(ui.painter(), &g, rect.left() + 14.0, y + 11.0, p.muted);
        }
    }
    // Edge to edge across the card (the shell gives the centre card a 10px inset).
    let r = ui.max_rect();
    let ext = if cx.in_card { 10.0 } else { 0.0 };
    ui.painter().with_clip_rect(r.expand2(vec2(ext, 0.0))).hline(r.left() - ext..=r.right() + ext, rect.bottom(), Stroke::new(1.0, p.line));
    ui.add_space(2.0);
}

/// `In lounge · 4` when you're in voice, otherwise `Join voice`. Returns the new right edge.
fn voice_pill(ui: &mut Ui, cx: &mut PanelCx<'_>, right: f32, y: f32) -> f32 {
    let p = *cx.palette;
    if !denchat_core::voice_supported() {
        return right;
    }
    let (text, fg, bg, join) = match cx.state.voice.channel.clone() {
        Some(ch) => {
            let n = cx.state.members_in_voice(&ch).count();
            (format!("{}  In {ch} · {n}", icons::PHONE), p.ok, p.ok_soft, None)
        }
        None => {
            let Some(target) = cx.state.voice_channels().max_by_key(|c| cx.state.members_in_voice(&c.id).count()).cloned() else {
                return right;
            };
            if !cx.state.conn.is_connected() {
                return right;
            }
            (format!("{}  Join {}", icons::PHONE, target.name), p.text, p.surface, Some(target.id))
        }
    };
    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text, kit::font(12.5, W::Semibold), fg));
    let r = Rect::from_min_size(pos2(right - g.size().x - 24.0, y - 15.0), vec2(g.size().x + 24.0, 30.0));
    let resp = ui.interact(r, ui.scope_id().with("voice-pill"), Sense::click());
    let hot = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    ui.painter().rect_filled(r, CornerRadius::same(15), crate::theme::mix(bg, fg, 0.08 * hot));
    kit::name(&resp, Role::Button, g.text().trim_start_matches(icons::PHONE).trim());
    ui.painter().galley(r.center() - g.size() / 2.0, g, fg);
    if let Some(id) = join {
        if resp.on_hover_text("Join voice").clicked() {
            cx.actions.session(move |s| s.voice_join(Some(id)));
        }
    } else {
        resp.on_hover_text("You're in voice");
    }
    r.left() - 8.0
}

/// The empty-channel intro.
fn welcome(ui: &mut Ui, cx: &PanelCx<'_>, channel: &str) {
    let p = cx.palette;
    ui.add_space(24.0);
    Frame::new().inner_margin(Margin::symmetric(16, 0)).show(ui, |ui| {
        let (r, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(p.card_radius.min(16)), p.surface);
        kit::icon(ui.painter(), r.center(), icons::HASH, 28.0, p.muted);
        ui.add_space(10.0);
        ui.label(RichText::new(format!("Welcome to #{channel}")).font(kit::font(22.0, W::Semibold)).color(p.text));
        ui.label(RichText::new(format!("This is the start of #{channel}.")).color(p.muted));
    });
}

/// Body text with `@nick` mentions picked out.
fn body_job(text: &str, size: f32, color: egui::Color32, italic: bool, p: &Palette, nicks: &HashSet<String>, wrap: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(40.0);
    let normal = TextFormat { font_id: kit::font(size, W::Regular), color, italics: italic, ..Default::default() };
    let tag = TextFormat {
        font_id: kit::font(size, W::Medium),
        color: p.accent_ink,
        background: p.accent_soft,
        italics: italic,
        ..Default::default()
    };
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let after = &rest[at + 1..];
        let end = after.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')).unwrap_or(after.len());
        let word = after[..end].trim_end_matches('.');
        if !word.is_empty() && nicks.contains(&word.to_lowercase()) {
            job.append(&rest[..at], 0.0, normal.clone());
            job.append(&rest[at..at + 1 + word.len()], 0.0, tag.clone());
            rest = &rest[at + 1 + word.len()..];
        } else {
            job.append(&rest[..at + 1], 0.0, normal.clone());
            rest = &rest[at + 1..];
        }
    }
    job.append(rest, 0.0, normal);
    job
}

fn copy_menu(resp: &egui::Response, text: &str) {
    resp.context_menu(|ui| {
        if ui.button(format!("{}  Copy text", icons::COPY)).clicked() {
            ui.ctx().copy_text(text.to_owned());
            ui.close();
        }
    });
}

const AVATAR: f32 = 36.0;
const TEXT_X: f32 = 64.0;

/// Avatar-led rows: name and time above a group, hover highlight, full-row mentions.
fn cozy(ui: &mut Ui, m: &ChatMessage, v: &View<'_>, start: bool, p: &Palette, nicks: &HashSet<String>, behind: egui::Color32) {
    let w = ui.available_width();
    let wrap = w - TEXT_X - 14.0;
    if m.kind == MessageKind::System {
        let (glyph, ic) = if v.text.ends_with(" joined") || v.text.contains(" joined ") {
            (icons::ARROW_RIGHT, p.ok)
        } else if v.text.ends_with(" left") {
            (icons::ARROW_LEFT, p.muted)
        } else {
            (icons::INFO, p.muted)
        };
        ui.add_space(4.0);
        let text = format!("{}   {}", v.text, v.time);
        let g = ui.ctx().fonts_mut(|f| f.layout(text, kit::font(13.0, W::Regular), p.muted, wrap));
        let (rect, resp) = ui.allocate_exact_size(vec2(w, g.size().y.max(18.0) + 6.0), Sense::hover());
        kit::name(&resp, Role::Label, v.text);
        kit::icon(ui.painter(), pos2(rect.left() + 14.0 + AVATAR / 2.0, rect.top() + 3.0 + 9.0), glyph, 15.0, ic);
        ui.painter().galley(pos2(rect.left() + TEXT_X, rect.top() + 3.0), g, p.muted);
        return;
    }
    let server = m.kind == MessageKind::Server;
    let start = start || server;
    let (body, italic, color) =
        if v.emote { (format!("{} {}", m.author, v.text), true, p.muted) } else { (v.text.to_owned(), false, p.text2) };
    let job = body_job(&body, p.size, color, italic, p, nicks, wrap);
    let g: Arc<Galley> = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let head = if start { 21.0 } else { 0.0 };
    let pad_top = if start { 4.0 } else { 1.0 };
    if start {
        ui.add_space(10.0);
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(w, pad_top + head + g.size().y + 3.0), Sense::click());
    kit::name(&resp, Role::ListItem, message_label(m, v, &body));
    let hovered = resp.hovered();
    let t = ui.ctx().animate_bool_with_time(resp.id, hovered, 0.1);
    let painter = ui.painter();
    let radius = CornerRadius::same(p.row_radius);
    if v.mention {
        let bg = rect.expand2(vec2(0.0, if start { 2.0 } else { 0.0 }));
        painter.rect_filled(bg, radius, p.mention);
        painter.rect_filled(
            Rect::from_min_size(bg.min, vec2(3.0, bg.height())),
            CornerRadius { nw: p.row_radius, sw: p.row_radius, ne: 0, se: 0 },
            p.accent,
        );
    } else if t > 0.0 {
        painter.rect_filled(rect, radius, crate::theme::alpha(p.text, 0.035 * t));
    }
    let top = rect.top() + pad_top;
    if start {
        let center = pos2(rect.left() + 14.0 + AVATAR / 2.0, top + AVATAR / 2.0);
        if server {
            painter.circle_filled(center, AVATAR / 2.0, p.accent_soft);
            kit::icon(painter, center, icons::INFO, 18.0, p.accent_ink);
        } else {
            Face::new(&m.author, AVATAR, behind).paint(painter, center, p, 0.0);
        }
        let (name, nc) = if server {
            (if m.author.is_empty() { "server".to_owned() } else { m.author.clone() }, p.accent_ink)
        } else {
            (m.author.clone(), nick_color(&m.author, p))
        };
        let ng = ui.ctx().fonts_mut(|f| f.layout_no_wrap(name, kit::font(14.5, W::Semibold), nc));
        let nw = ng.size().x;
        ui.painter().galley(pos2(rect.left() + TEXT_X, top), ng, nc);
        let tg = ui.ctx().fonts_mut(|f| f.layout_no_wrap(v.time.clone(), kit::font(11.5, W::Regular), p.muted));
        ui.painter().galley(pos2(rect.left() + TEXT_X + nw + 8.0, top + 3.0), tg, p.muted);
    } else if hovered {
        let tg = ui.ctx().fonts_mut(|f| f.layout_no_wrap(v.time.clone(), kit::font(10.5, W::Regular), p.muted));
        ui.painter().galley(pos2(rect.left() + TEXT_X - 10.0 - tg.size().x, top + 3.0), tg, p.muted);
    }
    ui.painter().galley(pos2(rect.left() + TEXT_X, top + head), g, color);
    let resp = match &m.author_key {
        Some(key) if start && !server => resp.on_hover_text_at_pointer(format!("key {}…", &key[..key.len().min(8)])),
        _ => resp,
    };
    copy_menu(&resp, &body);
}

/// Messenger style: others on the left, yours on the right in the accent colour.
fn bubble(ui: &mut Ui, m: &ChatMessage, v: &View<'_>, start: bool, end: bool, p: &Palette, nicks: &HashSet<String>) {
    let w = ui.available_width();
    if m.kind != MessageKind::User || v.emote {
        let text = match m.kind {
            MessageKind::Server => format!("{}: {}", if m.author.is_empty() { "server" } else { &m.author }, v.text),
            _ if v.emote => format!("{} {}", m.author, v.text),
            _ => v.text.to_owned(),
        };
        ui.add_space(8.0);
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            let rt = RichText::new(text).size(p.small_size + 0.5).color(p.muted);
            ui.label(if v.emote { rt.italics() } else { rt });
        });
        ui.add_space(4.0);
        return;
    }
    if start {
        ui.add_space(10.0);
    } else {
        ui.add_space(2.0);
    }
    let gutter = 36.0;
    let max_w = (w * 0.72).clamp(160.0, 560.0);
    let (fill, color) = match (v.own, v.mention) {
        (true, _) => (p.accent, p.accent_text),
        (false, true) => (p.mention, p.text),
        (false, false) => (p.bubble, p.text),
    };
    let job = body_job(v.text, p.size, color, false, p, nicks, max_w - 28.0);
    let g = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let head = if start && !v.own { 19.0 } else { 0.0 };
    let size = g.size() + vec2(28.0, 16.0);
    let (row, _) = ui.allocate_exact_size(vec2(w, head + size.y), Sense::hover());
    let bubble = if v.own {
        Rect::from_min_size(pos2(row.right() - 10.0 - size.x, row.top() + head), size)
    } else {
        Rect::from_min_size(pos2(row.left() + 8.0 + gutter, row.top() + head), size)
    };
    let resp = ui.interact(bubble, ui.scope_id().with(("bubble", m.id, m.ts)), Sense::click());
    kit::name(&resp, Role::ListItem, message_label(m, v, v.text));
    let (big, small) = (18u8, 5u8);
    let mut c = CornerRadius::same(big);
    let joined_top = !start;
    let joined_bottom = !end;
    if v.own {
        if joined_top {
            c.ne = small;
        }
        if joined_bottom {
            c.se = small;
        }
    } else {
        if joined_top {
            c.nw = small;
        }
        if joined_bottom {
            c.sw = small;
        }
    }
    let painter = ui.painter();
    painter.rect_filled(bubble, c, fill);
    if !v.own && v.mention {
        painter.rect_stroke(bubble, c, Stroke::new(1.0, p.accent_soft), egui::StrokeKind::Inside);
    } else if !v.own && !p.dark {
        painter.rect_stroke(bubble, c, Stroke::new(1.0, p.line), egui::StrokeKind::Inside);
    }
    painter.galley(bubble.min + vec2(14.0, 8.0), g, color);
    if !v.own {
        if start {
            let nc = nick_color(&m.author, p);
            let ng = ui.ctx().fonts_mut(|f| f.layout_no_wrap(m.author.clone(), kit::font(12.5, W::Semibold), nc));
            let nw = ng.size().x;
            ui.painter().galley(pos2(bubble.left() + 4.0, row.top()), ng, nc);
            let tg = ui.ctx().fonts_mut(|f| f.layout_no_wrap(v.time.clone(), kit::font(11.0, W::Regular), p.muted));
            ui.painter().galley(pos2(bubble.left() + 4.0 + nw + 8.0, row.top() + 1.5), tg, p.muted);
        }
        if end {
            Face::new(&m.author, 28.0, p.bg).paint(ui.painter(), pos2(row.left() + 8.0 + 14.0, bubble.bottom() - 14.0), p, 0.0);
        }
    }
    let resp = resp.on_hover_text(&v.time);
    copy_menu(&resp, v.text);
}

/// `bob: hi there` (`you: …` for your own), for the accessibility tree.
fn message_label(m: &ChatMessage, v: &View<'_>, body: &str) -> String {
    let who = match m.kind {
        MessageKind::Server if m.author.is_empty() => "server",
        _ if v.own => "you",
        _ => &m.author,
    };
    if v.emote { body.to_owned() } else { format!("{who}: {body}") }
}

/// One line per message: `time author text`, in the order `row` says.
fn compact_row(ui: &mut Ui, m: &ChatMessage, v: &View<'_>, row: &[String], grouped: bool, p: &Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for token in row {
            match token.as_str() {
                "time" => {
                    let t = if grouped { " ".repeat(v.time.len()) } else { v.time.clone() };
                    ui.label(RichText::new(t).monospace().size(p.small_size).color(p.muted));
                }
                "author" => match m.kind {
                    MessageKind::User if grouped => {}
                    MessageKind::User => {
                        let label = ui.label(
                            RichText::new(if v.emote { format!("* {}", m.author) } else { m.author.clone() })
                                .font(kit::font(p.size, W::Semibold))
                                .color(nick_color(&m.author, p)),
                        );
                        if let Some(key) = &m.author_key {
                            label.on_hover_text(format!("key {}…", &key[..key.len().min(8)]));
                        }
                    }
                    MessageKind::Server => {
                        ui.label(RichText::new(if m.author.is_empty() { "server" } else { &m.author }).strong().color(p.accent));
                    }
                    MessageKind::System => {
                        ui.label(RichText::new("—").color(p.muted));
                    }
                },
                "text" => {
                    let mut rt = RichText::new(v.text);
                    rt = match m.kind {
                        MessageKind::System => rt.color(p.muted).size(p.small_size + 1.0),
                        _ if v.emote => rt.italics(),
                        _ => rt,
                    };
                    if v.mention {
                        rt = rt.background_color(p.mention);
                    }
                    ui.label(rt);
                }
                _ => {}
            }
        }
    });
}

/// `[12:04] <bob> text`, `* bob waves`, `-!- bob joined`.
fn irc_row(ui: &mut Ui, m: &ChatMessage, v: &View<'_>, p: &Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let mono = |s: String| RichText::new(s).monospace();
        ui.label(mono(format!("[{}] ", v.time)).color(p.muted));
        match m.kind {
            MessageKind::System => {
                ui.label(mono("-!- ".into()).color(p.accent));
                ui.label(mono(v.text.to_owned()).color(p.muted));
            }
            MessageKind::Server => {
                let who = if m.author.is_empty() { "server" } else { &m.author };
                ui.label(mono(format!("-{who}- ")).color(p.accent).strong());
                ui.label(mono(v.text.to_owned()));
            }
            MessageKind::User if v.emote => {
                ui.label(mono(" * ".into()).color(p.muted));
                ui.label(mono(format!("{} ", m.author)).color(nick_color(&m.author, p)).strong());
                ui.label(mono(v.text.to_owned()).italics());
            }
            MessageKind::User => {
                ui.label(mono("<".into()).color(p.muted));
                ui.label(mono(m.author.clone()).color(nick_color(&m.author, p)).strong());
                ui.label(mono("> ".into()).color(p.muted));
                let mut rt = mono(v.text.to_owned());
                if v.mention {
                    rt = rt.background_color(p.mention);
                }
                ui.label(rt);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64, author: &str, ts: i64, text: &str) -> ChatMessage {
        ChatMessage {
            id,
            channel: "general".into(),
            ts,
            author_id: None,
            author: author.into(),
            author_key: None,
            text: text.into(),
            kind: MessageKind::User,
        }
    }

    #[test]
    fn groups_and_dividers() {
        let base = chrono::Local::now().timestamp_millis();
        let list = vec![
            msg(1, "kai", base, "hi"),
            msg(2, "kai", base + 1000, "again"),
            msg(3, "amy", base + 2000, "yo"),
            msg(4, "amy", base + 3000, "new one"),
        ];
        let items = plan(&list, true, true, Some(base + 3000), None);
        let shape: Vec<String> = items
            .iter()
            .map(|i| match i {
                Item::Day(_) => "day".into(),
                Item::New => "new".into(),
                Item::Msg { m, start, end } => format!("{}{}{}", m.id, if *start { "s" } else { "" }, if *end { "e" } else { "" }),
            })
            .collect();
        assert_eq!(shape, ["day", "1s", "2e", "3se", "new", "4se"]);
    }
}
