use denchat_core::session::ConnState;
use egui::{Align2, CornerRadius, RichText, Sense, Ui, pos2, vec2};

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

/// This panel's id.
pub const ID: &str = "core.header";

/// Panels that hold the View menu (the room header, and the lobby, which has
/// its own View button). View → Panels can't hide them: nothing would be left
/// on that screen to show them again. Taking one out of a layout file still works.
pub const HOLDS_VIEW_MENU: &[&str] = &[ID, super::lobby::ID];

/// Whether View → Panels may hide `id`.
pub fn can_hide(id: &str) -> bool {
    !HOLDS_VIEW_MENU.contains(&id)
}

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
    cx.state.server.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| "Den".into())
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
    let hover = kit::hover_fade(ui, resp.id, resp.hovered());
    if cx.in_card {
        painter.rect_filled(rect, radius, crate::theme::alpha(p.text, 0.04 * hover));
    } else if let Some(m) = p.material {
        painter.extend(crate::material::raised(painter, rect, radius, crate::theme::mix(p.panel, p.surface, 0.35 * hover), &p, &m));
    } else {
        painter.add(p.shadow_small().as_shape(rect, radius));
        painter.rect_filled(rect, radius, crate::theme::mix(p.panel, p.surface, 0.35 * hover));
    }
    let tile = egui::Rect::from_min_size(pos2(rect.left() + 10.0, rect.center().y - 19.0), vec2(38.0, 38.0));
    room_tile(painter, tile, &name, cx.picture(ui, cx.state.room_picture()), &p);
    let x = tile.right() + 10.0;
    let text_w = rect.right() - 34.0 - x;
    let g = kit::line(ui, name.clone(), kit::font(15.0, W::Semibold), p.text, text_w);
    kit::put(painter, &g, x, rect.center().y - 9.0, p.text);
    painter.circle_filled(pos2(x + 3.5, rect.center().y + 10.0), 3.5, dot);
    let g = kit::line(ui, line.clone(), kit::font(12.0, W::Regular), p.muted, text_w - 12.0);
    kit::put(painter, &g, x + 12.0, rect.center().y + 10.0, p.muted);
    kit::icon(painter, pos2(rect.right() - 18.0, rect.center().y), icons::CARET_DOWN, 16.0, p.muted);
    kit::name(&resp, egui::accesskit::Role::Button, format!("Room menu: {name} · {line}"));
    room_menu(&resp, cx);
}

/// The old full-width strip: room, channel, topic; menu and leave on the right.
fn bar(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let p = *cx.palette;
    ui.horizontal(|ui| {
        if let Some(tex) = cx.picture(ui, cx.state.room_picture()) {
            let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            crate::pictures::paint_rounded(ui.painter(), r, 6.0, tex);
        }
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

/// A room's tile: its picture, or its initial on the accent colour.
pub fn room_tile(painter: &egui::Painter, tile: egui::Rect, name: &str, picture: Option<egui::TextureId>, p: &crate::theme::Palette) {
    let radius = (tile.height() * 0.3).min(p.row_radius.saturating_add(2).min(12) as f32);
    match picture {
        Some(tex) => crate::pictures::paint_rounded(painter, tile, radius, tex),
        None => {
            let r = CornerRadius::same(radius as u8);
            match p.material {
                // A backlit key.
                Some(m) => {
                    painter.add(crate::material::glow(tile, r, p.accent, m.glow));
                    painter.extend(crate::material::button(painter, tile, r, p.accent, p, &m));
                }
                None => {
                    painter.rect_filled(tile, r, p.accent);
                }
            }
            let size = (tile.height() * 0.45).max(9.0);
            painter.text(tile.center(), Align2::CENTER_CENTER, kit::initial(name), kit::font(size, W::Semibold), p.accent_text);
        }
    }
}

/// Change or remove the room's picture (anyone in the room may).
fn picture_items(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    if !cx.state.conn.is_connected() {
        return;
    }
    let supported = cx.state.pictures_supported();
    let tip =
        if supported { "Anyone in the room can change it; everyone sees it" } else { "The host runs an older Den without room pictures" };
    let change = ui.add_enabled(supported, egui::Button::new(format!("{}  Change room picture…", icons::IMAGE)));
    if change.on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
        cx.actions.shell(ShellCmd::PickPicture(crate::pictures::PictureFor::Room));
        ui.close();
    }
    if supported && cx.state.room_picture().is_some() && ui.button(format!("{}  Remove room picture", icons::TRASH)).clicked() {
        cx.actions.session(|s| s.set_room_picture_file(None));
        ui.close();
    }
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
            ui.label(cx.small(format!("You're hosting as {}", denchat_core::net::short_addr(&h.addr))));
            if let Some(link) = h.share.first().cloned()
                && ui
                    .button(format!("{}  Copy invite link", icons::COPY))
                    .on_hover_text("friends paste this into Join; anyone with it can join")
                    .clicked()
            {
                ui.ctx().copy_text(link);
                cx.actions.session(|s| s.notice(denchat_core::proto::NoticeLevel::Info, "copied the invite link".into()));
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
            picture_items(ui, cx);
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
            let via = cx.state.via.map(|v| format!(" · {}", v.as_str())).unwrap_or_default();
            let line = ui.label(cx.small(format!("Connected to {}{ms}{via}", denchat_core::net::short_addr(addr))));
            if cx.state.via == Some(denchat_core::net::Via::Relay) {
                line.on_hover_text(
                    "Going through a relay: a firewall or router is blocking a direct path. It works, with a little more delay.",
                );
            }
            if ui
                .button(format!("{}  Copy invite link", icons::COPY))
                .on_hover_text("friends paste this into Join; anyone with it can join")
                .clicked()
            {
                let ctx = ui.ctx().clone();
                cx.actions.session(move |s| match s.invite_link() {
                    Some(link) => {
                        ctx.copy_text(link);
                        s.notice(denchat_core::proto::NoticeLevel::Info, "copied the invite link".into());
                    }
                    None => s.notice(denchat_core::proto::NoticeLevel::Warn, "no invite link for this room".into()),
                });
                ui.close();
            }
            picture_items(ui, cx);
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

/// Settings, theme / layout / panel toggles and reloads.
fn view_items(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    if ui.button(format!("{}  Settings", icons::GEAR_SIX)).on_hover_text("Microphone, headset, push to talk, look").clicked() {
        cx.actions.shell(ShellCmd::OpenSettings);
        ui.close();
    }
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
                let (enabled, hint) =
                    if can_hide(id) { (true, id.as_str()) } else { (!on, "holds this menu; take it out of the layout file instead") };
                if ui.add_enabled(enabled, egui::Checkbox::new(&mut on, title)).on_hover_text(hint).on_disabled_hover_text(hint).changed() {
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
