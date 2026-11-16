use denchat_core::session::ConnState;
use egui::{RichText, Ui};

use super::{Panel, PanelCx};

/// One-line status bar.
pub struct Status;

impl Panel for Status {
    fn title(&self) -> String {
        "Status".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = cx.palette;
        ui.horizontal(|ui| {
            let (dot, text) = match &cx.state.conn {
                ConnState::Connected { addr } => (p.ok, format!("connected · {addr}")),
                ConnState::Connecting { addr, attempt } if *attempt > 0 => (p.warn, format!("reconnecting to {addr} (try {attempt})")),
                ConnState::Connecting { addr, .. } => (p.warn, format!("connecting to {addr}")),
                ConnState::Failed { error, .. } => (p.danger, error.clone()),
                ConnState::Idle => (p.muted, "offline".into()),
            };
            super::dot(ui, dot, 4.0);
            ui.label(cx.small(text));
            if let Some(ms) = cx.state.latency_ms {
                ui.label(cx.small(format!("· {ms} ms")));
            }
            if let Some(via) = cx.state.via {
                ui.label(cx.small(format!("· {}", via.as_str())));
            }
            if let Some(h) = &cx.state.hosting {
                ui.label(cx.small(format!("· hosting as {}", denchat_core::net::short_addr(&h.addr))));
            }
            if let Some(v) = cx.voice {
                let codec = v.tx_codec.map(|c| format!("{c:?}")).unwrap_or_else(|| "—".into());
                let tx = if v.transmitting {
                    RichText::new("· voice TX").color(p.speaking).size(p.small_size)
                } else {
                    cx.small(format!("· voice {codec}"))
                };
                ui.label(tx);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mods = cx.session.mods().active().count();
                if mods > 0 {
                    ui.label(cx.small(format!("{mods} mod{}", if mods == 1 { "" } else { "s" })));
                }
                ui.label(cx.small(format!("{} · {}", cx.shell.theme, cx.shell.layout)));
            });
        });
    }
}
