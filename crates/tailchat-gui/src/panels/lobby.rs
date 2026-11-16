use egui::{RichText, TextEdit, Ui};
use tailchat_core::session::{ConnState, HostOptions};

use super::{Panel, PanelCx};
use crate::icons;
use crate::kit::{self, Face, W};

/// The screen before you're in a room: reopen a saved room, join one, pick
/// a room discovered on your tailnet, or host a new one.
#[derive(Default)]
pub struct Lobby {
    init: bool,
    nick: String,
    room: String,
    addr: String,
    bind: String,
    port: String,
    /// Saved room waiting for a second click on "forget".
    confirm_forget: Option<String>,
}

const BINDS: &[(&str, &str)] = &[("tailnet", "Tailnet only"), ("localhost", "This computer"), ("all", "All interfaces")];

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
            self.addr = cfg.recent.first().cloned().unwrap_or_default();
            self.bind = "tailnet".into();
            self.port = cfg.port.to_string();
            if cx.state.discovery.scanned_at.is_none() && cx.session.tailscale().is_some() && cfg.modules.discovery {
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
                ui.label(RichText::new("tailchat").font(kit::font(28.0, W::Semibold)).color(p.text));
                ui.label(cx.muted("Voice and text over your tailnet"));
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
    fn body(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>, cfg: &tailchat_core::config::AppConfig) {
        let p = *cx.palette;
        // Connection progress / errors.
        match &cx.state.conn {
            ConnState::Connecting { addr, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("connecting to {addr}…"));
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
                Face::new(&self.nick, 36.0, p.panel).presence(if cx.state.tailnet.is_some() { p.ok } else { p.muted }).show(ui, &p);
                ui.add_space(4.0);
                ui.label(RichText::new("Nick").font(kit::font(14.0, W::Medium)));
                let r = ui.add(TextEdit::singleline(&mut self.nick).desired_width(200.0));
                if r.lost_focus() && self.nick.trim() != cx.state.nick {
                    let n = self.nick.clone();
                    cx.actions.session(move |s| s.set_nick(&n));
                }
            });
            if let Some(ts) = &cx.state.tailnet {
                let me = ts.me.as_ref().map(|m| format!("{} · {}", m.short_name(), m.ipv4().map(|i| i.to_string()).unwrap_or_default()));
                let line = format!("Tailscale {}{}", ts.backend_state, me.map(|m| format!(" — {m}")).unwrap_or_default());
                ui.label(cx.small(line));
            } else if cx.session.tailscale().is_none() {
                ui.label(RichText::new("Tailscale CLI not found — rooms will only work on localhost/LAN").color(p.warn).size(p.small_size));
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
                let r = ui.add(TextEdit::singleline(&mut self.addr).hint_text("machine name or host:port").desired_width(300.0));
                let go = kit::primary_button(ui, "Join", !self.addr.trim().is_empty(), &p).clicked()
                    || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.addr.trim().is_empty());
                if go {
                    self.save_nick(cx);
                    let a = self.addr.trim().to_owned();
                    cx.actions.session(move |s| s.join(a));
                }
            });
            if !cfg.recent.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(cx.small("recent:"));
                    for r in cfg.recent.iter().take(5) {
                        if ui.small_button(r).clicked() {
                            self.addr = r.clone();
                        }
                    }
                });
            }
            if cfg.modules.discovery && cx.session.tailscale().is_some() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(cx.small("On your tailnet"));
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
                    ui.label(cx.small("no rooms found"));
                }
                for room in rooms {
                    ui.horizontal(|ui| {
                        if kit::pill_button(ui, "Join", &p).clicked() {
                            self.save_nick(cx);
                            let a = room.addr.clone();
                            cx.actions.session(move |s| s.join(a));
                        }
                        ui.label(RichText::new(&room.server.name).strong());
                        let saved = cx.state.rooms.iter().any(|r| r.id == room.server.room_id);
                        ui.label(cx.small(format!(
                            "{} · {} online{}{}",
                            room.peer.short_name(),
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
                "Runs the room on this computer; friends join with your machine name. Everyone keeps a copy, so anyone can host it later.",
            ));
            egui::Grid::new("host-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Name");
                ui.add(TextEdit::singleline(&mut self.room).desired_width(240.0));
                ui.end_row();
                ui.label("Reachable from");
                let shown = BINDS.iter().find(|(k, _)| *k == self.bind).map(|(_, l)| l.to_string()).unwrap_or_else(|| self.bind.clone());
                egui::ComboBox::from_id_salt("host-bind").selected_text(shown).show_ui(ui, |ui| {
                    for (k, l) in BINDS {
                        ui.selectable_value(&mut self.bind, k.to_string(), *l);
                    }
                });
                ui.end_row();
                ui.label("Port");
                ui.add(TextEdit::singleline(&mut self.port).desired_width(80.0));
                ui.end_row();
            });
            let port = self.port.trim().parse::<u16>().ok();
            if port.is_none() {
                ui.label(RichText::new("port must be a number").color(p.warn).size(p.small_size));
            }
            ui.add_space(4.0);
            if kit::primary_button(ui, "Host", port.is_some() && !matches!(cx.state.conn, ConnState::Connecting { .. }), &p).clicked() {
                self.save_nick(cx);
                let opts = HostOptions {
                    name: if self.room.trim().is_empty() { format!("{}'s room", self.nick) } else { self.room.trim().to_owned() },
                    bind: self.bind.clone(),
                    port: port.unwrap_or(cfg.port),
                };
                cx.actions.session(move |s| s.host(opts));
            }
        });
        ui.add_space(12.0);
        ui.label(cx.small(format!("tailchat {} · {} · {}", tailchat_core::VERSION, cx.shell.theme, cx.shell.layout)));
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
                ui.label(RichText::new(&room.name).strong());
                let live = cx.state.discovery.rooms.iter().find(|d| d.server.room_id == room.id);
                let line = match (live, room.addrs.first()) {
                    (Some(d), _) => format!("live on {} · {} online", d.peer.short_name(), d.members),
                    (None, Some(a)) => format!("last hosted at {a}"),
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
                                    s.notice(tailchat_core::proto::NoticeLevel::Warn, format!("{e:#}"));
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
    f.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        add(ui)
    });
}

fn title(ui: &mut Ui, text: &str, p: &crate::theme::Palette) {
    ui.label(RichText::new(text).font(kit::font(16.0, W::Semibold)).color(p.text));
}
