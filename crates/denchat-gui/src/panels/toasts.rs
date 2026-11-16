use denchat_core::proto::NoticeLevel;
use egui::{Frame, Margin, RichText, Ui};

use super::{Panel, PanelCx};

/// Recent notices, drawn as an overlay. Remove `core.toasts` from a layout's
/// `overlays` to switch them off.
pub struct Toasts;

impl Panel for Toasts {
    fn title(&self) -> String {
        "Notices".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let toasts = cx.session.ui().toasts(6_000);
        if !toasts.is_empty() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
        for t in toasts.iter().rev().take(4) {
            let p = cx.palette;
            let (color, glyph) = match t.level {
                NoticeLevel::Info => (p.accent, crate::icons::INFO),
                NoticeLevel::Warn => (p.warn, crate::icons::WARNING),
                NoticeLevel::Error => (p.danger, crate::icons::WARNING),
            };
            let resp = Frame::new()
                .fill(p.panel)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(egui::CornerRadius::same(p.card_radius.min(14)))
                .shadow(p.shadow)
                .inner_margin(Margin { left: 12, right: 14, top: 9, bottom: 9 })
                .show(ui, |ui| {
                    ui.set_max_width(340.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(RichText::new(glyph).size(16.0).color(color));
                        ui.label(RichText::new(&t.text).color(p.text));
                    });
                })
                .response
                .interact(egui::Sense::click());
            if resp.clicked() {
                let id = t.id;
                cx.actions.session(move |s| s.ui().dismiss_toast(id));
            }
            ui.add_space(4.0);
        }
    }
}
