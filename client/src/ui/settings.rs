//! Settings, in sections down a rail as OpenController lays them out: your profile, voice and
//! sound, camera and background, appearance and language, sounds, advanced. Changes apply as they
//! are made, with no Save button.

mod profile;

use crate::core::settings::{Background, CustomTheme, HotkeyAction};
use crate::hotkeys::Combo;
use crate::media::audio::{self, DeviceInfo, MicGuard};
use crate::media::camera;
use crate::media::diagnose;
use crate::media::video::FrameSlot;
use crate::media::voice::{Voice, apply_mic_levels, apply_mic_prefs, cue_volume, gate_threshold};
use crate::prefs::{prefs, set_prefs};
use crate::session::Session;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{self, CustomColors, Theme, current, hex, px, radius, to_hex};
use crate::ui::camera::Preview;
use crate::ui::hotkeys;
use crate::ui::overlay::{self, Dismiss, dialog_card};
use crate::ui::server::ServerView;
use crate::ui::server::sidebar::{Menu, MenuEntry};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, WindowAppearance,
    div,
};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Profile,
    Voice,
    Camera,
    Appearance,
    Sounds,
    Hotkeys,
    Advanced,
}

pub struct Settings {
    focus: FocusHandle,
    section: Section,
    inputs: Vec<DeviceInfo>,
    outputs: Vec<DeviceInfo>,
    cameras: Vec<camera::Device>,
    level: f32,
    gate_open: bool,
    _mic: Option<MicGuard>,
    /// The camera while the Camera section shows.
    preview: Preview,
    voice: Option<Entity<Voice>>,
    session: Entity<Session>,
    server: WeakEntity<ServerView>,
    profile: profile::Profile,
    custom: [Entity<TextField>; 4],
    appearance: WindowAppearance,
    test_steps: Vec<diagnose::Step>,
    test_outcome: Option<diagnose::Outcome>,
    testing: bool,
    _meter: Task<()>,
}

impl EventEmitter<Dismiss> for Settings {}

impl Focusable for Settings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Opens on your profile. `server` is the view it was opened from, which does the account's work.
pub fn open(voice: Option<Entity<Voice>>, session: Entity<Session>, server: WeakEntity<ServerView>, window: &mut Window, cx: &mut App) {
    let appearance = window.appearance();
    let view = cx.new(|cx| Settings::new(voice, session, server, appearance, cx));
    overlay::open_dialog(view, window, cx);
}

impl Settings {
    fn new(
        voice: Option<Entity<Voice>>,
        session: Entity<Session>,
        server: WeakEntity<ServerView>,
        appearance: WindowAppearance,
        cx: &mut Context<Self>,
    ) -> Settings {
        let c = prefs(cx).custom_theme.clone().unwrap_or(CustomTheme {
            bg: "#0f1116".into(),
            surface: "#171a21".into(),
            text: "#e6e8ee".into(),
            accent: "#5b8cff".into(),
        });
        let field = |value: String, cx: &mut Context<Self>| {
            let f = cx.new(|cx| {
                let mut f = TextField::new(cx, false, 7);
                f.set_text(&value, cx);
                f
            });
            cx.subscribe(&f, |this: &mut Settings, _, ev: &TextFieldEvent, cx| {
                let _ = ev;
                this.apply_custom(cx);
            })
            .detach();
            f
        };
        let custom = [field(c.bg, cx), field(c.surface, cx), field(c.text, cx), field(c.accent, cx)];
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let profile = profile::Profile::new(&session, cx);
        let meter = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(60)).await;
                let ok = this.update(cx, |s, cx| {
                    if s.section == Section::Voice {
                        let a = audio::audio();
                        let l = a.mic.level.take();
                        s.level = l.max(s.level * 0.8);
                        s.gate_open = a.mic.open.load(std::sync::atomic::Ordering::Relaxed);
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        });
        let mut s = Settings {
            focus: cx.focus_handle(),
            section: Section::Voice,
            inputs: Vec::new(),
            outputs: Vec::new(),
            cameras: Vec::new(),
            level: 0.,
            gate_open: false,
            _mic: None,
            preview: Preview::default(),
            voice,
            session,
            server,
            profile,
            custom,
            appearance,
            test_steps: Vec::new(),
            test_outcome: None,
            testing: false,
            _meter: meter,
        };
        s.enter(Section::Profile, cx);
        s.load_devices(cx);
        s
    }

    fn load_devices(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (i, o, c) =
                cx.background_executor().spawn(async move { (audio::input_devices(), audio::output_devices(), camera::devices()) }).await;
            let _ = this.update(cx, |s, cx| {
                s.inputs = i;
                s.outputs = o;
                s.cameras = c;
                cx.notify();
            });
        })
        .detach();
    }

    fn enter(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        // The microphone only while its meter shows; the camera only while its preview does.
        self._mic = (section == Section::Voice).then(|| audio::audio().acquire_mic());
        if section == Section::Camera {
            self.start_preview(cx);
        } else {
            self.stop_preview(cx);
        }
        cx.notify();
    }

    fn start_preview(&mut self, cx: &mut Context<Self>) {
        let (device, bg) = (prefs(cx).voice_camera_id.clone(), prefs(cx).camera_background.clone());
        self.preview.start(self.voice.as_ref(), &device, bg, cx);
    }

    fn stop_preview(&mut self, cx: &mut Context<Self>) {
        self.preview.stop(cx);
    }

    fn set_background(&mut self, bg: Background, cx: &mut Context<Self>) {
        self.preview.set_background(bg.clone());
        if let Some(v) = &self.voice {
            let b = bg.clone();
            v.update(cx, |v, _| v.set_camera_background(b));
        }
        set_prefs(cx, |p| p.camera_background = bg);
        cx.notify();
    }

    fn set_camera(&mut self, id: String, cx: &mut Context<Self>) {
        set_prefs(cx, |p| p.voice_camera_id = id);
        self.stop_preview(cx);
        self.start_preview(cx);
        // A camera already in the call moves over too.
        if let Some(v) = self.voice.clone()
            && v.read(cx).camera_on()
        {
            let (device, bg) = (prefs(cx).voice_camera_id.clone(), prefs(cx).camera_background.clone());
            v.update(cx, |v, cx| {
                v.stop_camera(cx);
                v.start_camera(&device, bg, cx);
            });
        }
    }

    fn run_test(&mut self, cx: &mut Context<Self>) {
        let api = self.session.read(cx).api.clone();
        if self.testing {
            return;
        }
        self.testing = true;
        self.test_steps.clear();
        self.test_outcome = None;
        let (tx, rx) = async_channel::unbounded::<diagnose::Step>();
        let task = crate::core::run(async move {
            diagnose::run(api, move |s| {
                let _ = tx.send_blocking(s);
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let steps = async {
                while let Ok(step) = rx.recv().await {
                    let _ = this.update(cx, |s, cx| {
                        match s.test_steps.iter_mut().find(|x| x.name == step.name) {
                            Some(x) => *x = step,
                            None => s.test_steps.push(step),
                        }
                        cx.notify();
                    });
                }
            };
            let (outcome, _) = futures::join!(task, steps);
            let _ = this.update(cx, |s, cx| {
                s.testing = false;
                s.test_outcome = Some(outcome);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_custom(&mut self, cx: &mut Context<Self>) {
        let v: Vec<String> = self.custom.iter().map(|f| f.read(cx).text()).collect();
        if v.iter().all(|x| hex(x).is_some()) {
            let ct = CustomTheme { bg: v[0].clone(), surface: v[1].clone(), text: v[2].clone(), accent: v[3].clone() };
            set_prefs(cx, |p| {
                p.theme = "custom".into();
                p.custom_theme = Some(ct);
            });
        }
    }

    fn pick_palette(&mut self, name: &str, cx: &mut Context<Self>) {
        if name == "custom" && prefs(cx).custom_theme.is_none() {
            // Custom starts from the palette in use.
            let t = current();
            let to = |c: gpui::Hsla| {
                let r = c.to_rgb();
                let b = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
                to_hex(b(r.r) << 16 | b(r.g) << 8 | b(r.b))
            };
            let vals = [to(t.base), to(t.pane), to(t.text), to(t.accent)];
            for (f, v) in self.custom.iter().zip(vals.iter()) {
                f.update(cx, |f, cx| f.set_text(v, cx));
            }
            self.apply_custom(cx);
            return;
        }
        let n = name.to_string();
        set_prefs(cx, |p| p.theme = n);
        cx.notify();
    }

    // Sections.

    fn voice_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs(cx).clone();
        let input_name = self
            .inputs
            .iter()
            .find(|d| d.id == p.voice_input_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| tr!("Windows default", "Padrão do Windows").into());
        let output_name = self
            .outputs
            .iter()
            .find(|d| d.id == p.voice_output_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| tr!("Windows default", "Padrão do Windows").into());
        let mut ins = vec![(String::new(), tr!("Windows default", "Padrão do Windows").to_string())];
        ins.extend(self.inputs.iter().map(|d| (d.id.clone(), d.name.clone())));
        let mut outs = vec![(String::new(), tr!("Windows default", "Padrão do Windows").to_string())];
        outs.extend(self.outputs.iter().map(|d| (d.id.clone(), d.name.clone())));
        let gate = gate_threshold(p.mic_sensitivity);
        // The meter's scale, the old client's: sqrt(rms / 0.25).
        let pos = |rms: f32| (rms / 0.25).sqrt().clamp(0., 1.);
        let level = pos(self.level);
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(row_field(
                tr!("Microphone", "Microfone"),
                select("mic", input_name, ins, t, cx, |_, id, cx| {
                    set_prefs(cx, |p| p.voice_input_id = id);
                    apply_mic_prefs(cx);
                }),
            ))
            .child(row_field(
                tr!("Speakers or headphones", "Alto-falantes ou fones"),
                select("out", output_name, outs, t, cx, |_, id, cx| {
                    set_prefs(cx, |p| p.voice_output_id = id.clone());
                    audio::audio().set_output(&id);
                }),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(label(tr!("Input volume", "Volume de entrada"), t))
                            .child(mono(format!("{}%", p.mic_gain), if p.mic_gain > 100 { t.caution } else { t.text2 })),
                    )
                    .child(slider(
                        "mic-gain",
                        Slider {
                            value: p.mic_gain as f32 / 200.,
                            mark: Some(0.5),
                            color: if p.mic_gain > 100 { t.caution } else { t.accent },
                        },
                        t,
                        cx,
                        |_, v, cx| {
                            set_prefs(cx, |p| p.mic_gain = ((v * 200.) / 5.).round() as u32 * 5);
                            apply_mic_levels(cx);
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().flex().justify_between().child(label(tr!("Input sensitivity", "Sensibilidade de entrada"), t)).child(
                        mono(
                            if p.mic_sensitivity == 0 { tr!("Off", "Desligada").to_string() } else { p.mic_sensitivity.to_string() },
                            t.text2,
                        ),
                    ))
                    .child(slider(
                        "mic-gate",
                        Slider { value: p.mic_sensitivity as f32 / 100., mark: None, color: t.accent },
                        t,
                        cx,
                        |_, v, cx| {
                            set_prefs(cx, |p| p.mic_sensitivity = (v * 100.).round() as u32);
                            apply_mic_levels(cx);
                        },
                    ))
                    // The live meter, with the gate's threshold on it.
                    .child(
                        div()
                            .relative()
                            .h(px(8.))
                            .rounded(px(4.))
                            .bg(t.well)
                            .overflow_hidden()
                            .child(
                                div().absolute().left_0().top_0().h_full().rounded(px(4.)).w(gpui::relative(level)).bg(if self.gate_open {
                                    t.success
                                } else {
                                    t.text3
                                }),
                            )
                            .when(gate > 0., |d| {
                                d.child(div().absolute().top_0().h_full().w(px(2.)).bg(t.caution).left(gpui::relative(pos(gate))))
                            }),
                    )
                    .child(caption(
                        if p.mic_sensitivity == 0 {
                            tr!(
                                "Everything the microphone hears is sent. Raise it to cut out background noise between words.",
                                "Tudo o que o microfone capta é enviado. Aumente para cortar o ruído de fundo entre as palavras."
                            )
                        } else {
                            tr!("Below the amber line, nothing is sent.", "Abaixo da linha âmbar, nada é enviado.")
                        },
                        t.text3,
                    )),
            )
            .child(toggle_row(
                "echo",
                tr!("Echo cancellation", "Cancelamento de eco"),
                tr!("Stops others hearing themselves through your speakers.", "Evita que os outros se ouçam pelos seus alto-falantes."),
                p.echo_cancellation,
                t,
                cx,
                |cx| {
                    set_prefs(cx, |p| p.echo_cancellation = !p.echo_cancellation);
                    apply_mic_prefs(cx);
                },
            ))
            .child(toggle_row(
                "noise",
                tr!("Noise suppression", "Supressão de ruído"),
                tr!("Takes out fans, keyboards and hum.", "Tira ventiladores, teclados e zumbidos."),
                p.noise_suppression,
                t,
                cx,
                |cx| {
                    set_prefs(cx, |p| p.noise_suppression = !p.noise_suppression);
                    apply_mic_prefs(cx);
                },
            ))
            .child(toggle_row(
                "confirm-join",
                tr!("Ask before joining a voice channel", "Perguntar antes de entrar num canal de voz"),
                tr!(
                    "A click on a voice channel asks first, so a stray one doesn't put you on the air.",
                    "Um clique num canal de voz pergunta antes, para um clique sem querer não te colocar no ar."
                ),
                p.confirm_voice_join,
                t,
                cx,
                |cx| set_prefs(cx, |p| p.confirm_voice_join = !p.confirm_voice_join),
            ))
            .into_any_element()
    }

    fn camera_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs(cx).clone();
        let cam_name = self.cameras.iter().find(|d| d.id == p.voice_camera_id).map(|d| d.name.clone()).unwrap_or_else(|| {
            self.cameras.first().map(|c| c.name.clone()).unwrap_or_else(|| tr!("No camera found", "Nenhuma câmera encontrada").into())
        });
        let cams: Vec<(String, String)> = self.cameras.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
        let current_bg = p.camera_background.clone();
        let grid = crate::ui::camera::backgrounds(&current_bg, false, t, cx, |s: &mut Settings, bg, cx| s.set_background(bg, cx));
        let blur = if let Background::Blur { strength } = current_bg { Some(strength) } else { None };
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(
                self.preview.frame(t, cx).h(px(220.)),
            )
            .child(row_field(tr!("Camera", "Câmera"), select("camera", cam_name, cams, t, cx, |s: &mut Settings, id, cx| s.set_camera(id, cx))))
            .child(div().flex().flex_col().gap(px(8.)).child(label(tr!("Background", "Fundo"), t)).child(grid))
            .when_some(blur, |d, strength| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(div().flex().justify_between().child(label(tr!("Blur strength", "Intensidade do desfoque"), t)).child(mono(strength.to_string(), t.text2)))
                        .child(slider("blur", Slider { value: (strength as f32 - 2.) / 28., mark: None, color: t.accent }, t, cx, |s: &mut Settings, v, cx| {
                            let strength = (2. + v * 28.).round() as u32;
                            s.set_background(Background::Blur { strength }, cx);
                        })),
                )
            })
            .child(caption(
                tr!(
                    "Only you see the preview mirrored. Backgrounds are worked out on this computer; nothing leaves it but the finished picture.",
                    "Só você vê a prévia espelhada. Os fundos são feitos neste computador; nada sai dele além da imagem pronta."
                ),
                t.text3,
            ))
            .into_any_element()
    }

    fn appearance_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs(cx).clone();
        let custom = p.custom_theme.as_ref().and_then(CustomColors::from_settings);
        let mut grid = div().flex().flex_wrap().gap(px(10.));
        for (name, title) in theme::palettes() {
            let on = p.theme == name;
            let [bg, surface, accent] = theme::swatches(name, self.appearance, custom.as_ref());
            let hover = t.stroke_strong;
            grid = grid.child(
                div()
                    .id(gpui::ElementId::Name(format!("palette-{name}").into()))
                    .w(px(112.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .cursor_pointer()
                    .child(
                        div()
                            .h(px(54.))
                            .rounded(px(radius::CONTROL))
                            .overflow_hidden()
                            .border_2()
                            .border_color(if on { t.accent } else { t.stroke })
                            .when(!on, |d| d.hover(move |s| s.border_color(hover)))
                            .flex()
                            .child(div().flex_1().bg(bg))
                            .child(div().flex_1().bg(surface))
                            .child(div().w(px(22.)).bg(accent)),
                    )
                    .child(div().text_center().text_size(px(12.5)).text_color(if on { t.text } else { t.text2 }).child(title))
                    .on_click(cx.listener(move |s, _, _, cx| s.pick_palette(name, cx))),
            );
        }
        let scale = p.ui_scale;
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(self.language_row(t, cx))
            .child(div().flex().flex_col().gap(px(8.)).child(label(tr!("Colour palette", "Paleta de cores"), t)).child(grid))
            .when(p.theme == "custom", |d| {
                let names = [tr!("Background", "Fundo"), tr!("Surface", "Superfície"), tr!("Text", "Texto"), tr!("Accent", "Destaque")];
                let mut row = div().flex().gap(px(10.));
                for (i, f) in self.custom.iter().enumerate() {
                    let color = hex(&f.read(cx).text()).map(|c| gpui::rgb(c).into()).unwrap_or(t.well);
                    row = row.child(
                        div().flex_1().flex().flex_col().gap(px(6.)).child(label(names[i], t)).child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(div().flex_none().size(px(28.)).rounded(px(7.)).border_1().border_color(t.stroke_strong).bg(color))
                                .child(f.clone()),
                        ),
                    );
                }
                d.child(row).child(caption(
                    tr!(
                        "Hex colours, like #5b8cff. Everything else is worked out from these four.",
                        "Cores em hexadecimal, como #5b8cff. O resto é calculado a partir destas quatro."
                    ),
                    t.text3,
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(label(tr!("Interface size", "Tamanho da interface"), t))
                            .child(mono(format!("{scale}%"), t.text2)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                icon_button("smaller", "minus", t).tooltip(tip(tr!("Smaller", "Menor"), t)).on_click(
                                    cx.listener(|_, _, _, cx| set_prefs(cx, |p| p.ui_scale = p.ui_scale.saturating_sub(5).max(70))),
                                ),
                            )
                            .child(div().flex_1().child(slider(
                                "scale",
                                Slider { value: (scale as f32 - 70.) / 110., mark: Some(30. / 110.), color: t.accent },
                                t,
                                cx,
                                |_, v, cx| set_prefs(cx, |p| p.ui_scale = ((70. + v * 110.) / 5.).round() as u32 * 5),
                            )))
                            .child(
                                icon_button("bigger", "plus", t)
                                    .tooltip(tip(tr!("Bigger", "Maior"), t))
                                    .on_click(cx.listener(|_, _, _, cx| set_prefs(cx, |p| p.ui_scale = (p.ui_scale + 5).min(180)))),
                            )
                            .child(
                                button("scale-reset", tr!("Reset", "Redefinir"), Kind::Subtle, t)
                                    .on_click(cx.listener(|_, _, _, cx| set_prefs(cx, |p| p.ui_scale = 100))),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn language_row(&mut self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        use crate::i18n;
        let setting = prefs(cx).language.clone();
        let picked = if setting.is_empty() { "" } else { i18n::code(i18n::resolve(&setting)) };
        let choices: Vec<(&'static str, SharedString)> =
            vec![("", tr!("System", "Sistema").into()), ("en", "English".into()), ("pt", "Português".into())];
        let hint = if picked.is_empty() {
            tr!(
                "Portuguese when Windows is in Portuguese, English otherwise.",
                "Português quando o Windows está em português, inglês nos outros casos."
            )
        } else {
            tr!("Changes the whole window right away.", "Muda a janela inteira na hora.")
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label(tr!("Language", "Idioma"), t))
            .child(div().w(px(360.)).child(segmented("language", choices, picked, t, cx, |_, code, _, cx| {
                set_prefs(cx, |p| p.language = code.into());
                i18n::set(i18n::resolve(code));
                // Text is looked up as it is drawn, so drawing every window again is enough.
                cx.refresh_windows();
            })))
            .child(caption(hint, t.text3))
    }

    fn sounds_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs(cx).clone();
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(toggle_row(
                "cues",
                tr!("Voice channel sounds", "Sons do canal de voz"),
                tr!(
                    "Short tones when you connect, when someone joins or leaves, when a stream starts or stops, and when you mute or deafen.",
                    "Toques curtos quando você conecta, quando alguém entra ou sai, quando uma transmissão começa ou para, e quando você silencia o microfone ou o som."
                ),
                p.voice_sounds,
                t,
                cx,
                |cx| set_prefs(cx, |p| p.voice_sounds = !p.voice_sounds),
            ))
            .child(toggle_row(
                "mention",
                tr!("Mention sound", "Som de menção"),
                tr!(
                    "A chime when someone @mentions you in a channel you don't have open.",
                    "Um aviso sonoro quando alguém te @menciona num canal que você não está vendo."
                ),
                p.mention_sound,
                t,
                cx,
                |cx| set_prefs(cx, |p| p.mention_sound = !p.mention_sound),
            ))
            .child(toggle_row(
                "message",
                tr!("New message sound", "Som de nova mensagem"),
                tr!(
                    "A soft tone when someone writes in a channel you aren't looking at. Channels you muted stay quiet.",
                    "Um toque suave quando alguém escreve num canal que você não está vendo. Canais silenciados ficam quietos."
                ),
                p.message_sound,
                t,
                cx,
                |cx| set_prefs(cx, |p| p.message_sound = !p.message_sound),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(label(tr!("Sound effects volume", "Volume dos efeitos sonoros"), t))
                            .child(mono(format!("{}%", p.sound_volume), if p.sound_volume > 100 { t.caution } else { t.text2 })),
                    )
                    .child(slider(
                        "sound-volume",
                        Slider {
                            value: p.sound_volume as f32 / 200.,
                            mark: Some(0.5),
                            color: if p.sound_volume > 100 { t.caution } else { t.accent },
                        },
                        t,
                        cx,
                        |_, v, cx| set_prefs(cx, |p| p.sound_volume = ((v * 200.) / 5.).round() as u32 * 5),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(label(tr!("Soundboard volume", "Volume do painel de sons"), t))
                            .child(mono(format!("{}%", p.soundpad_volume), if p.soundpad_volume > 100 { t.caution } else { t.text2 })),
                    )
                    .child(slider(
                        "soundpad",
                        Slider {
                            value: p.soundpad_volume as f32 / 350.,
                            mark: Some(100. / 350.),
                            color: if p.soundpad_volume > 100 { t.caution } else { t.accent },
                        },
                        t,
                        cx,
                        |_, v, cx| set_prefs(cx, |p| p.soundpad_volume = ((v * 350.) / 5.).round() as u32 * 5),
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        button("test-join", tr!("Play the join sound", "Tocar o som de entrada"), Kind::Standard, t)
                            .on_click(|_, _, cx| audio::cue(audio::Cue::Join, cue_volume(cx))),
                    )
                    .child(
                        button("test-mention", tr!("Play the mention sound", "Tocar o som de menção"), Kind::Standard, t)
                            .on_click(|_, _, cx| audio::cue(audio::Cue::Mention, cue_volume(cx))),
                    ),
            )
            .into_any_element()
    }

    fn hotkeys_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let server = self.session.read(cx).api.base();
        let mut list = div().flex().flex_col().gap(px(10.));
        let rows = [
            (HotkeyAction::Mute, "hotkey-mute", tr!("Mute or unmute", "Silenciar ou reativar o microfone")),
            (HotkeyAction::Deafen, "hotkey-deafen", tr!("Deafen or undeafen", "Desativar ou reativar o som")),
        ];
        for (action, id, what) in rows {
            let combo = Combo::parse(prefs(cx).hotkey(&server, action));
            let problem = combo.and(hotkeys::failure(action, cx));
            let (s1, s2) = (server.clone(), server.clone());
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(radius::CARD))
                    .bg(t.layer)
                    .border_1()
                    .border_color(if problem.is_some() { t.critical.opacity(0.5) } else { t.stroke })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(body(what, t.text))
                            .when_some(problem, |d, f| d.child(caption(f.message(), t.critical))),
                    )
                    .child(match combo {
                        Some(c) => hotkeys::keys(&c.keycaps(), t),
                        None => mono(tr!("Not set", "Sem atalho"), t.text3),
                    })
                    .child(
                        button(id, if combo.is_some() { tr!("Change", "Trocar") } else { tr!("Set", "Definir") }, Kind::Standard, t)
                            .on_click(move |_, window, cx| hotkeys::edit(action, what, s1.clone(), window, cx)),
                    )
                    .when(combo.is_some(), |d| {
                        d.child(
                            button((id, 1usize), tr!("Clear", "Limpar"), Kind::Subtle, t)
                                .on_click(move |_, _, cx| hotkeys::bind(action, &s2, "", cx)),
                        )
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(body(
                tr!(
                    "These work anywhere, even while a game has focus, and only while you are in a call. A combination bound here is taken from every other program while Harmony is open, so use Ctrl or Alt, or an F-key.",
                    "Funcionam em qualquer lugar, até com um jogo em primeiro plano, e só enquanto você está em uma chamada. Uma combinação definida aqui deixa de funcionar nos outros programas enquanto o Harmony estiver aberto, então use Ctrl ou Alt, ou uma tecla F."
                ),
                t.text2,
            ))
            .child(list)
            .child(caption(
                tr!(
                    "To give a soundboard sound a hotkey, right-click it in the soundboard.",
                    "Para dar um atalho a um som do painel, clique nele com o botão direito no painel de sons."
                ),
                t.text3,
            ))
            .into_any_element()
    }

    fn advanced_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = prefs(cx).clone();
        let hw = p.hardware_encoding != "off";
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .when(cfg!(windows), |d| {
                d.child(toggle_row(
                    "close-to-tray",
                    tr!("Close to the tray", "Fechar para a bandeja"),
                    tr!(
                        "Closing the window keeps Harmony running by the clock, calls included. Quit from its icon there.",
                        "Fechar a janela deixa o Harmony rodando ao lado do relógio, inclusive em chamadas. Para sair, use o ícone dele ali."
                    ),
                    p.close_to_tray,
                    t,
                    cx,
                    |cx| set_prefs(cx, |p| p.close_to_tray = !p.close_to_tray),
                ))
            })
            .child(toggle_row(
                "hw",
                tr!("Encode on the graphics card", "Codificar na placa de vídeo"),
                tr!(
                    "Uses the GPU's video encoder for cameras and screens when it has one. Turn it off if shares stutter or fail.",
                    "Usa o codificador de vídeo da placa para câmeras e telas, quando ela tem um. Desligue se as transmissões travarem ou falharem."
                ),
                hw,
                t,
                cx,
                |cx| {
                    set_prefs(cx, |p| p.hardware_encoding = if p.hardware_encoding == "off" { "auto".into() } else { "off".into() });
                    crate::media::rtc::set_hardware_encoding(prefs(cx).hardware_encoding != "off");
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(label(tr!("Picture and file cache", "Cache de imagens e arquivos"), t))
                            .child(mono(format!("{} MB", p.media_cache_mb), t.text2)),
                    )
                    .child(slider(
                        "cache",
                        Slider { value: (p.media_cache_mb as f32 - 64.) / (4096. - 64.), mark: None, color: t.accent },
                        t,
                        cx,
                        |_, v, cx| set_prefs(cx, |p| p.media_cache_mb = (((64. + v * (4096. - 64.)) / 64.).round() as u64) * 64),
                    ))
                    .child(caption(
                        tr!(
                            "Avatars, emoji and attachments are kept on disk so they load once. Takes effect next time you connect.",
                            "Avatares, emojis e anexos ficam guardados no disco para carregar uma vez só. Vale a partir da próxima conexão."
                        ),
                        t.text3,
                    )),
            )
            .child(self.connection_test(t, cx))
            .child(crate::ui::updates::settings_row(t, cx))
            .child(caption(
                trf!(
                    "Harmony {} · settings in {}",
                    "Harmony {} · configurações em {}",
                    env!("CARGO_PKG_VERSION"),
                    crate::core::settings::data_dir().display()
                ),
                t.text3,
            ))
            .into_any_element()
    }

    fn connection_test(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut steps = div().flex().flex_col().gap(px(6.));
        for s in &self.test_steps {
            let (glyph, color) = match s.ok {
                Some(true) => ("check", t.success),
                Some(false) => ("close", t.critical),
                None => ("clock", t.text3),
            };
            steps = steps.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(icon(glyph, 15., color))
                    .child(div().flex_1().child(body(s.name, t.text)))
                    .child(div().max_w(px(300.)).truncate().child(mono(s.detail.clone(), t.text3))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(14.))
            .rounded(px(radius::CARD))
            .bg(t.layer)
            .border_1()
            .border_color(t.stroke)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(body(tr!("Test the connection", "Testar a conexão"), t.text))
                            .child(caption(
                                tr!(
                                    "Walks the path a share takes with a tiny test stream, and says where it breaks if it does.",
                                    "Manda um pequeno vídeo de teste pelo mesmo caminho de uma transmissão e mostra onde falha, se falhar."
                                ),
                                t.text2,
                            )),
                    )
                    .child(
                        button(
                            "run-test",
                            if self.testing { tr!("Testing…", "Testando…") } else { tr!("Run the test", "Fazer o teste") },
                            Kind::Standard,
                            t,
                        )
                        .when(self.testing, |b| b.opacity(0.5))
                        .on_click(cx.listener(|s, _, _, cx| s.run_test(cx))),
                    ),
            )
            .when(!self.test_steps.is_empty(), |d| d.child(steps))
            .when_some(self.test_outcome.clone(), |d, o| {
                let color = if o.ok { t.success } else { t.caution };
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(10.))
                        .rounded(px(radius::CONTROL))
                        .bg(t.tint(color))
                        .border_1()
                        .border_color(color.opacity(0.4))
                        .child(body(o.verdict, t.text)),
                )
            })
            .into_any_element()
    }
}

fn row_field(name: &'static str, control: impl IntoElement) -> gpui::Div {
    let t = current();
    div().flex().flex_col().gap(px(6.)).child(label(name, &t)).child(control)
}

fn toggle_row(
    id: &'static str,
    title_text: &'static str,
    hint: &'static str,
    on: bool,
    t: &Theme,
    cx: &mut Context<Settings>,
    f: impl Fn(&mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let hover = t.layer_hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(16.))
        .px(px(14.))
        .py(px(12.))
        .rounded(px(radius::CARD))
        .bg(t.layer)
        .border_1()
        .border_color(t.stroke)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(2.)).child(body(title_text, t.text)).child(caption(hint, t.text2)))
        .child(switch(on, t))
        .on_click(cx.listener(move |_, _, _, cx| {
            f(cx);
            cx.notify();
        }))
}

/// A field that shows the current choice and opens a menu of the others.
pub fn select<V: 'static>(
    id: &'static str,
    current_label: String,
    options: Vec<(String, String)>,
    t: &Theme,
    cx: &mut Context<V>,
    on_pick: impl Fn(&mut V, String, &mut Context<V>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let on_pick = std::rc::Rc::new(on_pick);
    let hover = t.control_hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(38.))
        .px(px(12.))
        .rounded(px(radius::CONTROL))
        .bg(t.control)
        .border_1()
        .border_color(t.stroke)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(div().flex_1().min_w(px(0.)).truncate().child(body(current_label.clone(), t.text)))
        .child(icon("chevron-down", 14., t.text2))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, e: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let me = cx.entity().downgrade();
                let items = options
                    .iter()
                    .map(|(value, name)| {
                        let (value, me, on_pick) = (value.clone(), me.clone(), on_pick.clone());
                        let glyph = if *name == current_label { "check" } else { "blank" };
                        MenuEntry::item(glyph, name.clone(), false, move |_, cx| {
                            if let Some(me) = me.upgrade() {
                                let v = value.clone();
                                let f = on_pick.clone();
                                me.update(cx, |this, cx| f(this, v, cx));
                            }
                        })
                    })
                    .collect();
                let menu = cx.new(|_| Menu { items });
                overlay::open_menu(menu, e.position, cx);
            }),
        )
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        // Your own picture stands in for the profile's icon.
        let me = self.session.read(cx).me.clone();
        let me_img = self.session.update(cx, |s, cx| s.avatar(me.id, cx));
        let nav = |id: &'static str,
                   glyph: Option<&'static str>,
                   text: &'static str,
                   section: Section,
                   this: &Settings,
                   cx: &mut Context<Self>| {
            let on = this.section == section;
            let hover = t.layer_hover;
            div()
                .id(id)
                .relative()
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(38.))
                .px(px(12.))
                .rounded(px(radius::CONTROL))
                .cursor_pointer()
                .when(on, |d| {
                    d.bg(t.layer_hover).child(div().absolute().left(px(0.)).top(px(11.)).w(px(3.)).h(px(16.)).rounded(px(2.)).bg(t.accent))
                })
                .when(!on, |d| d.hover(move |s| s.bg(hover)))
                .child(match glyph {
                    Some(g) => icon(g, 16., if on { t.text } else { t.text2 }),
                    None => avatar(me.name(), me_img.clone(), 18., None),
                })
                .child(
                    div()
                        .text_size(px(13.5))
                        .font_weight(if on { gpui::FontWeight::SEMIBOLD } else { gpui::FontWeight::NORMAL })
                        .text_color(if on { t.text } else { t.text2 })
                        .child(text),
                )
                .on_click(cx.listener(move |s, _, _, cx| s.enter(section, cx)))
        };
        let body_el = match self.section {
            Section::Profile => self.profile_section(&t, cx),
            Section::Voice => self.voice_section(&t, cx),
            Section::Camera => self.camera_section(&t, cx),
            Section::Appearance => self.appearance_section(&t, cx),
            Section::Sounds => self.sounds_section(&t, cx),
            Section::Hotkeys => self.hotkeys_section(&t, cx),
            Section::Advanced => self.advanced_section(&t, cx),
        };
        let heading = match self.section {
            Section::Profile => tr!("Your profile", "Seu perfil"),
            Section::Voice => tr!("Voice and sound", "Voz e som"),
            Section::Camera => tr!("Camera and background", "Câmera e fundo"),
            Section::Appearance => tr!("Appearance and language", "Aparência e idioma"),
            Section::Sounds => tr!("Sounds", "Sons"),
            Section::Hotkeys => tr!("Hotkeys", "Atalhos globais"),
            Section::Advanced => tr!("Advanced", "Avançado"),
        };
        // A fixed height, so the rail stays put while sections of different lengths come and go.
        let max_h = (window.viewport_size().height * 0.86).min(px(640.));
        dialog_card(&t, 820.)
            .track_focus(&self.focus)
            .h(max_h)
            .flex_row()
            .child(
                div()
                    .w(px(210.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(12.))
                    .bg(t.layer)
                    .border_r_1()
                    .border_color(t.stroke)
                    .child(div().px(px(10.)).pt(px(6.)).pb(px(12.)).child(title(tr!("Settings", "Configurações"), t.text)))
                    .child(nav("nav-profile", None, tr!("Profile", "Perfil"), Section::Profile, self, cx))
                    .child(div().mx(px(10.)).my(px(6.)).h(px(1.)).bg(t.stroke))
                    .child(nav("nav-voice", Some("mic"), tr!("Voice and sound", "Voz e som"), Section::Voice, self, cx))
                    .child(nav("nav-camera", Some("camera"), tr!("Camera", "Câmera"), Section::Camera, self, cx))
                    .child(nav("nav-appearance", Some("palette"), tr!("Appearance", "Aparência"), Section::Appearance, self, cx))
                    .child(nav("nav-sounds", Some("music"), tr!("Sounds", "Sons"), Section::Sounds, self, cx))
                    .child(nav("nav-hotkeys", Some("keyboard"), tr!("Hotkeys", "Atalhos globais"), Section::Hotkeys, self, cx))
                    .child(nav("nav-advanced", Some("settings"), tr!("Advanced", "Avançado"), Section::Advanced, self, cx))
                    .child(div().flex_1())
                    .child(
                        div()
                            .px(px(10.))
                            .pb(px(6.))
                            .child(caption(tr!("Changes apply right away.", "As mudanças valem na hora."), t.text3)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(24.))
                            .pt(px(20.))
                            .pb(px(6.))
                            .child(title(heading, t.text))
                            .child(
                                icon_button("settings-close", "close", &t)
                                    .tooltip(tip(tr!("Close (Esc)", "Fechar (Esc)"), &t))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                            ),
                    )
                    .child(
                        div()
                            .id("settings-body")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .px(px(24.))
                            .pt(px(10.))
                            .pb(px(24.))
                            .child(body_el),
                    ),
            )
    }
}

pub fn _slot(_: &FrameSlot) {}
