use denchat_core::proto::NoticeLevel;
use denchat_core::session::{BUILTIN_COMMANDS, message_problem};
use egui::{CornerRadius, Frame, Key, Margin, RichText, Sense, Stroke, TextEdit, Ui, vec2};

use super::{Panel, PanelCx};
use crate::icons;
use crate::kit::{self, W};

/// Message input. Enter sends, Shift+Enter adds a line, Up recalls history,
/// Tab completes slash commands (built-in and mod).
///
/// Settings (`[panel."core.composer"]`): `placeholder` (`{channel}` is
/// replaced), `style` = `"field"` (default, rounded box) or `"pill"` (fully
/// round, for bubble layouts), `send_button` (default true).
#[derive(Default)]
pub struct Composer {
    text: String,
    history: Vec<String>,
    cursor: Option<usize>,
    focus_next: bool,
}

impl Panel for Composer {
    fn title(&self) -> String {
        "Composer".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        let channel = cx.state.focus.clone();
        let placeholder =
            cx.setting_str("placeholder").unwrap_or("Message #{channel}").replace("{channel}", channel.as_deref().unwrap_or("…"));
        let enabled = cx.state.conn.is_connected();

        // Suggestions for `/` commands.
        let mut suggestions: Vec<(String, String)> = Vec::new();
        if let Some(prefix) = self.text.strip_prefix('/').filter(|t| !t.contains(' ')) {
            for (usage, desc) in BUILTIN_COMMANDS {
                if usage[1..].starts_with(prefix) {
                    suggestions.push(((*usage).to_owned(), (*desc).to_owned()));
                }
            }
            for (name, spec) in cx.session.mods().commands() {
                if name.starts_with(prefix) {
                    let usage = spec.usage.clone().map(|u| format!("/{name} {u}")).unwrap_or_else(|| format!("/{name}"));
                    suggestions.push((usage, spec.description.clone().unwrap_or_default()));
                }
            }
        }
        if !suggestions.is_empty() {
            Frame::new()
                .fill(p.panel)
                .stroke(Stroke::new(1.0, p.line))
                .corner_radius(CornerRadius::same(p.card_radius.min(14)))
                .shadow(p.shadow_small())
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for (i, (usage, desc)) in suggestions.iter().take(6).enumerate() {
                        ui.horizontal(|ui| {
                            let rt = RichText::new(usage).monospace().color(if i == 0 { p.accent_ink } else { p.text });
                            ui.label(rt);
                            ui.label(cx.small(desc.clone()));
                            if i == 0 {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(cx.small("Tab")));
                            }
                        });
                    }
                });
            ui.add_space(6.0);
        }

        let pill = cx.setting_str("style") == Some("pill");
        let send_button = cx.setting_bool("send_button", true);
        let id = ui.make_persistent_id(("composer", cx.id));
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        // Tab completes a command while there are suggestions; otherwise it
        // moves focus on, like anywhere else (the box never types a tab).
        let tab = focused && !suggestions.is_empty() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab));
        let radius = if pill { CornerRadius::same(22) } else { CornerRadius::same(p.card_radius.min(14)) };
        let mut send_clicked = false;
        let field = Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, if focused { crate::theme::alpha(p.accent, 0.45) } else { p.line }))
            .corner_radius(radius)
            .inner_margin(Margin { left: if pill { 16 } else { 14 }, right: 6, top: 6, bottom: 6 })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let btn = if send_button { 32.0 } else { 0.0 };
                    let w = ui.available_width() - if send_button { btn + ui.spacing().item_spacing.x } else { 0.0 };
                    let resp = ui
                        .add_enabled_ui(enabled, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut self.text)
                                    .id(id)
                                    .frame(egui::Frame::NONE)
                                    .font(kit::font(p.size, W::Regular))
                                    .hint_text(RichText::new(placeholder).color(p.muted))
                                    .desired_rows(1)
                                    .desired_width(w)
                                    .min_size(vec2(w, 32.0))
                                    .align(egui::Align2::LEFT_CENTER)
                                    .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, Key::Enter))
                                    .lock_focus(false),
                            )
                        })
                        .inner;
                    if send_button {
                        let ready = enabled && !self.text.trim().is_empty();
                        let (r, b) = ui.allocate_exact_size(vec2(btn, btn), if ready { Sense::click() } else { Sense::hover() });
                        let hot = ui.ctx().animate_bool_with_time(b.id, b.hovered() && ready, 0.12);
                        let (bg, fg) = if ready {
                            (crate::theme::mix(p.accent, p.text, 0.12 * hot), p.accent_text)
                        } else {
                            (crate::theme::alpha(p.text, 0.06), p.muted)
                        };
                        let c = r.center();
                        ui.painter().circle_filled(c, btn / 2.0, bg);
                        kit::icon(ui.painter(), c, icons::ARROW_UP, 17.0, fg);
                        b.widget_info(|| egui::WidgetInfo::labeled(egui::accesskit::Role::Button, ready, "Send"));
                        send_clicked = b.on_hover_text("Send (Enter)").clicked();
                    }
                    resp
                })
                .inner
            });
        let resp = field.inner;
        if self.focus_next || (enabled && !ui.ctx().memory(|m| m.focused().is_some())) {
            resp.request_focus();
            self.focus_next = false;
        }
        if send_clicked {
            self.send(channel.clone(), cx);
        }
        if resp.has_focus() {
            let (enter, arrow_up, arrow_down) = ui.input_mut(|i| {
                (
                    // Plain Enter only: `consume_key` ignores Shift, and Shift+Enter is a new line.
                    i.events.iter().any(|e| {
                        matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if modifiers.matches_exact(egui::Modifiers::NONE))
                    }) && i.consume_key(egui::Modifiers::NONE, Key::Enter),
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::ArrowDown),
                )
            });
            if arrow_up && (self.text.is_empty() || self.cursor.is_some()) && !self.history.is_empty() {
                let next = self.cursor.map(|c| c.saturating_sub(1)).unwrap_or(self.history.len() - 1);
                self.cursor = Some(next);
                self.text = self.history[next].clone();
            } else if arrow_down && self.cursor.is_some() {
                let c = self.cursor.unwrap() + 1;
                if c >= self.history.len() {
                    self.cursor = None;
                    self.text.clear();
                } else {
                    self.cursor = Some(c);
                    self.text = self.history[c].clone();
                }
            }
            if tab {
                if let Some((usage, _)) = suggestions.first() {
                    let name = usage.split_whitespace().next().unwrap_or(usage);
                    self.text = format!("{name} ");
                    if let Some(mut state) = TextEdit::load_state(ui.ctx(), id) {
                        let end = egui::text::CCursor::new(self.text.chars().count());
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                        state.store(ui.ctx(), id);
                    }
                }
                self.focus_next = true;
            }
            if enter {
                self.send(channel, cx);
            }
        }
    }
}

impl Composer {
    fn send(&mut self, channel: Option<String>, cx: &mut PanelCx<'_>) {
        // Keep a message the host would refuse in the box, so it isn't lost.
        if !self.text.trim_start().starts_with('/')
            && let Some(problem) = message_problem(&self.text).filter(|_| !self.text.trim().is_empty())
        {
            cx.actions.session(move |s| s.notice(NoticeLevel::Warn, problem));
            self.focus_next = true;
            return;
        }
        let text = std::mem::take(&mut self.text);
        let trimmed = text.trim_end_matches('\n').to_owned();
        if !trimmed.trim().is_empty() {
            if self.history.last() != Some(&trimmed) {
                self.history.push(trimmed.clone());
                if self.history.len() > 100 {
                    self.history.remove(0);
                }
            }
            self.cursor = None;
            cx.actions.session(move |s| s.submit(channel.as_deref(), &trimmed));
        }
        self.focus_next = true;
    }
}
