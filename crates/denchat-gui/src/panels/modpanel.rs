//! Panels owned by mods (or created ad hoc through `ui.panel.set`).

use denchat_core::ui::Widget;
use egui::{RichText, Ui};

use super::{Actions, Panel, PanelCx};
use crate::theme::Palette;

pub struct ModPanel {
    pub id: String,
}

impl Panel for ModPanel {
    fn title(&self) -> String {
        self.id.clone()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let Some(content) = cx.session.ui().panel(&self.id) else {
            ui.label(cx.small(format!("{} (no content yet)", self.id)));
            return;
        };
        let p = *cx.palette;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(crate::icons::PUZZLE_PIECE).size(15.0).color(p.muted));
            ui.label(RichText::new(&content.title).font(crate::kit::font(14.5, crate::kit::W::Semibold)).color(p.text));
        });
        ui.add_space(4.0);
        if content.widgets.is_empty() {
            ui.label(cx.small("…"));
            return;
        }
        let palette = *cx.palette;
        render_widgets(ui, &content.widgets, &palette, content.owner.as_deref(), &self.id, cx.actions);
        ui.add_space(4.0);
    }
}

/// Render declarative widgets. Buttons queue mod commands / `ui.action` events.
pub fn render_widgets(ui: &mut Ui, widgets: &[Widget], palette: &Palette, owner: Option<&str>, panel: &str, actions: &mut Actions) {
    for w in widgets {
        match w {
            Widget::Heading { text } => {
                ui.label(RichText::new(text).strong().size(palette.heading_size * 0.9));
            }
            Widget::Text { text, color, mono, small } => {
                let mut rt = RichText::new(text);
                if let Some(c) = color.as_deref().and_then(|c| palette.token(c)) {
                    rt = rt.color(c);
                }
                if *mono {
                    rt = rt.monospace();
                }
                if *small {
                    rt = rt.size(palette.small_size);
                }
                ui.label(rt);
            }
            Widget::Separator => {
                ui.separator();
            }
            Widget::Button { label, command, args, action } => {
                if ui.button(label).clicked() {
                    if let Some(c) = command.clone() {
                        let owner = owner.map(str::to_owned);
                        let args = args.clone();
                        actions.session(move |s| s.run_mod_command(owner.as_deref(), &c, &args));
                    }
                    if let Some(a) = action.clone() {
                        let panel = panel.to_owned();
                        actions.session(move |s| s.ui_action(&panel, &a, serde_json::Value::Null));
                    }
                }
            }
            Widget::Kv { key, value } => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(key).color(palette.muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(value));
                });
            }
            Widget::Progress { value, text } => {
                // Label above a thin bar: text drawn over the fill is unreadable in many palettes.
                if let Some(t) = text {
                    ui.label(RichText::new(t).size(palette.small_size + 1.0));
                }
                ui.add(egui::ProgressBar::new(value.clamp(0.0, 1.0)).fill(palette.accent).desired_height(6.0));
            }
            Widget::List { items } => {
                for item in items {
                    ui.label(format!("• {item}"));
                }
            }
            Widget::Link { text, url } => {
                ui.hyperlink_to(text, url);
            }
            Widget::Row { children } => {
                ui.horizontal_wrapped(|ui| render_widgets(ui, children, palette, owner, panel, actions));
            }
        }
    }
}
