use denchat_core::loudness;
use denchat_core::session::DevicesView;
use egui::accesskit::Role;
use egui::{CornerRadius, Event, Key, PointerButton, Rect, RichText, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::{Panel, PanelCx, ShellCmd};
use crate::pictures::PictureFor;

use crate::icons;
use crate::kit::{self, Face, W};
use crate::theme::{Palette, alpha, mix};

/// The settings page: microphone and headset, how you're sent (voice
/// activity with a sensitivity marker on a live meter, push to talk with a
/// rebindable key, or always on), volumes, and the look.
///
/// The shell shows it over everything (gear in `core.voice`, *Settings* in
/// the room menu, `[keys] settings`, or `den api ui.settings`). While it's
/// open outside a call it runs a mic test, so the meter and your avatar's
/// talking ring move as you speak.
#[derive(Default)]
pub struct Settings {
    page: Page,
    devices: Option<Result<DevicesView, String>>,
    /// Waiting for a key (or mouse button) to bind to push to talk.
    capturing: bool,
    /// Frame we last drew on, to notice the page being (re)opened.
    last_frame: u64,
    was_in_call: bool,
    /// The nickname being edited on the profile page.
    nick: Option<String>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Page {
    #[default]
    Voice,
    Profile,
    Appearance,
}

const MODES: &[(&str, &str, &str)] = &[
    ("voice_activity", "Voice activity", "Sent while you speak louder than the sensitivity marker"),
    ("push_to_talk", "Push to talk", "Sent only while you hold a key"),
    ("open", "Always on", "Always sent, unless you mute"),
];

impl Panel for Settings {
    fn title(&self) -> String {
        "Settings".into()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        self.lifecycle(ui, cx);

        // Title bar.
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
        let g = kit::line(ui, "Settings", kit::font(20.0, W::Semibold), p.text, bar.width() - 50.0);
        kit::put(ui.painter(), &g, bar.left() + 2.0, bar.center().y, p.text);
        let close = Rect::from_center_size(pos2(bar.right() - 16.0, bar.center().y), vec2(32.0, 32.0));
        if kit::icon_button_at(ui, close, "close", icons::X, "Close (Esc)", kit::Tone::Plain, &p).clicked() {
            cx.actions.shell(ShellCmd::CloseSettings);
        }
        ui.add_space(8.0);

        let body_h = ui.available_height().max(120.0);
        ui.horizontal_top(|ui| {
            // Navigation.
            ui.vertical(|ui| {
                ui.set_width(150.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                for (page, glyph, label) in [
                    (Page::Voice, icons::MICROPHONE, "Voice & audio"),
                    (Page::Profile, icons::USER_CIRCLE, "Profile"),
                    (Page::Appearance, icons::PAINT_BRUSH, "Appearance"),
                ] {
                    let (rect, resp) = kit::row(ui, 34.0, self.page == page, false, &p);
                    kit::name_selected(&resp, Role::Tab, self.page == page, label);
                    let color = if self.page == page { p.text } else { p.muted };
                    kit::icon(ui.painter(), pos2(rect.left() + 16.0, rect.center().y), glyph, 16.0, color);
                    let g = kit::line(ui, label, kit::font(13.5, W::Medium), color, rect.width() - 40.0);
                    kit::put(ui.painter(), &g, rect.left() + 32.0, rect.center().y, color);
                    if resp.clicked() {
                        self.page = page;
                        self.capturing = false;
                    }
                }
            });
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ScrollArea::vertical()
                    .id_salt("settings-page")
                    .auto_shrink([false, false])
                    .min_scrolled_height(body_h)
                    .max_height(body_h)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width() - 8.0);
                        match self.page {
                            Page::Voice => self.voice_page(ui, cx),
                            Page::Profile => self.profile_page(ui, cx),
                            Page::Appearance => appearance_page(ui, cx),
                        }
                        ui.add_space(8.0);
                    });
            });
        });
    }
}

impl Settings {
    /// Start a mic test when the page opens (or a call ends while it's open);
    /// the shell stops it when the page closes.
    fn lifecycle(&mut self, ui: &Ui, cx: &mut PanelCx<'_>) {
        let frame = ui.ctx().cumulative_frame_nr();
        let opened = frame > self.last_frame + 1;
        self.last_frame = frame;
        if opened {
            self.devices = None;
            self.capturing = false;
            self.nick = None;
        }
        let v = &cx.state.voice;
        let in_call = v.channel.is_some() || v.engine;
        let call_ended = self.was_in_call && !in_call;
        self.was_in_call = in_call;
        if denchat_core::voice_supported() && !in_call && !v.testing && (opened || call_ended) {
            cx.actions.session(|s| s.voice_test(true));
        }
    }

    fn voice_page(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        if !denchat_core::voice_supported() {
            ui.label(cx.muted("This build of Den has no voice support."));
            return;
        }
        if !cx.session.config().modules.voice {
            ui.label(cx.muted("Voice is switched off in config.toml ([modules] voice = false)."));
            return;
        }
        let cfg = cx.session.config().voice;

        // ----- microphone ------------------------------------------------
        section(ui, &p, icons::MICROPHONE, "Microphone");
        group(ui, &p, |ui| {
            let devices = self.devices.get_or_insert_with(|| cx.session.voice_devices().map_err(|e| format!("{e:#}")));
            let (inputs, default_in) = match devices {
                Ok(d) => (d.inputs.clone(), d.default_input.clone()),
                Err(_) => (Vec::new(), None),
            };
            let mut refresh = false;
            ui.horizontal(|ui| {
                if let Some(input) = device_pick(ui, "dev-in", &cfg.input_device, &inputs, &default_in, ui.available_width() - 40.0) {
                    let output = cfg.output_device.clone();
                    cx.actions.session(move |s| s.set_voice_devices(input, output));
                }
                refresh = kit::icon_button(ui, icons::ARROW_CLOCKWISE, "Look for devices again", kit::Tone::Plain, 28.0, &p).clicked();
            });
            if let Some(Err(e)) = &self.devices {
                ui.label(RichText::new(format!("Couldn't list devices: {e}")).color(p.danger).size(p.small_size));
            }
            if refresh {
                self.devices = None;
            }
            ui.add_space(6.0);
            let mut gain = cfg.input_gain;
            if percent_slider(ui, &p, "Input volume", &mut gain, 0.0..=4.0) {
                cx.actions.session(move |s| s.set_input_gain(gain));
            }
        });

        // ----- mic test / sensitivity --------------------------------------
        section(ui, &p, icons::WAVEFORM, "Mic test");
        group(ui, &p, |ui| self.meter_group(ui, cx, &cfg));

        // ----- input mode --------------------------------------------------
        section(ui, &p, icons::SLIDERS_HORIZONTAL, "Input mode");
        group(ui, &p, |ui| {
            for (key, label, about) in MODES {
                let on = cfg.mode == *key;
                let (rect, resp) = kit::row(ui, 44.0, on, false, &p);
                kit::name_selected(&resp, Role::RadioButton, on, *label);
                let c = rect.left_center() + vec2(16.0, 0.0);
                ui.painter().circle_stroke(c, 7.0, Stroke::new(1.5, if on { p.accent } else { p.line_strong }));
                if on {
                    ui.painter().circle_filled(c, 4.0, p.accent);
                }
                let g = kit::line(ui, *label, kit::font(14.0, W::Semibold), p.text, rect.width() - 50.0);
                kit::put(ui.painter(), &g, rect.left() + 34.0, rect.center().y - 8.0, p.text);
                let g = kit::line(ui, *about, kit::font(12.0, W::Regular), p.muted, rect.width() - 50.0);
                kit::put(ui.painter(), &g, rect.left() + 34.0, rect.center().y + 9.0, p.muted);
                if resp.clicked() && !on {
                    let k = key.to_string();
                    cx.actions.session(move |s| s.set_voice_mode(&k));
                }
            }
            if cfg.mode == "push_to_talk" {
                ui.add_space(8.0);
                self.ptt_key(ui, cx, &cfg);
            }
        });

        // ----- output ------------------------------------------------------
        section(ui, &p, icons::HEADPHONES, "Headset / speakers");
        group(ui, &p, |ui| {
            let (outputs, default_out) = match &self.devices {
                Some(Ok(d)) => (d.outputs.clone(), d.default_output.clone()),
                _ => (Vec::new(), None),
            };
            if let Some(output) = device_pick(ui, "dev-out", &cfg.output_device, &outputs, &default_out, ui.available_width() - 40.0) {
                let input = cfg.input_device.clone();
                cx.actions.session(move |s| s.set_voice_devices(input, output));
            }
            ui.add_space(6.0);
            let mut vol = cfg.output_volume;
            if percent_slider(ui, &p, "Output volume", &mut vol, 0.0..=2.0) {
                cx.actions.session(move |s| s.set_output_volume(vol));
            }
        });
    }

    /// Your avatar with its talking ring, the live meter with the
    /// sensitivity marker, and "hear myself".
    fn meter_group(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>, cfg: &denchat_core::config::VoiceConfig) {
        let p = *cx.palette;
        let v = &cx.state.voice;
        let stats = cx.voice;
        let in_call = v.channel.is_some();
        let level = stats.map_or(0.0, |s| s.input_level);
        let open = stats.is_some_and(|s| s.transmitting);
        let vad = cfg.mode == "voice_activity";

        ui.horizontal(|ui| {
            let face = 52.0;
            let (r, _) = ui.allocate_exact_size(vec2(face + Face::reach(face) * 2.0, face + Face::reach(face) * 2.0), Sense::hover());
            let talking = open.then(|| super::smooth(ui, "settings-talk", loudness::intensity(level)));
            let fill = mix(p.panel, p.surface, 0.5);
            Face::new(&cx.state.nick, face, fill).talking(talking).paint(ui.painter(), r.center(), &p, ui.input(|i| i.time));
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.add_space(14.0);
                let threshold = vad.then_some(cfg.vad_threshold);
                if let Some(t) = meter(ui, &p, level, open, threshold) {
                    cx.actions.session(move |s| s.set_vad_threshold(t));
                }
                ui.add_space(6.0);
                let hint = match cfg.mode.as_str() {
                    "voice_activity" => "Drag the marker: you're sent while the bar is past it.".to_owned(),
                    "push_to_talk" => {
                        format!("You're sent while you hold {}.", cx.shell.ptt_key.clone().unwrap_or_else(|| "the push-to-talk key".into()))
                    }
                    _ => "You're always sent while unmuted.".to_owned(),
                };
                ui.label(cx.small(hint));
            });
        });

        // What the meter is listening to.
        let (status, color) = if in_call {
            if v.muted {
                ("In a call, muted: your mic is metered but not sent".to_owned(), p.warn)
            } else {
                ("In a call: live".to_owned(), p.ok)
            }
        } else if v.testing && stats.is_some() {
            let dev = stats.and_then(|s| s.input_device.clone()).unwrap_or_else(|| "no microphone".into());
            (format!("Testing {dev}"), p.ok)
        } else if v.testing {
            ("Opening your microphone…".to_owned(), p.muted)
        } else {
            ("Mic test is off".to_owned(), p.muted)
        };
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            super::dot(ui, color, 3.5);
            ui.label(RichText::new(status).size(p.small_size).color(p.text2));
            if !in_call && !v.testing && ui.small_button("Start test").clicked() {
                cx.actions.session(|s| s.voice_test(true));
            }
        });
        if let Some(err) = stats.and_then(|s| s.error.clone()).or_else(|| v.error.clone()) {
            ui.label(RichText::new(err).color(p.warn).size(p.small_size));
        }
        ui.add_space(4.0);
        let mut hear = v.loopback;
        let resp = ui.add_enabled(!in_call, egui::Checkbox::new(&mut hear, "Hear myself"));
        let resp = if in_call {
            resp.on_disabled_hover_text("Leave the call to hear yourself")
        } else {
            resp.on_hover_text("Plays back what others would hear, through your headset. Use headphones, or it will echo.")
        };
        if resp.changed() {
            cx.actions.session(move |s| s.set_voice_loopback(hear));
        }
    }

    /// The push-to-talk binding: click, then press a key or a side mouse button.
    fn ptt_key(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>, cfg: &denchat_core::config::VoiceConfig) {
        let p = *cx.palette;
        if self.capturing {
            let got = ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, Key::Escape) {
                    return Some(None);
                }
                for e in &i.events {
                    if let Event::Key { key, pressed: true, .. } = e {
                        return Some(Some(key.name().to_owned()));
                    }
                }
                if i.pointer.button_pressed(PointerButton::Extra1) {
                    return Some(Some("Mouse4".to_owned()));
                }
                if i.pointer.button_pressed(PointerButton::Extra2) {
                    return Some(Some("Mouse5".to_owned()));
                }
                None
            });
            if let Some(key) = got {
                self.capturing = false;
                if let Some(k) = key {
                    cx.actions.session(move |s| s.set_ptt_key(Some(k)));
                }
            }
        }
        ui.horizontal(|ui| {
            kit::icon(ui.painter(), ui.cursor().min + vec2(10.0, 14.0), icons::KEYBOARD, 18.0, p.muted);
            ui.add_space(26.0);
            ui.label(RichText::new("Push-to-talk key").font(kit::font(13.5, W::Medium)).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if cfg.ptt_key.is_some() && !self.capturing {
                    let tip = match &cx.shell.layout_ptt_key {
                        Some(k) => format!("Back to the layout's key ({k})"),
                        None => "Back to the layout's key (none)".to_owned(),
                    };
                    if kit::icon_button(ui, icons::ARROW_COUNTER_CLOCKWISE, &tip, kit::Tone::Plain, 28.0, &p).clicked() {
                        cx.actions.session(|s| s.set_ptt_key(None));
                    }
                }
                let label =
                    if self.capturing { "Press a key…".to_owned() } else { cx.shell.ptt_key.clone().unwrap_or_else(|| "Not set".into()) };
                if keycap(ui, &p, &label, self.capturing)
                    .on_hover_text("Click, then press a key or a side mouse button (Esc cancels)")
                    .clicked()
                {
                    self.capturing = !self.capturing;
                }
            });
        });
        ui.label(cx.small("Works while the Den window is focused."));
    }
}

impl Settings {
    /// Your picture and nickname: what everyone in your rooms sees.
    fn profile_page(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        let p = *cx.palette;
        section(ui, &p, icons::USER_CIRCLE, "Profile");
        group(ui, &p, |ui| {
            ui.horizontal(|ui| {
                let size = 88.0;
                let fill = mix(p.panel, p.surface, 0.5);
                let pic = cx.my_picture(ui);
                let (r, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
                Face::new(&cx.state.nick, size, fill).picture(pic).paint(ui.painter(), r.center(), &p, 0.0);
                picture_hover(ui, &resp, r, size / 2.0);
                kit::name(&resp, Role::Button, "Your picture");
                if resp.on_hover_text("Change your picture").clicked() {
                    cx.actions.shell(ShellCmd::PickPicture(PictureFor::Me));
                }
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    let label = ui.label(RichText::new("Nickname").font(kit::font(13.0, W::Medium)).color(p.muted));
                    let nick = self.nick.get_or_insert_with(|| cx.state.nick.clone());
                    let edit = ui.add(egui::TextEdit::singleline(nick).desired_width(220.0)).labelled_by(label.id);
                    if edit.lost_focus() && nick.trim() != cx.state.nick && !nick.trim().is_empty() {
                        let n = nick.trim().to_owned();
                        cx.actions.session(move |s| s.set_nick(&n));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(format!("{}  Change picture…", icons::IMAGE)).clicked() {
                            cx.actions.shell(ShellCmd::PickPicture(PictureFor::Me));
                        }
                        if cx.state.picture.is_some() && ui.button(format!("{}  Remove picture", icons::TRASH)).clicked() {
                            cx.actions.session(|s| s.set_picture_file(None));
                        }
                    });
                });
            });
            ui.add_space(6.0);
            ui.label(
                cx.small("Everyone in your rooms sees your picture and nickname. Any image works: it's cut to a square and made small."),
            );
        });
    }
}

/// Darken a picture you can click and show a camera, while hovered.
pub(crate) fn picture_hover(ui: &Ui, resp: &egui::Response, r: Rect, radius: f32) {
    let hover = kit::hover_fade(ui, resp.id, resp.hovered());
    if hover > 0.0 {
        let corner = CornerRadius::same(radius.round().clamp(0.0, 255.0) as u8);
        ui.painter().rect_filled(r, corner, alpha(egui::Color32::BLACK, 0.45 * hover));
        kit::icon(ui.painter(), r.center(), icons::CAMERA, (r.height() * 0.3).clamp(14.0, 26.0), alpha(egui::Color32::WHITE, hover));
    }
}

fn appearance_page(ui: &mut Ui, cx: &mut PanelCx<'_>) {
    let p = *cx.palette;
    section(ui, &p, icons::PALETTE, "Theme");
    group(ui, &p, |ui| {
        for (name, origin) in &cx.shell.themes {
            if ui.radio(cx.shell.theme == *name, name).on_hover_text(origin).clicked() {
                cx.actions.shell(ShellCmd::SetTheme(name.clone()));
            }
        }
    });
    section(ui, &p, icons::LAYOUT, "Layout");
    group(ui, &p, |ui| {
        for (name, origin) in &cx.shell.layouts {
            if ui.radio(cx.shell.layout == *name, name).on_hover_text(origin).clicked() {
                cx.actions.shell(ShellCmd::SetLayout(name.clone()));
            }
        }
    });
    ui.add_space(8.0);
    if ui.button(format!("{}  Open config folder", icons::FOLDER_OPEN)).on_hover_text("Edit themes, layouts and mods as files").clicked() {
        cx.actions.shell(ShellCmd::OpenConfigDir);
    }
}

// ----- pieces ----------------------------------------------------------------

fn section(ui: &mut Ui, p: &Palette, glyph: &str, text: &str) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(glyph).size(15.0).color(p.muted));
        ui.label(RichText::new(text).font(kit::font(14.0, W::Semibold)).color(p.text));
    });
    ui.add_space(2.0);
}

fn group(ui: &mut Ui, p: &Palette, body: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(mix(p.panel, p.surface, 0.5))
        .corner_radius(CornerRadius::same(p.card_radius.min(12)))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        });
}

/// A device dropdown: "Default (name)" first. Returns the new choice.
fn device_pick(
    ui: &mut Ui,
    id: &str,
    current: &Option<String>,
    options: &[String],
    default: &Option<String>,
    width: f32,
) -> Option<Option<String>> {
    let shown = current.clone().unwrap_or_else(|| format!("Default{}", default.as_ref().map(|d| format!(" ({d})")).unwrap_or_default()));
    // Built-in test devices (`test:…`, `denchat_audio::testdev`) are never in the OS list.
    let missing = current.as_ref().is_some_and(|c| !options.is_empty() && !options.contains(c) && !c.starts_with("test:"));
    let shown = if missing { format!("{shown} (not connected)") } else { shown };
    let mut out = None;
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(width.max(120.0)).show_ui(ui, |ui| {
        let label = format!("Default{}", default.as_ref().map(|d| format!(" ({d})")).unwrap_or_default());
        if ui.selectable_label(current.is_none(), label).clicked() {
            out = Some(None);
        }
        for o in options {
            if ui.selectable_label(current.as_deref() == Some(o), o).clicked() {
                out = Some(Some(o.clone()));
            }
        }
    });
    out
}

/// A labelled gain slider shown in percent; true when it changed.
fn percent_slider(ui: &mut Ui, p: &Palette, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).font(kit::font(13.5, W::Medium)).color(p.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let reset = (*value - 1.0).abs() > 0.005 && ui.small_button("100%").on_hover_text("Reset").clicked();
            if reset {
                *value = 1.0;
            }
            ui.spacing_mut().slider_width = (ui.available_width() - 60.0).max(80.0);
            let r = ui.add(
                egui::Slider::new(value, range)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .custom_parser(|s| s.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)),
            );
            reset || r.changed()
        })
        .inner
    })
    .inner
}

/// A key cap button (`F8`), highlighted while waiting for a key.
fn keycap(ui: &mut Ui, p: &Palette, text: &str, active: bool) -> egui::Response {
    let fg = if active { p.accent_text } else { p.text };
    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_owned(), kit::font(13.0, W::Semibold), fg));
    let (rect, resp) = ui.allocate_exact_size(vec2((g.size().x + 24.0).max(64.0), 30.0), Sense::click());
    kit::name(&resp, Role::Button, format!("Push-to-talk key: {text}"));
    let hot = kit::hover_fade(ui, resp.id, resp.hovered());
    let radius = CornerRadius::same(6);
    let bg = if active { p.accent } else { mix(p.surface, p.surface_hover, hot) };
    // A darker lip under the cap.
    ui.painter().rect_filled(rect.translate(vec2(0.0, 2.0)), radius, if active { mix(p.accent, p.text, 0.25) } else { p.line_strong });
    ui.painter().rect_filled(rect, radius, bg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A segmented level meter on a dB scale. With `threshold` (voice
/// activity), the zone past it is tinted and a draggable marker sets it;
/// returns the new threshold while dragged or clicked.
fn meter(ui: &mut Ui, p: &Palette, level: f32, open: bool, threshold: Option<f32>) -> Option<f32> {
    const SEGMENTS: usize = 36;
    let w = ui.available_width().max(120.0);
    let sense = if threshold.is_some() { Sense::click_and_drag() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), sense);
    let label = match threshold {
        Some(t) => format!("Mic level, sensitivity {:.0} dB", loudness::db(t)),
        None => "Mic level".to_owned(),
    };
    kit::name(&resp, if threshold.is_some() { Role::Slider } else { Role::ProgressIndicator }, label);
    let painter = ui.painter();
    let x_at = |pos: f32| rect.left() + pos.clamp(0.0, 1.0) * rect.width();
    painter.rect_filled(rect, CornerRadius::same(6), alpha(p.text, 0.05));
    let marker = threshold.map(loudness::meter);
    if let Some(m) = marker {
        let zone = Rect::from_min_max(pos2(x_at(m), rect.top()), rect.max);
        painter.rect_filled(zone, CornerRadius { nw: 0, sw: 0, ne: 6, se: 6 }, alpha(p.ok, 0.10));
    }

    let lit = super::smooth(ui, "settings-meter", loudness::meter(level));
    let inner = rect.shrink2(vec2(5.0, 6.0));
    let gap = 2.0;
    let seg_w = (inner.width() - gap * (SEGMENTS - 1) as f32) / SEGMENTS as f32;
    for i in 0..SEGMENTS {
        let x = inner.left() + i as f32 * (seg_w + gap);
        let seg = Rect::from_min_size(pos2(x, inner.top()), vec2(seg_w, inner.height()));
        let pos = (i as f32 + 0.5) / SEGMENTS as f32;
        let color = if pos <= lit {
            let past = marker.is_none_or(|m| pos >= m);
            if open && past { p.speaking } else { mix(p.muted, p.text, 0.15) }
        } else {
            alpha(p.text, 0.08)
        };
        painter.rect_filled(seg, CornerRadius::same(2), color);
    }

    let mut out = None;
    if let Some(m) = marker {
        let x = x_at(m);
        let hot = resp.hovered() || resp.dragged();
        let stroke = Stroke::new(if hot { 3.0 } else { 2.0 }, p.text);
        painter.line_segment([pos2(x, rect.top() - 3.0), pos2(x, rect.bottom() + 3.0)], stroke);
        painter.circle_filled(pos2(x, rect.top() - 3.0), if hot { 5.0 } else { 4.0 }, p.text);
        if (resp.dragged() || resp.clicked())
            && let Some(at) = resp.interact_pointer_pos()
        {
            let pos = ((at.x - rect.left()) / rect.width()).clamp(0.02, 0.98);
            out = Some(loudness::meter_to_rms(pos));
        }
        let db = loudness::db(threshold.unwrap_or_default());
        resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text(format!("Sensitivity: {db:.0} dB (drag)"));
    }
    out
}
