use denchat_core::net::short_addr;
use denchat_core::session::{ConnState, HostOptions};
use egui::{RichText, TextEdit, Ui};

use super::{Panel, PanelCx, ShellCmd};
use crate::pictures::PictureFor;

use crate::icons;
use crate::kit::{self, Face, W};

/// The screen before you're in a room: reopen a saved room, join one by
/// invite link, or host a new one.
/// This panel's id.
pub const ID: &str = "core.lobby";

#[derive(Default)]
pub struct Lobby {
    init: bool,
    nick: String,
    room: String,
    addr: String,
    /// Host without an invite key (anyone with the address may join).
    open: bool,
    /// Saved room waiting for a second click on "forget".
    confirm_forget: Option<String>,
}

impl Panel for Lobby {
    fn title(&self) -> String {
        "Lobby".into()
    }

    fn fill(&self) -> bool {
        true
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        let cfg = cx.session.config();
        if !self.init {
            self.init = true;
            self.nick = cx.state.nick.clone();
            self.room = format!("{}'s room", cx.state.nick);
            if cx.state.discovery.scanned_at.is_none() && !cx.state.rooms.is_empty() && cfg.modules.discovery {
                cx.actions.session(|s| s.scan());
            }
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_max_width(520.0);
                ui.add_space(24.0);
                let (r, _) = ui.allocate_exact_size(egui::vec2(52.0, 52.0), egui::Sense::hover());
                ui.painter().rect_filled(r, egui::CornerRadius::same(p.card_radius.min(16)), p.accent);
                kit::icon(ui.painter(), r.center(), icons::CHAT_CIRCLE, 28.0, p.accent_text);
                ui.add_space(8.0);
                ui.label(RichText::new("Den").font(kit::font(28.0, W::Semibold)).color(p.text));
                ui.label(cx.muted("Peer-to-peer voice and text"));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| super::header::view_menu(ui, cx));
                });
                ui.add_space(6.0);
            });
            ui.vertical_centered(|ui| {
                ui.set_max_width(520.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| self.body(ui, cx, &cfg));
            });
        });
    }
}

impl Lobby {
    fn body(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>, cfg: &denchat_core::config::AppConfig) {
        let p = *cx.palette;
        // Connection progress / errors.
        match &cx.state.conn {
            ConnState::Connecting { addr, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    // Before there's a host to dial, `addr` says what's happening instead.
                    let what = if addr.contains("://") || !addr.ends_with('…') {
                        format!("connecting to {}…", short_addr(addr))
                    } else {
                        addr.clone()
                    };
                    ui.label(what);
                    if ui.small_button("cancel").clicked() {
                        cx.actions.session(|s| s.leave());
                    }
                });
            }
            ConnState::Failed { error, .. } => {
                ui.label(RichText::new(error).color(p.danger));
            }
            _ => {}
        }

        let raised = !cx.in_card;
        card(ui, &p, raised, |ui| {
            ui.horizontal(|ui| {
                let pic = cx.my_picture(ui);
                let (r, resp) = ui.allocate_exact_size(egui::vec2(36.0, 36.0), egui::Sense::click());
                let presence = if cx.state.net.is_some() { p.ok } else { p.muted };
                Face::new(&self.nick, 36.0, p.panel).presence(presence).picture(pic).paint(ui.painter(), r.center(), &p, 0.0);
                super::settings::picture_hover(ui, &resp, r, 18.0);
                kit::name(&resp, egui::accesskit::Role::Button, "Your picture");
                if resp.on_hover_text("Change your picture").clicked() {
                    cx.actions.shell(ShellCmd::PickPicture(PictureFor::Me));
                }
                ui.add_space(4.0);
                let label = ui.label(RichText::new("Nickname").font(kit::font(14.0, W::Medium)));
                let r = ui.add(TextEdit::singleline(&mut self.nick).desired_width(200.0)).labelled_by(label.id);
                if r.lost_focus() && self.nick.trim() != cx.state.nick {
                    let n = self.nick.clone();
                    cx.actions.session(move |s| s.set_nick(&n));
                }
                let label = if cx.state.picture.is_some() { "Change picture" } else { "Add a picture" };
                if kit::icon_button(ui, icons::IMAGE, label, kit::Tone::Plain, 28.0, &p).clicked() {
                    cx.actions.shell(ShellCmd::PickPicture(PictureFor::Me));
                }
            });
            match &cx.state.net {
                Some(n) => {
                    let via = match (&n.relay, n.relays_on) {
                        (Some(r), _) => format!("reachable directly or via {}", r.trim_end_matches('/').trim_start_matches("https://")),
                        (None, true) => "no relay yet — direct connections only".to_owned(),
                        (None, false) => "relays off — direct connections only".to_owned(),
                    };
                    ui.label(cx.small(format!("device {} · {via} · {}", n.short, n.reach.short())))
                        .on_hover_text(format!("For hosting: {}", n.reach.describe()));
                }
                None => {
                    ui.label(cx.small("starting the network…"));
                }
            }
        });

        if !cx.state.rooms.is_empty() {
            ui.add_space(8.0);
            card(ui, &p, raised, |ui| self.saved_rooms(ui, cx));
        }

        ui.add_space(8.0);
        card(ui, &p, raised, |ui| {
            title(ui, "Join a room", &p);
            ui.horizontal(|ui| {
                let r = ui.add(TextEdit::singleline(&mut self.addr).hint_text("paste an invite link").desired_width(300.0));
                let go = kit::primary_button(ui, "Join", !self.addr.trim().is_empty(), &p).clicked()
                    || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.addr.trim().is_empty());
                if go {
                    self.save_nick(cx);
                    let a = self.addr.trim().to_owned();
                    cx.actions.session(move |s| s.join(a));
                }
            });
            ui.label(cx.small("Ask someone in the room for its invite link (room menu → Copy invite link)."));
            if cfg.modules.discovery && !cx.state.rooms.is_empty() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(cx.small("Your rooms that are live now"));
                    if cx.state.discovery.scanning {
                        ui.spinner();
                    } else if kit::icon_button(ui, icons::ARROW_CLOCKWISE, "Scan again", kit::Tone::Plain, 24.0, &p).clicked() {
                        cx.actions.session(|s| s.scan());
                    }
                });
                if let Some(e) = &cx.state.discovery.error {
                    ui.label(RichText::new(e).color(p.warn).size(p.small_size));
                }
                let rooms = cx.state.discovery.rooms.clone();
                if rooms.is_empty() && cx.state.discovery.scanned_at.is_some() && !cx.state.discovery.scanning {
                    ui.label(cx.small("none of your rooms are being hosted right now"));
                }
                for room in rooms {
                    ui.horizontal(|ui| {
                        if kit::pill_button(ui, "Join", &p).clicked() {
                            self.save_nick(cx);
                            let a = room.addr.clone();
                            cx.actions.session(move |s| s.join(a));
                        }
                        let (tile, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                        let pic = cx.picture(ui, room.server.picture.as_deref());
                        super::header::room_tile(ui.painter(), tile, &room.server.name, pic, &p);
                        ui.label(RichText::new(&room.server.name).strong());
                        let saved = cx.state.rooms.iter().any(|r| r.id == room.server.room_id);
                        ui.label(cx.small(format!(
                            "{} · {} online{}{}",
                            room.host,
                            room.members,
                            if room.server.mode == "server" { " · server" } else { "" },
                            if saved { " · saved" } else { "" }
                        )));
                    });
                }
            }
        });

        ui.add_space(8.0);
        card(ui, &p, raised, |ui| {
            title(ui, "Host a room", &p);
            ui.label(cx.small(
                "Runs the room on this computer; friends join with its invite link. Everyone keeps a copy, so anyone can host it later.",
            ));
            egui::Grid::new("host-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                let label = ui.label("Name");
                ui.add(TextEdit::singleline(&mut self.room).desired_width(240.0)).labelled_by(label.id);
                ui.end_row();
            });
            ui.checkbox(&mut self.open, "Open room (no invite key: anyone who finds the address can join)");
            ui.add_space(4.0);
            if kit::primary_button(ui, "Host", !matches!(cx.state.conn, ConnState::Connecting { .. }), &p).clicked() {
                self.save_nick(cx);
                let opts = HostOptions {
                    name: if self.room.trim().is_empty() { format!("{}'s room", self.nick) } else { self.room.trim().to_owned() },
                    open: self.open,
                };
                cx.actions.session(move |s| s.host(opts));
            }
        });
        ui.add_space(12.0);
        ui.label(cx.small(format!("Den {} · {} · {}", denchat_core::VERSION, cx.shell.theme, cx.shell.layout)));
    }

    /// Rooms you've been in. "Open" joins whoever hosts it now, or hosts it
    /// from your copy if nobody does.
    fn saved_rooms(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        title(ui, "Your rooms", &p);
        ui.label(cx.small("Open joins whoever is hosting it now, or hosts it from your copy."));
        let busy = matches!(cx.state.conn, ConnState::Connecting { .. });
        for room in cx.state.rooms.clone() {
            ui.horizontal(|ui| {
                if kit::pill_button(ui, "Open", &p).clicked() && !busy {
                    self.save_nick(cx);
                    let id = room.id.clone();
                    cx.actions.session(move |s| s.open_room(&id));
                }
                let (tile, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                super::header::room_tile(ui.painter(), tile, &room.name, cx.picture(ui, room.picture.as_deref()), &p);
                ui.label(RichText::new(&room.name).strong());
                let live = cx.state.discovery.rooms.iter().find(|d| d.server.room_id == room.id);
                let line = match (live, room.addrs.first()) {
                    (Some(d), _) => format!("live on {} · {} online", d.host, d.members),
                    (None, Some(a)) => format!("last hosted by {}", short_addr(a)),
                    (None, None) => "not hosted yet".into(),
                };
                ui.label(cx.small(line));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.confirm_forget.as_deref() == Some(room.id.as_str()) {
                        if ui.small_button(RichText::new("delete history").color(p.danger)).clicked() {
                            self.confirm_forget = None;
                            let id = room.id.clone();
                            cx.actions.session(move |s| {
                                if let Err(e) = s.forget_room(&id) {
                                    s.notice(denchat_core::proto::NoticeLevel::Warn, format!("{e:#}"));
                                }
                            });
                        }
                        if ui.small_button("keep").clicked() {
                            self.confirm_forget = None;
                        }
                    } else if ui.small_button("forget").on_hover_text("delete your copy of this room").clicked() {
                        self.confirm_forget = Some(room.id.clone());
                    }
                });
            });
        }
    }

    fn save_nick(&self, cx: &mut PanelCx<'_>) {
        let n = self.nick.trim().to_owned();
        if !n.is_empty() && n != cx.state.nick {
            cx.actions.session(move |s| s.set_nick(&n));
        }
    }
}

fn card(ui: &mut Ui, p: &crate::theme::Palette, raised: bool, add: impl FnOnce(&mut Ui)) {
    let mut f = egui::Frame::new()
        .fill(p.panel)
        .stroke(egui::Stroke::new(1.0, p.line))
        .corner_radius(egui::CornerRadius::same(p.card_radius.min(16)))
        .inner_margin(egui::Margin::same(16));
    if raised {
        f = f.shadow(p.shadow_small());
    }
    kit::plate(ui, p, f, kit::Plate::Raised, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        add(ui)
    });
}

fn title(ui: &mut Ui, text: &str, p: &crate::theme::Palette) {
    ui.label(RichText::new(text).font(kit::font(16.0, W::Semibold)).color(p.text));
}
