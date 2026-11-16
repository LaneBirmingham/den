use denchat_core::proto::Member;
use egui::{Color32, Sense, Ui, pos2, vec2};

use super::{Panel, PanelCx};
use crate::icons;
use crate::kit::{self, Face, W};

/// Everyone in the room, with what they're doing (talking, muted, in which
/// voice channel) and their room key on hover.
///
/// Settings (`[panel."core.members"]`): `group_voice` (default true) folds the
/// people in your own voice channel into one row, since the voice card already
/// lists them.
pub struct Members;

impl Panel for Members {
    fn title(&self) -> String {
        "People".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        let speaking = super::channels::speaking_set(cx);
        let behind = if cx.in_card { p.panel } else { p.canvas };
        ui.spacing_mut().item_spacing.y = 2.0;

        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        let g = kit::line(ui, "People", kit::font(15.0, W::Semibold), p.text, rect.width() - 40.0);
        kit::put(ui.painter(), &g, rect.left() + 4.0, rect.center().y, p.text);
        kit::badge(ui.painter(), ui, pos2(rect.right() - 4.0, rect.center().y), &cx.state.members.len().to_string(), p.surface, p.muted);

        let mine = cx.state.voice.channel.clone();
        let mut members: Vec<&Member> = cx.state.members.values().collect();
        members.sort_by_key(|m| (m.voice.is_none(), m.nick.to_lowercase()));
        let (with_me, rest): (Vec<&Member>, Vec<&Member>) = match (&mine, cx.setting_bool("group_voice", true)) {
            (Some(ch), true) => members.into_iter().partition(|m| m.voice.as_ref() == Some(ch)),
            _ => (Vec::new(), members),
        };
        if let Some(ch) = mine.as_ref().filter(|_| !with_me.is_empty()) {
            kit::section(ui, &format!("With you in {ch}"), &p);
            let me = cx.state.me;
            let others: Vec<&str> = with_me.iter().filter(|m| Some(m.id) != me).map(|m| m.nick.as_str()).collect();
            let faces: Vec<(String, bool)> = with_me.iter().map(|m| (m.nick.clone(), speaking.contains(&m.id))).collect();
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
            let w = kit::paint_stack(ui, pos2(r.left() + 4.0, r.center().y), &faces, (26.0, 4), behind, &p);
            let x = r.left() + 4.0 + w + 10.0;
            let (title, sub) = match others.len() {
                0 => ("Just you".to_owned(), "waiting for others".to_owned()),
                _ => (others.join(", "), "and you".to_owned()),
            };
            let g = kit::line(ui, title, kit::font(14.0, W::Semibold), p.text, r.right() - x);
            kit::put(ui.painter(), &g, x, r.center().y - 8.0, p.text);
            let g = kit::line(ui, sub, kit::font(12.0, W::Regular), p.muted, r.right() - x);
            kit::put(ui.painter(), &g, x, r.center().y + 9.0, p.muted);
            resp.on_hover_text(faces.iter().map(|f| f.0.as_str()).collect::<Vec<_>>().join("\n"));
            if !rest.is_empty() {
                kit::section(ui, "Elsewhere", &p);
            }
        } else {
            kit::section(ui, "Online", &p);
        }
        for m in rest {
            member_row(ui, cx, m, speaking.contains(&m.id), behind);
        }
    }
}

fn member_row(ui: &mut Ui, cx: &PanelCx<'_>, m: &Member, talking: bool, behind: Color32) {
    let p = *cx.palette;
    let is_me = cx.state.me == Some(m.id);
    let (rect, resp) = kit::row(ui, 44.0, false, false, &p);
    let painter = ui.painter();
    Face::new(&m.nick, 32.0, behind).presence(p.ok).speaking(talking).paint(
        painter,
        pos2(rect.left() + 20.0, rect.center().y),
        &p,
        ui.input(|i| i.time),
    );
    let (status, color) = if talking {
        ("Talking".to_owned(), p.ok)
    } else if m.deafened && m.voice.is_some() {
        (format!("Deafened · {}", m.voice.clone().unwrap_or_default()), p.danger)
    } else if m.muted && m.voice.is_some() {
        (format!("Muted · {}", m.voice.clone().unwrap_or_default()), p.danger)
    } else if let Some(v) = &m.voice {
        (format!("In {v}"), p.muted)
    } else {
        ("Online".to_owned(), p.muted)
    };
    let mut right = rect.right() - 8.0;
    if m.voice.is_some() && (m.deafened || m.muted) {
        let glyph = if m.deafened { icons::EAR_SLASH } else { icons::MICROPHONE_SLASH };
        kit::icon(painter, pos2(right - 8.0, rect.center().y), glyph, 15.0, p.danger);
        right -= 22.0;
    }
    let x = rect.left() + 46.0;
    let g = kit::line(ui, &m.nick, kit::font(14.5, W::Semibold), kit::nick_color(&m.nick, &p), right - x - if is_me { 36.0 } else { 0.0 });
    let name_w = g.size().x;
    kit::put(ui.painter(), &g, x, rect.center().y - 8.0, p.text);
    if is_me {
        let tag = ui.ctx().fonts_mut(|f| f.layout_no_wrap("you".into(), kit::font(11.0, W::Semibold), p.muted));
        let r = egui::Rect::from_min_size(pos2(x + name_w + 6.0, rect.center().y - 16.0), vec2(tag.size().x + 10.0, 16.0));
        ui.painter().rect_filled(r, egui::CornerRadius::same(4), p.surface);
        ui.painter().galley(r.center() - tag.size() / 2.0, tag, p.muted);
    }
    let g = kit::line(ui, status, kit::font(12.0, W::Regular), color, right - x);
    kit::put(ui.painter(), &g, x, rect.center().y + 9.0, color);
    let who = match (m.identity.verified, m.identity.short()) {
        (true, Some(k)) => format!("Signed in with key {k}… (this room's profile for {})", m.nick),
        _ => "No key: anyone could be using this nick".to_owned(),
    };
    resp.on_hover_text(format!("{who}\nConnection: {}", m.reach.describe()));
}
