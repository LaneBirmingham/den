use egui::{Align2, CornerRadius, RichText, Sense, Ui, pos2, vec2};
use tailchat_core::session::ConnState;

use super::{Panel, PanelCx, ShellCmd};
use crate::icons;
use crate::kit::{self, W};

/// The room: name, who's online, and the room menu (invite, View, leave).
/// The View menu is how people discover that the UI is configurable, so it
/// lives here rather than in a hard-coded menu bar.
///
/// Settings (`[panel."core.header"]`): `style = "pill"` (a card for the top
/// of a sidebar) or `"bar"` (default, a full-width strip for the top slot).
#[derive(Default)]
pub struct Header;

impl Panel for Header {
    fn title(&self) -> String {
        "Header".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        if cx.setting_str("style") == Some("pill") {
            pill(ui, cx);
            if cx.state.host_lost() {
                takeover_button(ui, cx);
            }
        } else {
            bar(ui, cx);
        }
        if let Some(err) = &cx.shell.layout_error {
            ui.label(RichText::new(err).color(cx.palette.danger).size(cx.palette.small_size));
        }
    }
}

fn room_name(cx: &PanelCx<'_>) -> String {
    cx.state.server.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| "tailchat".into())
}

/// `5 online` (`· you host` when it runs here), or the connection problem.
fn presence_line(cx: &PanelCx<'_>) -> (String, egui::Color32) {
    let p = cx.palette;
    match &cx.state.conn {
        ConnState::Connected { .. } if cx.state.is_host() => (format!("{} online · you host", cx.state.members.len()), p.ok),
        ConnState::Connected { .. } => (format!("{} online", cx.state.members.len()), p.ok),
        ConnState::Connecting { attempt, .. } if *attempt > 0 => ("Reconnecting…".into(), p.warn),
        ConnState::Connecting { .. } => ("Connecting…".into(), p.warn),
        ConnState::Failed { .. } => ("Disconnected".into(), p.danger),
        ConnState::Idle => ("Offline".into(), p.muted),
    }
}

/// A card: room tile, name, online count, chevron. Click for the room menu.
fn pill(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let p = *cx.palette;
    let name = room_name(cx);
    let (line, dot) = presence_line(cx);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
    let painter = ui.painter();
    let radius = CornerRadius::same(p.card_radius.min(16));
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    if cx.in_card {
        painter.rect_filled(rect, radius, crate::theme::alpha(p.text, 0.04 * hover));
    } else {
        painter.add(p.shadow_small().as_shape(rect, radius));
        painter.rect_filled(rect, radius, crate::theme::mix(p.panel, p.surface, 0.35 * hover));
    }
    let tile = egui::Rect::from_min_size(pos2(rect.left() + 10.0, rect.center().y - 19.0), vec2(38.0, 38.0));
    painter.rect_filled(tile, CornerRadius::same(p.row_radius.saturating_add(2).min(12)), p.accent);
    painter.text(tile.center(), Align2::CENTER_CENTER, kit::initial(&name), kit::font(17.0, W::Semibold), p.accent_text);
    let x = tile.right() + 10.0;
    let text_w = rect.right() - 34.0 - x;
    let g = kit::line(ui, name, kit::font(15.0, W::Semibold), p.text, text_w);
    kit::put(painter, &g, x, rect.center().y - 9.0, p.text);
    painter.circle_filled(pos2(x + 3.5, rect.center().y + 10.0), 3.5, dot);
    let g = kit::line(ui, line, kit::font(12.0, W::Regular), p.muted, text_w - 12.0);
    kit::put(painter, &g, x + 12.0, rect.center().y + 10.0, p.muted);
    kit::icon(painter, pos2(rect.right() - 18.0, rect.center().y), icons::CARET_DOWN, 16.0, p.muted);
    room_menu(&resp, cx);
}

/// The old full-width strip: room, channel, topic; menu and leave on the right.
fn bar(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let p = *cx.palette;
    ui.horizontal(|ui| {
        ui.label(RichText::new(room_name(cx)).font(kit::font(p.heading_size, W::Semibold)));
        let (line, dot) = presence_line(cx);
        super::dot(ui, dot, 3.5);
        ui.label(cx.small(line));
        if let Some(motd) = cx.state.server.as_ref().and_then(|s| s.motd.clone()) {
            ui.label(cx.small(format!("· {motd}")));
        }
        if cx.state.host_lost() {
            takeover_button(ui, cx);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let resp = kit::icon_button(ui, icons::CARET_DOWN, "Room menu", kit::Tone::Plain, 30.0, &p);
            room_menu(&resp, cx);
        });
    });
}

/// The host is gone: run the room from your own copy (or join whoever
/// already took over).
fn takeover_button(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let Some(id) = cx.state.room_id().map(str::to_owned) else { return };
    let mut tip = "Nobody is hosting right now. Host the room from your copy; everyone else finds it here.".to_owned();
    if let Some(m) = cx.state.better_host() {
        tip.push_str(&format!("\n\n{} is better placed to host ({}): if they're around, let them.", m.nick, m.reach.short()));
    }
    if kit::pill_button(ui, "Host this room", cx.palette).on_hover_text(tip).clicked() {
        cx.actions.session(move |s| s.open_room(&id));
    }
}

/// Invite, hand off, theme / layout / panels, reload, leave.
fn room_menu(resp: &egui::Response, cx: &mut PanelCx<'_>) {
    let p = *cx.palette;
    egui::Popup::menu(resp).width(240.0).show(|ui| {
        ui.set_min_width(220.0);
        if let Some(h) = cx.state.hosting.clone() {
            ui.label(cx.small(format!("You're hosting as {}", tailchat_core::net::short_addr(&h.addr))));
            if let Some(link) = h.share.first().cloned()
                && ui.button(format!("{}  Copy invite link", icons::COPY)).on_hover_text("friends paste this into Join; anyone with it can join").clicked()
            {
                ui.ctx().copy_text(link);
                cx.actions.session(|s| s.notice(tailchat_core::proto::NoticeLevel::Info, "copied the invite link".into()));
                ui.close();
            }
            let others: Vec<(u32, String, &'static str)> =
                cx.state.can_take_over().map(|m| (m.id, m.nick.clone(), m.reach.describe())).collect();
            if others.is_empty() {
                ui.label(cx.small("Nobody else here can host yet"));
            } else {
                ui.menu_button(format!("{}  Hand off to…", icons::USERS), |ui| {
                    for (id, nick, reach) in others {
                        let tip = format!("{reach}\nThey host the room from their copy; everyone moves over.");
                        if ui.button(nick).on_hover_text(tip).clicked() {
                            cx.actions.session(move |s| s.handoff(Some(id)));
                            ui.close();
                        }
                    }
                });
            }
            ui.separator();
        } else if cx.state.host_lost() {
            ui.label(cx.small("The host is gone"));
            if let Some(m) = cx.state.better_host() {
                ui.label(cx.small(format!("{} is better placed to host ({})", m.nick, m.reach.short())));
            }
            if let Some(id) = cx.state.room_id().map(str::to_owned)
                && ui.button(format!("{}  Host this room", icons::ARROW_CLOCKWISE)).clicked()
            {
                cx.actions.session(move |s| s.open_room(&id));
                ui.close();
            }
            ui.separator();
        } else if let ConnState::Connected { addr } = &cx.state.conn {
            let ms = cx.state.latency_ms.map(|m| format!(" · {m} ms")).unwrap_or_default();
            ui.label(cx.small(format!("Connected to {}{ms}", tailchat_core::net::short_addr(addr))));
            if ui.button(format!("{}  Copy invite link", icons::COPY)).on_hover_text("friends paste this into Join; anyone with it can join").clicked() {
                let ctx = ui.ctx().clone();
                cx.actions.session(move |s| match s.invite_link() {
                    Some(link) => {
                        ctx.copy_text(link);
                        s.notice(tailchat_core::proto::NoticeLevel::Info, "copied the invite link".into());
                    }
                    None => s.notice(tailchat_core::proto::NoticeLevel::Warn, "no invite link for this room".into()),
                });
                ui.close();
            }
            ui.separator();
        }
        view_items(ui, cx);
        ui.separator();
        let leave = match &cx.state.hosting {
            Some(_) if cx.state.can_take_over().next().is_some() => "Leave (someone else hosts)",
            Some(_) => "Stop hosting",
            None => "Leave room",
        };
        if ui.button(RichText::new(format!("{}  {leave}", icons::SIGN_OUT)).color(p.danger)).clicked() {
            cx.actions.session(|s| s.leave());
            ui.close();
        }
    });
}

/// Theme / layout / panel toggles and reloads.
fn view_items(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    ui.menu_button(format!("{}  Theme", icons::PALETTE), |ui| {
        for (name, origin) in &cx.shell.themes {
            if ui.radio(cx.shell.theme == *name, name).on_hover_text(origin).clicked() {
                cx.actions.shell(ShellCmd::SetTheme(name.clone()));
            }
        }
    });
    ui.menu_button(format!("{}  Layout", icons::LAYOUT), |ui| {
        for (name, origin) in &cx.shell.layouts {
            if ui.radio(cx.shell.layout == *name, name).on_hover_text(origin).clicked() {
                cx.actions.shell(ShellCmd::SetLayout(name.clone()));
            }
        }
    });
    if !cx.shell.placed.is_empty() {
        ui.menu_button(format!("{}  Panels", icons::SQUARES_FOUR), |ui| {
            for (id, title) in &cx.shell.placed {
                let mut on = !cx.shell.hidden.contains(id);
                if ui.checkbox(&mut on, title).on_hover_text(id).changed() {
                    cx.actions.shell(ShellCmd::TogglePanel(id.clone()));
                }
            }
        });
    }
    if ui.button(format!("{}  Reload theme + layout", icons::ARROW_CLOCKWISE)).clicked() {
        cx.actions.shell(ShellCmd::ReloadAssets);
        ui.close();
    }
    if ui.button(format!("{}  Reload mods", icons::PUZZLE_PIECE)).clicked() {
        cx.actions.session(|s| s.reload_mods());
        ui.close();
    }
    if ui.button(format!("{}  Open config folder", icons::FOLDER_OPEN)).clicked() {
        cx.actions.shell(ShellCmd::OpenConfigDir);
        ui.close();
    }
}

/// The View menu as a button (the lobby uses this).
pub fn view_menu(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let resp = ui.button(format!("{}  View", icons::PALETTE));
    egui::Popup::menu(&resp).width(220.0).show(|ui| view_items(ui, cx));
}
