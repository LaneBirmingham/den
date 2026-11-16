use egui::{Align2, RichText, Sense, Ui, vec2};

use super::{Panel, PanelCx};
use crate::kit::{self, Face, W};

/// Voice channels as a stage of participant tiles (for voice-first layouts).
///
/// Settings (`[panel."core.stage"]`): `tile_size` (px, default 84),
/// `show_empty` (list empty voice channels too, default true).
pub struct Stage;

impl Panel for Stage {
    fn title(&self) -> String {
        "Stage".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        let tile = cx.settings.get("tile_size").and_then(|v| v.as_float().or(v.as_integer().map(|i| i as f64))).unwrap_or(84.0) as f32;
        let show_empty = cx.setting_bool("show_empty", true);
        let speaking: std::collections::HashSet<u32> =
            cx.voice.map(|v| v.speakers.iter().filter(|s| s.speaking).map(|s| s.id).collect()).unwrap_or_default();
        let me_tx = cx.voice.is_some_and(|v| v.transmitting);
        let channels: Vec<_> = cx.state.voice_channels().cloned().collect();
        if channels.is_empty() {
            ui.label(cx.small("no voice channels in this room"));
            return;
        }
        for c in channels {
            let people: Vec<_> = cx.state.members_in_voice(&c.id).cloned().collect();
            let here = cx.state.voice.channel.as_deref() == Some(c.id.as_str());
            if people.is_empty() && !show_empty && !here {
                continue;
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new(crate::icons::SPEAKER_HIGH).size(16.0).color(if people.is_empty() { p.muted } else { p.ok }));
                ui.label(RichText::new(&c.name).font(kit::font(14.5, W::Semibold)).color(p.text));
                ui.label(cx.small(match people.len() {
                    0 => "nobody here yet".to_owned(),
                    1 => "1 in voice".to_owned(),
                    n => format!("{n} in voice"),
                }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if here {
                        if ui.button("Leave").clicked() {
                            cx.actions.session(|s| s.voice_leave());
                        }
                    } else if cx.state.conn.is_connected() && ui.button("Join").clicked() {
                        let id = c.id.clone();
                        cx.actions.session(move |s| s.voice_join(Some(id)));
                    }
                });
            });
            if !people.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                    for m in &people {
                        let talking = speaking.contains(&m.id) || (cx.state.me == Some(m.id) && me_tx);
                        let (rect, resp) = ui.allocate_exact_size(vec2(tile, tile + 26.0), Sense::hover());
                        let painter = ui.painter_at(rect.expand(6.0));
                        painter.rect_filled(rect, egui::CornerRadius::same(p.card_radius.min(16)), p.surface);
                        let center = rect.center_top() + vec2(0.0, tile * 0.48);
                        Face::new(&m.nick, tile * 0.6, p.surface).speaking(talking).paint(&painter, center, &p, ui.input(|i| i.time));
                        let name_pos = rect.center_bottom() - vec2(0.0, 15.0);
                        painter.text(name_pos, Align2::CENTER_CENTER, &m.nick, kit::font(p.small_size + 1.0, W::Medium), p.text);
                        let flag = if m.deafened {
                            Some("deafened")
                        } else if m.muted {
                            Some("muted")
                        } else {
                            None
                        };
                        if let Some(f) = flag {
                            painter.text(
                                rect.right_top() + vec2(-6.0, 9.0),
                                Align2::RIGHT_CENTER,
                                f,
                                kit::font(p.small_size - 1.0, W::Medium),
                                p.danger,
                            );
                        }
                        if m.identity.verified {
                            resp.on_hover_text(m.identity.login.clone().unwrap_or_default());
                        }
                    }
                });
            }
            ui.add_space(8.0);
        }
    }
}
