use denchat_core::session::ConnState;
use egui::accesskit::Role;
use egui::{CornerRadius, Rect, RichText, Sense, Ui, pos2, vec2};

use super::{Panel, PanelCx, ShellCmd};
use crate::icons;
use crate::kit::{self, Face, Tone, W};

/// You: avatar (with your talking level), where you are, and your voice
/// controls (mic, sound, hang up). The gear, or your name while in a call,
/// opens the settings page (devices, mode, sensitivity, volume).
#[derive(Default)]
pub struct Voice {
    ptt_held: bool,
}

impl Panel for Voice {
    fn title(&self) -> String {
        "Voice".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        let supported = denchat_core::voice_supported();
        let v = cx.state.voice.clone();
        if let Some(err) = &v.error {
            ui.label(RichText::new(err).color(p.danger).size(p.small_size));
        }
        let fill = if cx.in_card { p.sunk } else { p.panel };
        let levels = super::talk_levels(ui, cx);
        let mut gear = None;
        let metal = p.material.is_some();
        let meter = metal && supported && v.channel.is_some();
        kit::plate(ui, &p, kit::card(&p, fill, !cx.in_card), kit::Plate::Metal, |ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
            let y = rect.center().y;
            let time = ui.input(|i| i.time);

            // Right to left: settings (only out of voice, where there's room; in
            // voice, click your name), hang up, then mic / sound in one segment.
            let mut right = rect.right();
            let btn = |right: f32, size: f32| Rect::from_min_size(pos2(right - size, y - size / 2.0), vec2(size, size));
            if v.channel.is_none() || !supported {
                let r = btn(right, 30.0);
                gear = Some(kit::icon_button_at(ui, r, "gear", icons::GEAR_SIX, "Voice settings", Tone::Plain, &p));
                right = r.left() - 4.0;
            }
            if supported {
                if v.channel.is_some() {
                    let r = btn(right, 32.0);
                    if kit::icon_button_at(ui, r, "hang", icons::PHONE_X, "Leave voice", Tone::Danger, &p).clicked() {
                        cx.actions.session(|s| s.voice_leave());
                    }
                    right = r.left() - 6.0;
                }
                let seg = Rect::from_min_size(pos2(right - 64.0, y - 17.0), vec2(64.0, 34.0));
                if !metal {
                    ui.painter().rect_filled(seg, CornerRadius::same(p.row_radius.saturating_add(2).min(12)), p.surface);
                }
                let (muted, deafened) = (v.muted, v.deafened);
                // Material themes: lever toggles, as tall as the segment.
                let (mic, snd) = if metal {
                    let mic = Rect::from_min_size(seg.min, vec2(30.0, 34.0));
                    (mic, Rect::from_min_size(pos2(mic.right() + 4.0, seg.top()), vec2(30.0, 34.0)))
                } else {
                    let mic = Rect::from_min_size(seg.min + vec2(2.0, 2.0), vec2(30.0, 30.0));
                    (mic, Rect::from_min_size(pos2(mic.right(), mic.top()), vec2(30.0, 30.0)))
                };
                let (glyph, tip) = if muted { (icons::MICROPHONE_SLASH, "Unmute") } else { (icons::MICROPHONE, "Mute") };
                if kit::toggle_at(ui, mic, "mic", glyph, tip, !muted, &p).clicked() {
                    cx.actions.session(move |s| s.set_muted(!muted));
                }
                let (glyph, tip) = if deafened { (icons::EAR_SLASH, "Undeafen") } else { (icons::HEADPHONES, "Deafen") };
                if kit::toggle_at(ui, snd, "deaf", glyph, tip, !deafened, &p).clicked() {
                    cx.actions.session(move |s| s.set_deafened(!deafened));
                }
                right = seg.left() - 8.0;
            }

            // Left: you. In voice this is also the way into voice settings.
            let me_rect = Rect::from_min_max(pos2(rect.left() - 4.0, rect.top() - 2.0), pos2(right + 4.0, rect.bottom() + 2.0));
            if gear.is_none() {
                let resp = ui.interact(me_rect, ui.scope_id().with("me"), Sense::click());
                let hot = kit::hover_fade(ui, resp.id, resp.hovered());
                if hot > 0.0 {
                    ui.painter().rect_filled(me_rect, CornerRadius::same(p.row_radius), crate::theme::alpha(p.text, 0.05 * hot));
                }
                gear = Some(resp);
            }
            let painter = ui.painter();
            let connected = cx.state.conn.is_connected();
            // On a narrow sidebar the buttons take the row: leave out the
            // avatar and name rather than draw them under the buttons.
            let show_me = right - rect.left() >= 40.0;
            if show_me {
                let face =
                    Face::new(&cx.state.nick, 36.0, fill).presence(if connected { p.ok } else { p.muted }).picture(cx.my_picture(ui));
                let level = cx.state.me.and_then(|me| levels.get(&me).copied());
                face.talking(level).paint(painter, pos2(rect.left() + 18.0, y), &p, time);
            }
            let x = rect.left() + 46.0;
            let w = (right - x).max(10.0);
            if show_me {
                let g = kit::line(ui, &cx.state.nick, kit::font(14.0, W::Semibold), p.text, w);
                kit::put(painter, &g, x, y - 8.0, p.text);
            }
            let (status, color, tip) = match (&v.channel, &cx.state.conn) {
                (Some(ch), ConnState::Connected { addr }) => {
                    let (n, c) = kit::signal_level(cx.state.latency_ms, &p);
                    if show_me {
                        kit::signal(painter, pos2(x, y + 13.0), n, c, p.surface_hover);
                    }
                    let ms = cx.state.latency_ms.map(|m| format!(" · {m} ms")).unwrap_or_default();
                    let via = cx.state.via.map(|v| format!(" · {}", v.as_str())).unwrap_or_default();
                    (ch.clone(), p.muted, format!("Voice connected to {addr}{ms}{via}"))
                }
                (_, ConnState::Connected { .. }) if !supported => ("No voice in this build".into(), p.muted, String::new()),
                (_, ConnState::Connected { .. }) => ("Not in voice".into(), p.muted, String::new()),
                (_, ConnState::Connecting { .. }) => ("Connecting…".into(), p.warn, String::new()),
                (_, ConnState::Failed { error, .. }) => ("Disconnected".into(), p.danger, error.clone()),
                (_, ConnState::Idle) => ("Offline".into(), p.muted, String::new()),
            };
            let indent = if v.channel.is_some() && connected { 16.0 } else { 0.0 };
            let me = format!("{} · {status}", cx.state.nick);
            if show_me {
                let g = kit::line(ui, status, kit::font(12.0, W::Regular), color, w - indent);
                kit::put(ui.painter(), &g, x + indent, y + 9.0, color);
            }
            if v.channel.is_some() && supported {
                if let Some(g) = gear.take() {
                    kit::name(&g, Role::Button, format!("Voice settings ({me})"));
                    let tip = if tip.is_empty() { "Voice settings".to_owned() } else { format!("{tip}\nClick for voice settings") };
                    gear = Some(g.on_hover_text(tip));
                }
            } else {
                let resp = ui.interact(me_rect, ui.scope_id().with("me-tip"), Sense::hover());
                kit::name(&resp, Role::Label, me);
                if !tip.is_empty() {
                    resp.on_hover_text(tip);
                }
            }

            if meter {
                // Your mic on an LED bar, like a channel strip's meter (dark while muted).
                let level = cx.voice.filter(|_| !v.muted).map_or(0.0, |s| s.input_intensity);
                let level = super::smooth(ui, "voice-meter", level);
                ui.add_space(4.0);
                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
                kit::led_meter(ui.painter(), r, level, &p);
                // Not "Mic level": that's the settings page's meter, which can be up at the same time.
                resp.widget_info(|| {
                    let mut info = egui::WidgetInfo::labeled(Role::ProgressIndicator, true, "Mic meter");
                    info.value = Some(level as f64);
                    info
                });
            }

            if supported && v.channel.is_some() && v.mode == "push_to_talk" {
                ui.add_space(6.0);
                let label = if v.ptt { "Talking…" } else { "Hold to talk" };
                let resp = ui.add_sized([ui.available_width(), 30.0], egui::Button::new(label).sense(Sense::drag()));
                let held = resp.is_pointer_button_down_on();
                if held != self.ptt_held {
                    self.ptt_held = held;
                    cx.actions.session(move |s| s.set_ptt(held));
                }
                if let Some(k) = cx.shell.ptt_key.clone() {
                    ui.label(cx.small(format!("or hold {k}")));
                }
            }
        });
        if gear.is_some_and(|g| g.clicked()) {
            cx.actions.shell(ShellCmd::OpenSettings);
        }
    }
}
