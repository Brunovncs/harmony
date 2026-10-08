//! "Choose what to share": a screen or a window, with a still of each, and how to send it.

use crate::media::screen::{self, Resolution, ScreenChoice, ScreenSource, Sound, SourceKind};
use crate::prefs::{prefs, set_prefs};
use crate::theme::{current, px, radius};
use crate::ui::overlay::{Dismiss, dialog_card};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, Render, RenderImage,
    StatefulInteractiveElement, Styled, StyledImage, Task, Window, div, img,
};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

type OnPick = Rc<dyn Fn(ScreenChoice, &mut Window, &mut App)>;

pub struct Picker {
    focus: FocusHandle,
    kind: SourceKind,
    sources: Option<Vec<ScreenSource>>,
    /// Stills by source, filled in as they land; dropped from the window's atlas when replaced.
    stills: HashMap<u64, Arc<RenderImage>>,
    picked: Option<u64>,
    resolution: Resolution,
    fps: u32,
    sharp: bool,
    sound: Sound,
    /// Starting a share, or changing one already running.
    changing: bool,
    on_pick: OnPick,
    _load: Task<()>,
}

impl EventEmitter<Dismiss> for Picker {}

impl Focusable for Picker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Picker {
    pub fn new(
        current: Option<ScreenChoice>,
        cx: &mut Context<Self>,
        on_pick: impl Fn(ScreenChoice, &mut Window, &mut App) + 'static,
    ) -> Picker {
        let p = prefs(cx);
        let mut picker = Picker {
            focus: cx.focus_handle(),
            kind: current.as_ref().map(|c| c.source.kind).unwrap_or(SourceKind::Screen),
            sources: None,
            stills: HashMap::new(),
            picked: current.as_ref().map(|c| c.source.id),
            resolution: current.as_ref().map(|c| c.resolution).unwrap_or_else(|| Resolution::from_setting(&p.resolution)),
            fps: current.as_ref().map(|c| c.fps).unwrap_or(if p.framerate == 0 { 30 } else { p.framerate }),
            sharp: current.as_ref().map(|c| c.sharp).unwrap_or(p.priority != "smooth"),
            sound: current.as_ref().map(|c| c.sound).unwrap_or(Sound::System),
            changing: current.is_some(),
            on_pick: Rc::new(on_pick),
            _load: Task::ready(()),
        };
        cx.on_release(|this: &mut Picker, cx| this.drop_stills(cx)).detach();
        picker.load(cx);
        picker
    }

    /// The list first, by title; then the stills, in parallel, each drawn as it lands.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.sources = None;
        self.drop_stills(cx);
        let kind = self.kind;
        self._load = cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async move { screen::sources(kind) }).await;
            let ids = list.iter().map(|s| s.id).collect();
            let shown = this.update(cx, |this, cx| {
                if this.picked.is_none_or(|id| !list.iter().any(|s| s.id == id)) {
                    this.picked = list.first().map(|s| s.id);
                }
                this.sources = Some(list);
                cx.notify();
            });
            if shown.is_err() {
                return;
            }
            // Dropping this task (closed, or another kind picked) drops the receiver, which
            // stops the threads taking stills.
            let stills = screen::stills(kind, ids);
            while let Ok((id, still)) = stills.recv().await {
                let Some(still) = still else { continue };
                let ok = this.update(cx, |this, cx| {
                    if let Some(s) = this.sources.iter_mut().flatten().find(|s| s.id == id) {
                        s.size = Some(still.size);
                    }
                    if let Some(old) = this.stills.insert(id, still.image) {
                        cx.drop_image(old, None);
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        });
        cx.notify();
    }

    fn drop_stills(&mut self, cx: &mut App) {
        for (_, image) in self.stills.drain() {
            cx.drop_image(image, None);
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.sources.as_ref().and_then(|l| l.iter().find(|s| Some(s.id) == self.picked)).cloned() else { return };
        let sound = if self.sound == Sound::App && source.kind == SourceKind::Screen { Sound::System } else { self.sound };
        let choice = ScreenChoice { source, resolution: self.resolution, fps: self.fps, sharp: self.sharp, sound };
        let (res, fps, sharp) = (self.resolution, self.fps, self.sharp);
        set_prefs(cx, |p| {
            p.resolution = res.setting().into();
            p.framerate = fps;
            p.priority = if sharp { "sharp" } else { "smooth" }.into();
        });
        cx.emit(Dismiss);
        (self.on_pick)(choice, window, cx);
    }
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let mut grid = div().id("sources").flex().flex_wrap().gap(px(10.)).max_h(px(330.)).overflow_y_scroll();
        match &self.sources {
            None => {
                grid = grid.child(
                    div()
                        .w_full()
                        .py(px(60.))
                        .flex()
                        .justify_center()
                        .child(caption(tr!("Looking for what you can share…", "Procurando o que dá para compartilhar…"), t.text3)),
                );
            }
            Some(list) if list.is_empty() => {
                grid = grid.child(div().w_full().py(px(60.)).flex().justify_center().child(caption(
                    if self.kind == SourceKind::Window {
                        tr!(
                            "No windows to share. Open the app you want to show first.",
                            "Nenhuma janela para compartilhar. Abra primeiro o app que você quer mostrar."
                        )
                    } else {
                        tr!("No screens found.", "Nenhuma tela encontrada.")
                    },
                    t.text3,
                )));
            }
            Some(list) => {
                for s in list.clone() {
                    let on = Some(s.id) == self.picked;
                    let id = s.id;
                    let hover = t.stroke_strong;
                    grid = grid.child(
                        div()
                            .id(("source", s.id))
                            .w(px(214.))
                            .flex()
                            .flex_col()
                            .rounded(px(radius::CARD))
                            .overflow_hidden()
                            .bg(t.layer)
                            .border_2()
                            .border_color(if on { t.accent } else { t.stroke })
                            .cursor_pointer()
                            .when(!on, |d| d.hover(move |s| s.border_color(hover)))
                            .child(
                                div().h(px(120.)).bg(t.stage).flex().items_center().justify_center().child(match self.stills.get(&s.id) {
                                    Some(i) => img(i.clone()).size_full().object_fit(gpui::ObjectFit::Contain).into_any_element(),
                                    None => icon(if s.kind == SourceKind::Screen { "monitor" } else { "image" }, 24., t.text3)
                                        .into_any_element(),
                                }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .px(px(10.))
                                    .py(px(8.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_size(px(13.))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child(s.title.clone()),
                                    )
                                    .when_some(s.size, |d, (w, h)| d.child(mono(format!("{w}×{h}"), t.text3))),
                            )
                            .on_click(cx.listener(move |this, e: &gpui::ClickEvent, window, cx| {
                                this.picked = Some(id);
                                if e.click_count() == 2 {
                                    this.start(window, cx);
                                }
                                cx.notify();
                            })),
                    );
                }
            }
        }
        let window_kind = self.kind == SourceKind::Window;
        let sound_choices = if window_kind {
            vec![
                (Sound::App, tr!("This app only", "Só este app").into()),
                (Sound::System, tr!("Everything", "Tudo").into()),
                (Sound::Off, tr!("No sound", "Sem som").into()),
            ]
        } else {
            vec![(Sound::System, tr!("Computer sound", "Som do computador").into()), (Sound::Off, tr!("No sound", "Sem som").into())]
        };
        let sound = if !window_kind && self.sound == Sound::App { Sound::System } else { self.sound };
        let note = match sound {
            Sound::System => tr!(
                "Viewers hear what your computer plays, but not the call.",
                "Quem assiste ouve o que o computador toca, mas não a chamada."
            ),
            Sound::App => tr!("Viewers hear only this app.", "Quem assiste ouve só este app."),
            Sound::Off => tr!("Viewers see the picture without sound.", "Quem assiste vê a imagem sem som."),
        };
        let field = |name: &'static str, control: gpui::Div| div().flex().flex_col().gap(px(6.)).child(label(name, &t)).child(control);
        dialog_card(&t, 720.)
            .track_focus(&self.focus)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .p(px(22.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(title(
                                if self.changing {
                                    tr!("Change what you share", "Trocar o que você compartilha")
                                } else {
                                    tr!("Choose what to share", "Escolha o que compartilhar")
                                },
                                t.text,
                            ))
                            .child(caption(
                                tr!("People in the channel see it as a tile.", "Quem está no canal vê isso num quadro."),
                                t.text2,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().flex_1().child(segmented(
                                "kind",
                                vec![
                                    (SourceKind::Screen, tr!("Screens", "Telas").into()),
                                    (SourceKind::Window, tr!("Windows", "Janelas").into()),
                                ],
                                self.kind,
                                &t,
                                cx,
                                |this, k, _, cx| {
                                    this.kind = k;
                                    this.load(cx);
                                },
                            )))
                            .child(
                                button("refresh", tr!("Refresh", "Atualizar"), Kind::Standard, &t)
                                    .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                            ),
                    )
                    .child(grid)
                    .child(
                        div()
                            .flex()
                            .gap(px(14.))
                            .child(div().flex_1().child(field(
                                tr!("Resolution", "Resolução"),
                                segmented(
                                    "res",
                                    vec![
                                        (Resolution::P480, "480p".into()),
                                        (Resolution::P720, "720p".into()),
                                        (Resolution::P1080, "1080p".into()),
                                        (Resolution::Native, tr!("Native", "Nativa").into()),
                                    ],
                                    self.resolution,
                                    &t,
                                    cx,
                                    |this, r, _, cx| {
                                        this.resolution = r;
                                        cx.notify();
                                    },
                                ),
                            )))
                            .child(div().w(px(230.)).child(field(
                                tr!("Frame rate", "Quadros por segundo"),
                                segmented(
                                    "fps",
                                    vec![(30, "30".into()), (60, "60".into()), (120, "120".into())],
                                    self.fps,
                                    &t,
                                    cx,
                                    |this, f, _, cx| {
                                        this.fps = f;
                                        cx.notify();
                                    },
                                ),
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(14.))
                            .child(div().flex_1().child(field(
                                tr!("When the connection is tight", "Se a conexão piorar"),
                                segmented(
                                    "priority",
                                    vec![
                                        (true, tr!("Keep it sharp", "Priorizar a nitidez").into()),
                                        (false, tr!("Keep it smooth", "Priorizar a fluidez").into()),
                                    ],
                                    self.sharp,
                                    &t,
                                    cx,
                                    |this, s, _, cx| {
                                        this.sharp = s;
                                        cx.notify();
                                    },
                                ),
                            )))
                            .child(div().flex_1().child(field(
                                tr!("Sound", "Som"),
                                segmented("sound", sound_choices, sound, &t, cx, |this, s, _, cx| {
                                    this.sound = s;
                                    cx.notify();
                                }),
                            ))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(22.))
                    .py(px(14.))
                    .bg(t.layer)
                    .border_t_1()
                    .border_color(t.stroke)
                    .child(div().flex_1().child(caption(note, t.text2)))
                    .child(
                        button("picker-cancel", tr!("Cancel", "Cancelar"), Kind::Standard, &t)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                    )
                    .child(
                        button(
                            "picker-start",
                            if self.changing { tr!("Use this", "Usar") } else { tr!("Start sharing", "Começar a compartilhar") },
                            Kind::Primary,
                            &t,
                        )
                        .when(self.picked.is_none(), |b| b.opacity(0.5))
                        .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                    ),
            )
    }
}
