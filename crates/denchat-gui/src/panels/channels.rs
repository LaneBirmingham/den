use std::collections::HashSet;

use denchat_core::proto::{Channel, Member, MessageKind};
use egui::accesskit::Role;
use egui::{CornerRadius, RichText, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::{Panel, PanelCx};
use crate::icons;
use crate::kit::{self, Face, W};
use crate::theme::Palette;

/// Text channels (click to switch) and voice channels (click to join).
///
/// Settings (`[panel."core.channels"]`):
/// * `style` — `list` (default) or `conversations` (two-line rows with the
///   last message, for messenger-style layouts)
/// * `orientation` — `vertical` (default) or `horizontal` (a tab bar, for top/bottom slots)
/// * `voice` — list voice channels too (default true)
pub struct Channels;

impl Panel for Channels {
    fn title(&self) -> String {
        "Channels".into()
    }

    fn fill(&self) -> bool {
        true
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        if cx.setting_str("orientation") == Some("horizontal") {
            return horizontal(ui, cx);
        }
        let p = *cx.palette;
        let conversations = cx.setting_str("style") == Some("conversations");
        let show_voice = cx.setting_bool("voice", true);
        ScrollArea::vertical().id_salt("channels").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            kit::section(ui, if conversations { "Chats" } else { "Channels" }, &p);
            let channels: Vec<Channel> = cx.state.text_channels().cloned().collect();
            for c in &channels {
                if conversations {
                    conversation_row(ui, cx, c);
                } else {
                    channel_row(ui, cx, c);
                }
            }
            let voice: Vec<Channel> = cx.state.voice_channels().cloned().collect();
            if show_voice && !voice.is_empty() && denchat_core::voice_supported() {
                kit::section(ui, "Voice", &p);
                let speaking = speaking_set(cx);
                for c in &voice {
                    if cx.state.voice.channel.as_deref() == Some(c.id.as_str()) {
                        voice_card(ui, cx, c, &speaking);
                    } else {
                        voice_row(ui, cx, c, &speaking);
                    }
                }
            }
            ui.add_space(6.0);
        });
    }
}

/// Members currently talking (including us while we transmit).
pub(crate) fn speaking_set(cx: &PanelCx<'_>) -> HashSet<u32> {
    let mut s: HashSet<u32> = cx.voice.map(|v| v.speakers.iter().filter(|s| s.speaking).map(|s| s.id).collect()).unwrap_or_default();
    if let (Some(me), true) = (cx.state.me, cx.voice.is_some_and(|v| v.transmitting)) {
        s.insert(me);
    }
    s
}

fn counts(cx: &PanelCx<'_>, id: &str) -> (u32, u32) {
    (cx.state.unread.get(id).copied().unwrap_or(0), cx.state.mentions.get(id).copied().unwrap_or(0))
}

fn focus(cx: &mut PanelCx<'_>, id: &str) {
    let id = id.to_owned();
    cx.actions.session(move |s| {
        s.focus_channel(&id);
    });
}

/// `# general` with an unread count or a red mention badge.
fn channel_row(ui: &mut Ui, cx: &mut PanelCx<'_>, c: &Channel) {
    let p = *cx.palette;
    let selected = cx.state.focus.as_deref() == Some(c.id.as_str());
    let (unread, mentions) = counts(cx, &c.id);
    let (rect, resp) = kit::row(ui, 34.0, selected, !cx.in_card, &p);
    let painter = ui.painter();
    let lit = selected || unread > 0;
    kit::icon(painter, pos2(rect.left() + 18.0, rect.center().y), icons::HASH, 17.0, if selected { p.text } else { p.muted });
    let mut right = rect.right() - 8.0;
    if !selected && mentions > 0 {
        right -= kit::badge(painter, ui, pos2(right, rect.center().y), &mentions.min(99).to_string(), p.danger, p.bg) + 6.0;
    } else if !selected && unread > 0 {
        right -= kit::badge(painter, ui, pos2(right, rect.center().y), &unread.min(99).to_string(), p.surface, p.text) + 6.0;
    }
    let weight = if unread > 0 && !selected { W::Semibold } else { W::Medium };
    let g = kit::line(ui, &c.name, kit::font(14.5, weight), if lit { p.text } else { p.muted }, right - rect.left() - 40.0);
    kit::put(ui.painter(), &g, rect.left() + 34.0, rect.center().y, p.text);
    kit::name_selected(&resp, Role::Button, selected, unread_label(&format!("#{}", c.name), unread, mentions));
    if resp.clicked() && !selected {
        focus(cx, &c.id);
    }
    if let Some(t) = &c.topic {
        resp.on_hover_text(t);
    }
}

/// Messenger-style: a tile, the name and time, and the last message.
fn conversation_row(ui: &mut Ui, cx: &mut PanelCx<'_>, c: &Channel) {
    let p = *cx.palette;
    let selected = cx.state.focus.as_deref() == Some(c.id.as_str());
    let (unread, mentions) = counts(cx, &c.id);
    let (rect, resp) = kit::row(ui, 58.0, selected, !cx.in_card, &p);
    let painter = ui.painter();
    let tile = egui::Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - 20.0), vec2(40.0, 40.0));
    let (tile_bg, tile_fg) = if selected { (p.accent, p.accent_text) } else { (p.surface, p.muted) };
    painter.rect_filled(tile, CornerRadius::same(p.row_radius.saturating_add(2).min(12)), tile_bg);
    kit::icon(painter, tile.center(), icons::HASH, 20.0, tile_fg);

    let last = cx.state.messages(&c.id).iter().rev().find(|m| m.kind != MessageKind::System);
    let x = tile.right() + 12.0;
    let top = rect.center().y - 10.0;
    let bottom = rect.center().y + 10.0;
    let mut right = rect.right() - 10.0;
    if let Some(m) = last {
        let time = chrono::DateTime::from_timestamp_millis(m.ts)
            .map(|t| t.with_timezone(&chrono::Local).format("%-I:%M").to_string())
            .unwrap_or_default();
        let g = kit::line(ui, time, kit::font(11.5, W::Regular), p.muted, 60.0);
        kit::put(painter, &g, right - g.size().x, top, p.muted);
        let w = g.size().x;
        let g = kit::line(ui, &c.name, kit::font(15.0, W::Semibold), p.text, right - w - 8.0 - x);
        kit::put(painter, &g, x, top, p.text);
    } else {
        let g = kit::line(ui, &c.name, kit::font(15.0, W::Semibold), p.text, right - x);
        kit::put(painter, &g, x, top, p.text);
    }
    if !selected && mentions > 0 {
        right -= kit::badge(painter, ui, pos2(right, bottom), "@", p.danger, p.bg) + 6.0;
    } else if !selected && unread > 0 {
        right -= kit::badge(painter, ui, pos2(right, bottom), &unread.min(99).to_string(), p.accent, p.accent_text) + 6.0;
    }
    let preview = match last {
        Some(m) if m.kind == MessageKind::User => {
            let text = m.text.trim_matches('_');
            if text.starts_with(&m.author) { text.to_owned() } else { format!("{}: {text}", m.author) }
        }
        Some(m) => m.text.clone(),
        None => c.topic.clone().unwrap_or_else(|| "No messages yet".into()),
    };
    let color = if unread > 0 && !selected { p.text2 } else { p.muted };
    let g = kit::line(ui, preview.replace('\n', " "), kit::font(12.5, W::Regular), color, right - x);
    kit::put(painter, &g, x, bottom, color);
    kit::name_selected(&resp, Role::Button, selected, unread_label(&format!("#{}", c.name), unread, mentions));
    if resp.clicked() && !selected {
        focus(cx, &c.id);
    }
}

fn people<'a>(cx: &'a PanelCx<'_>, channel: &'a str) -> Vec<&'a Member> {
    let mut v: Vec<&Member> = cx.state.members_in_voice(channel).collect();
    // You first, then by name, so rows don't jump around.
    v.sort_by_key(|m| (cx.state.me != Some(m.id), m.nick.to_lowercase()));
    v
}

/// A voice channel you're not in: name, who's there, and Join.
fn voice_row(ui: &mut Ui, cx: &mut PanelCx<'_>, c: &Channel, speaking: &HashSet<u32>) {
    let p = *cx.palette;
    let folks: Vec<(String, bool)> = people(cx, &c.id).iter().map(|m| (m.nick.clone(), speaking.contains(&m.id))).collect();
    let (rect, resp) = kit::row(ui, 34.0, false, false, &p);
    let painter = ui.painter();
    kit::icon(painter, pos2(rect.left() + 18.0, rect.center().y), icons::SPEAKER_HIGH, 17.0, if folks.is_empty() { p.muted } else { p.ok });
    let can_join = cx.state.conn.is_connected();
    let mut right = rect.right() - 6.0;
    if can_join {
        let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap("Join".into(), kit::font(12.5, W::Semibold), p.text));
        let pill = egui::Rect::from_min_size(pos2(right - g.size().x - 20.0, rect.center().y - 12.0), vec2(g.size().x + 20.0, 24.0));
        let hot = resp.hovered();
        painter.rect_filled(pill, CornerRadius::same(12), if hot { p.surface_hover } else { p.surface });
        painter.galley(pill.center() - g.size() / 2.0, g, p.text);
        right = pill.left() - 8.0;
    }
    if !folks.is_empty() {
        let behind = if cx.in_card { p.panel } else { p.canvas };
        let w = kit::stack_width(folks.len(), 18.0, 3);
        kit::paint_stack(ui, pos2(right - w, rect.center().y), &folks, (18.0, 3), behind, &p);
        right -= w + 8.0;
    }
    let g = kit::line(ui, &c.name, kit::font(14.5, W::Medium), p.muted, right - rect.left() - 34.0);
    kit::put(ui.painter(), &g, rect.left() + 34.0, rect.center().y, p.muted);
    let who = folks.iter().map(|f| f.0.as_str()).collect::<Vec<_>>().join(", ");
    kit::name(&resp, Role::Button, if who.is_empty() { format!("Join {}", c.name) } else { format!("Join {} ({who})", c.name) });
    let resp = resp.on_hover_text(if folks.is_empty() {
        "Nobody here yet — click to join".to_owned()
    } else {
        format!("{} — click to join", folks.iter().map(|f| f.0.as_str()).collect::<Vec<_>>().join(", "))
    });
    if resp.clicked() && can_join {
        let id = c.id.clone();
        cx.actions.session(move |s| s.voice_join(Some(id)));
    }
}

/// The voice channel you're in: everyone as a row, who's talking, and the call timer.
fn voice_card(ui: &mut Ui, cx: &mut PanelCx<'_>, c: &Channel, speaking: &HashSet<u32>) {
    let p = *cx.palette;
    let fill = if cx.in_card { p.sunk } else { p.panel };
    kit::card(&p, fill, !cx.in_card).stroke(Stroke::new(1.0, p.ok_soft)).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
        kit::name(&resp, Role::Heading, format!("{} (you're here)", c.name));
        let painter = ui.painter();
        kit::icon(painter, pos2(rect.left() + 9.0, rect.center().y), icons::SPEAKER_HIGH, 16.0, p.ok);
        let timer = cx.state.voice.joined_at.map(|t| elapsed(denchat_core::now_ms() - t));
        let mut options = vec![match &timer {
            Some(t) => format!("You're here · {t}"),
            None => "You're here".into(),
        }];
        options.extend(timer);
        // The longest pill that still leaves room for the channel's name; on a
        // narrow sidebar, just the timer, or nothing.
        const NAME_ROOM: f32 = 56.0;
        let mut name_right = rect.right();
        for text in options {
            let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text, kit::font(11.5, W::Semibold), p.ok));
            let left = rect.right() - g.size().x - 16.0;
            if left - rect.left() - 34.0 >= NAME_ROOM {
                let pill = egui::Rect::from_min_size(pos2(left, rect.center().y - 10.0), vec2(g.size().x + 16.0, 20.0));
                painter.rect_filled(pill, CornerRadius::same(10), p.ok_soft);
                painter.galley(pill.center() - g.size() / 2.0, g, p.ok);
                name_right = pill.left();
                break;
            }
        }
        let g = kit::line(ui, &c.name, kit::font(14.5, W::Semibold), p.text, name_right - rect.left() - 34.0);
        kit::put(ui.painter(), &g, rect.left() + 26.0, rect.center().y, p.text);
        if !cx.state.voice.engine {
            ui.label(cx.small("Connecting audio…"));
        }
        let time = ui.input(|i| i.time);
        for m in people(cx, &c.id) {
            let talking = speaking.contains(&m.id);
            let me = cx.state.me == Some(m.id);
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            let painter = ui.painter();
            Face::new(&m.nick, 22.0, fill).speaking(talking).paint(painter, pos2(r.left() + 13.0, r.center().y), &p, time);
            let mut right = r.right() - 4.0;
            if talking {
                kit::eq(painter, pos2(right - 12.0, r.center().y + 6.0), p.speaking, time);
                right -= 20.0;
            } else if m.deafened {
                kit::icon(painter, pos2(right - 8.0, r.center().y), icons::EAR_SLASH, 15.0, p.danger);
                right -= 22.0;
            } else if m.muted {
                kit::icon(painter, pos2(right - 8.0, r.center().y), icons::MICROPHONE_SLASH, 15.0, p.danger);
                right -= 22.0;
            }
            let name = if me { format!("{} (you)", m.nick) } else { m.nick.clone() };
            let (font, color) = if talking { (kit::font(14.0, W::Semibold), p.text) } else { (kit::font(14.0, W::Regular), p.muted) };
            let g = kit::line(ui, name.clone(), font, color, right - r.left() - 34.0);
            kit::put(ui.painter(), &g, r.left() + 32.0, r.center().y, color);
            let state = if m.deafened {
                "deafened"
            } else if m.muted {
                "muted"
            } else if talking {
                "talking"
            } else {
                ""
            };
            kit::name(&resp, Role::Label, if state.is_empty() { name } else { format!("{name} · {state}") });
            if !state.is_empty() {
                resp.on_hover_text(state);
            }
        }
    });
    ui.add_space(4.0);
}

/// `#general · 3 unread`, for the accessibility tree.
fn unread_label(name: &str, unread: u32, mentions: u32) -> String {
    match (unread, mentions) {
        (_, m) if m > 0 => format!("{name} · {m} mentions"),
        (0, _) => name.to_owned(),
        (n, _) => format!("{name} · {n} unread"),
    }
}

/// `42:10` or `1:02:03`.
pub(crate) fn elapsed(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// One row: `# general  # random 3 │ lounge 2`.
fn horizontal(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let p: Palette = *cx.palette;
    ui.horizontal_wrapped(|ui| {
        for c in cx.state.text_channels() {
            let selected = cx.state.focus.as_deref() == Some(c.id.as_str());
            let (unread, mentions) = counts(cx, &c.id);
            let mut label =
                RichText::new(format!("{} {}", icons::HASH, c.name)).color(if selected || unread > 0 { p.text } else { p.muted });
            if unread > 0 && !selected {
                label = label.font(kit::font(p.size, W::Semibold));
            }
            if ui.selectable_label(selected, label).clicked() {
                let id = c.id.clone();
                cx.actions.session(move |s| {
                    s.focus_channel(&id);
                });
            }
            if mentions > 0 && !selected {
                ui.label(RichText::new(format!("@{mentions}")).size(p.small_size).color(p.danger));
            } else if unread > 0 && !selected {
                ui.label(RichText::new(unread.min(99).to_string()).size(p.small_size).color(p.accent));
            }
        }
        let voice: Vec<_> = cx.state.voice_channels().cloned().collect();
        if !voice.is_empty() {
            ui.separator();
        }
        for c in voice {
            let here = cx.state.voice.channel.as_deref() == Some(c.id.as_str());
            let n = cx.state.members_in_voice(&c.id).count();
            let text =
                if n > 0 { format!("{} {} · {n}", icons::SPEAKER_HIGH, c.name) } else { format!("{} {}", icons::SPEAKER_HIGH, c.name) };
            let resp = ui.selectable_label(here, RichText::new(text).color(if here || n > 0 { p.ok } else { p.muted }));
            if resp.clicked() && !here {
                let id = c.id.clone();
                cx.actions.session(move |s| s.voice_join(Some(id)));
            }
        }
    });
}
