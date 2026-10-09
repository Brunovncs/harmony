//! The voice stage: everyone in the channel as cards that light up while they speak, or the
//! cameras and screens being shared as tiles, one of them big when focused; and the soundboard.

use super::sidebar::live_chip;
use super::{Center, ServerView, VolumeSlider};
use crate::core;
use crate::core::settings::HotkeyAction;
use crate::core::types::*;
use crate::hotkeys::Combo;
use crate::media::audio;
use crate::media::video::{Tile, TileKind};
use crate::media::voice::Voice;
use crate::prefs::{prefs, set_prefs};
use crate::session::Session;
use crate::theme::{MONO, Theme, current, px, radius};
use crate::ui::hotkeys::Recorder;
use crate::ui::overlay::{self, Dismiss, menu_card, menu_item, menu_rule};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, Focusable, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, StyleRefinement, Styled, StyledImage, Window, anchored,
    div, img,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Whether you are sending your camera and your screen.
pub fn sharing(_: &Entity<Session>, voice: Option<&Entity<Voice>>, cx: &App) -> (bool, bool) {
    voice.map(|v| (v.read(cx).camera_on(), v.read(cx).screen_on())).unwrap_or_default()
}

/// What decoded soundpad clips may hold before the least recently played are let go.
const CLIP_BUDGET: usize = 48 << 20;

/// Soundpad clips by hash: decoded (16-bit, shared by every play), and the ones still decoding
/// with the volumes they are waiting to play at.
struct Clips {
    decoded: core::lru::Lru<String, Arc<[i16]>>,
    decoding: HashMap<String, Vec<f32>>,
}

fn clips() -> &'static Mutex<Clips> {
    static C: OnceLock<Mutex<Clips>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Clips { decoded: core::lru::Lru::new(CLIP_BUDGET), decoding: HashMap::new() }))
}

/// Plays a soundpad clip from the cache, decoding it (once, however many ask) the first time.
pub fn play_clip(session: &Entity<Session>, hash: String, gain: f32, cx: &mut App) {
    {
        let mut c = clips().lock().unwrap();
        if let Some(samples) = c.decoded.get(&hash).cloned() {
            drop(c);
            audio::play_shared(samples, gain);
            return;
        }
        if let Some(waiting) = c.decoding.get_mut(&hash) {
            waiting.push(gain);
            return;
        }
        c.decoding.insert(hash.clone(), vec![gain]);
    }
    let (api, cache) = {
        let s = session.read(cx);
        (s.api.clone(), s.cache.clone())
    };
    core::runtime().spawn(async move {
        let decoded = match cache.get(&api, &hash).await {
            Ok((bytes, _)) => tokio::task::spawn_blocking(move || audio::decode(Arc::unwrap_or_clone(bytes)))
                .await
                .unwrap_or_else(|e| Err(e.into()))
                .map(|f| f.iter().map(|s| (s.clamp(-1., 1.) * 32767.) as i16).collect::<Arc<[i16]>>()),
            Err(e) => Err(e.into()),
        };
        let gains = {
            let mut c = clips().lock().unwrap();
            if let Ok(samples) = &decoded {
                c.decoded.insert(hash.clone(), samples.clone(), samples.len() * 2);
            }
            c.decoding.remove(&hash).unwrap_or_default()
        };
        match decoded {
            Ok(samples) => gains.into_iter().for_each(|g| audio::play_shared(samples.clone(), g)),
            Err(e) => log::info!("soundpad clip {hash} did not play: {e}"),
        }
    });
}

impl ServerView {
    pub fn render_stage(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(call) = self.call.clone() else {
            return super::chat::empty_state(
                "volume",
                tr!("Not in a voice channel", "Fora de um canal de voz"),
                tr!("Pick a voice channel on the left to join it.", "Escolha um canal de voz à esquerda para entrar."),
                t,
            )
            .into_any_element();
        };
        let channel = call.read(cx).channel;
        let name = self.session.read(cx).channel(channel).map(|c| c.name.clone()).unwrap_or_default();
        let roster = self.session.read(cx).rosters.get(&channel).cloned().unwrap_or_default();
        let voice = call.read(cx).voice.clone();
        let tiles: Vec<Entity<Tile>> = voice.as_ref().map(|v| v.read(cx).tiles.clone()).unwrap_or_default();
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(16.))
            .border_b_1()
            .border_color(t.stroke)
            .child(icon("volume", 18., t.success))
            .child(title(name, t.text))
            .child(mono(trf!("{} here", "{} aqui", roster.len()), t.text3))
            .child(div().flex_1())
            .children(voice.as_ref().map(|v| self.ghosts(v, t, cx)));
        let body = if tiles.is_empty() {
            self.render_people(&roster, channel, t, cx)
        } else {
            self.render_tiles(&tiles, voice.as_ref(), t, window, cx)
        };
        div().flex().flex_col().size_full().child(header).child(div().flex_1().min_h(px(0.)).child(body)).into_any_element()
    }

    /// Closed streams, as chips that reopen them.
    fn ghosts(&mut self, voice: &Entity<Voice>, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let closed: Vec<(UserId, TileKind)> = voice.read(cx).closed.0.iter().copied().collect();
        let mut row = div().flex().gap(px(6.));
        for (i, (user, kind)) in closed.into_iter().enumerate() {
            let who = self.session.read(cx).display_name(Some(user), None);
            let v = voice.clone();
            let hover = t.layer_hover;
            row = row.child(
                div()
                    .id(("ghost", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(10.))
                    .rounded_full()
                    .border_1()
                    .border_dashed()
                    .border_color(t.stroke_strong)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(icon(if kind == TileKind::Screen { "monitor" } else { "camera" }, 13., t.text2))
                    .child(caption(trf!("{} · watch again", "{} · assistir de novo", who), t.text2))
                    .on_click(cx.listener(move |_, _, _, cx| v.update(cx, |v, cx| v.reopen_tile(user, kind, cx)))),
            );
        }
        row.into_any_element()
    }

    fn render_people(&mut self, roster: &[Member], channel: ChannelId, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let me = self.session.read(cx).me.id;
        let mut grid = div().flex().flex_wrap().justify_center().content_center().gap(px(14.)).p(px(24.)).size_full();
        for m in roster {
            let is_speaking = self.is_speaking(channel, m.user_id, cx);
            let name = self.session.read(cx).display_name(Some(m.user_id), Some(&m.nickname));
            let image = self.session.update(cx, |s, cx| s.avatar(m.user_id, cx));
            let (user, mid) = (m.user_id, m.mid);
            let gain = crate::media::voice::volume_of(m.user_id);
            let card = div()
                .id(("person", m.mid as u64))
                .relative()
                .w(px(200.))
                .h(px(150.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .rounded(px(radius::CARD + 2.))
                .bg(t.layer)
                .border_1()
                .border_color(t.stroke)
                .child(avatar(&name, image, 64., None))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().text_size(px(14.)).font_weight(gpui::FontWeight::MEDIUM).child(if m.user_id == me {
                            trf!("{} (you)", "{} (você)", name)
                        } else {
                            name
                        }))
                        .when(m.muted || m.silenced(), |d| d.child(icon("mic-off", 14., if m.force_muted { t.critical } else { t.text3 })))
                        .when(m.deafened, |d| d.child(icon("headphones-off", 14., t.text3)))
                        .when(m.publishes("s"), |d| d.child(live_chip(t, false))),
                )
                .when(m.user_id != me && gain != 1., |d| {
                    d.child(mono(trf!("{:.0}% for you", "{:.0}% para você", gain * 100.), if gain > 1. { t.caution } else { t.text3 }))
                })
                .when(m.user_id != me, |d| {
                    d.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, window, cx| this.peer_menu(channel, user, mid, e.position, window, cx)),
                    )
                });
            grid = grid.child(speaking(card, is_speaking, radius::CARD + 2., t));
        }
        if roster.len() <= 1 {
            grid = grid.child(div().w_full().flex().justify_center().child(caption(
                tr!(
                    "You're the only one here. Share your screen or turn on your camera while you wait.",
                    "Só você está aqui. Compartilhe a tela ou ligue a câmera enquanto espera."
                ),
                t.text3,
            )));
        }
        grid.into_any_element()
    }

    fn render_tiles(
        &mut self,
        tiles: &[Entity<Tile>],
        voice: Option<&Entity<Voice>>,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The space the stage has, roughly: the window less the side panes.
        let vw = f32::from(window.viewport_size().width) / crate::theme::scale();
        let vh = f32::from(window.viewport_size().height) / crate::theme::scale();
        let side = 268. + if prefs(cx).show_members && vw > 900. { 232. } else { 0. } + 40.;
        let (w, h) = ((vw - side).max(320.) - 32., (vh - 48. - 52. - 40.).max(240.));
        let focused = self.focused_tile.clone().filter(|f| tiles.contains(f));
        if let Some(f) = focused {
            let others: Vec<Entity<Tile>> = tiles.iter().filter(|x| **x != f).cloned().collect();
            let strip_h = if others.is_empty() { 0. } else { 130. };
            let big = self.tile_el(&f, voice, w, h - strip_h, true, t, window, cx);
            let mut strip = div().flex().gap(px(10.)).h(px(strip_h)).justify_center();
            for o in &others {
                strip = strip.child(self.tile_el(o, voice, 200., 112., false, t, window, cx));
            }
            return div().flex().flex_col().gap(px(10.)).p(px(16.)).size_full().items_center().child(big).child(strip).into_any_element();
        }
        // The column count that makes 16:9 tiles largest.
        let n = tiles.len();
        let gap = 12.;
        let (mut best, mut cols) = (0., 1);
        for c in 1..=n {
            let rows = n.div_ceil(c);
            let tw = ((w - gap * (c - 1) as f32) / c as f32).min((h - gap * (rows - 1) as f32) / rows as f32 * 16. / 9.);
            if tw > best {
                best = tw;
                cols = c;
            }
        }
        let tw = best.max(160.);
        let th = tw * 9. / 16.;
        let _ = cols;
        let mut grid = div().flex().flex_wrap().justify_center().content_center().gap(px(gap)).p(px(16.)).size_full();
        for tile in tiles {
            grid = grid.child(self.tile_el(tile, voice, tw, th, false, t, window, cx));
        }
        grid.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn tile_el(
        &mut self,
        tile: &Entity<Tile>,
        voice: Option<&Entity<Voice>>,
        w: f32,
        h: f32,
        focused: bool,
        t: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tl = tile.read(cx);
        let (user, kind, local) = (tl.user, tl.kind, tl.local);
        let gain = tl.gain();
        let has_sound = tl.sound.is_some();
        // Ask the decoder for frames no bigger than drawn.
        let s = window.scale_factor() * crate::theme::scale();
        tl.slot.want_w.store((w * s) as u32, std::sync::atomic::Ordering::Relaxed);
        tl.slot.want_h.store((h * s) as u32, std::sync::atomic::Ordering::Relaxed);
        let name = self.session.read(cx).display_name(Some(user), None);
        let id = tile.entity_id().as_u64();
        let group = SharedString::from(format!("tile-{id}"));
        let (t1, t2, t3) = (tile.clone(), tile.clone(), tile.clone());
        let voice = voice.cloned();
        let lit = kind == TileKind::Camera && self.call_channel(cx).is_some_and(|c| self.is_speaking(c, user, cx));
        let el = div()
            .id(("tile", id))
            .group(group.clone())
            .relative()
            .w(px(w))
            .h(px(h))
            .flex_none()
            .rounded(px(radius::CARD + 2.))
            .overflow_hidden()
            .bg(t.stage)
            .border_1()
            .border_color(t.stroke)
            // Its own view: a new frame redraws the picture alone.
            .child(tile.clone().cached(StyleRefinement::default().size_full()))
            .child(
                div()
                    .absolute()
                    .left(px(16.))
                    .bottom(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .bg(gpui::black().opacity(0.55))
                    .when(kind == TileKind::Screen, |d| d.child(live_chip(t, true)))
                    .child(div().text_size(px(12.5)).text_color(gpui::white()).child(if local {
                        trf!("{} (you)", "{} (você)", name)
                    } else {
                        name.to_string()
                    }))
                    .child(div().text_size(px(11.5)).text_color(gpui::white().opacity(0.7)).child(if kind == TileKind::Screen {
                        tr!("screen", "tela")
                    } else {
                        tr!("camera", "câmera")
                    })),
            )
            .child(
                div()
                    .absolute()
                    .top(px(8.))
                    .right(px(8.))
                    .flex()
                    .gap(px(4.))
                    .p(px(3.))
                    .rounded(px(9.))
                    .bg(gpui::black().opacity(0.55))
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .when(local && kind == TileKind::Screen, |d| {
                        d.child(
                            tool_button(("change-share", id), "screen-share", false, gpui::white(), t)
                                .tooltip(tip(tr!("Change what you share", "Trocar o que você compartilha"), t))
                                .on_click(cx.listener(|this, _, window, cx| this.pick_screen(window, cx))),
                        )
                        .child(
                            tool_button(("stop-share", id), "screen-share-off", false, gpui::white(), t)
                                .tooltip(tip(tr!("Stop sharing", "Parar de compartilhar"), t))
                                .on_click(cx.listener(|this, _, _, cx| this.stop_screen(cx))),
                        )
                    })
                    .when(local && kind == TileKind::Camera, |d| {
                        d.child(
                            tool_button(("stop-camera", id), "camera-off", false, gpui::white(), t)
                                .tooltip(tip(tr!("Turn the camera off", "Desligar a câmera"), t))
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_camera(window, cx))),
                        )
                    })
                    .when(has_sound, |d| {
                        let tile = t3.clone();
                        d.child(
                            // Room either side for the knob, which sits half past each end.
                            div().px(px(8.)).child(
                                VolumeSlider::new(gain, move |g, cx| tile.update(cx, |t, cx| t.set_gain(g, cx))).width(px(112.)).render(t, cx),
                            ),
                        )
                    })
                    .child(
                        tool_button(("focus", id), if focused { "shrink" } else { "expand" }, false, gpui::white(), t)
                            .tooltip(tip(
                                if focused { tr!("Back to the grid", "Voltar à grade") } else { tr!("Make it big", "Ampliar") },
                                t,
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.focused_tile = if this.focused_tile.as_ref() == Some(&t1) { None } else { Some(t1.clone()) };
                                cx.notify();
                            })),
                    )
                    .child(
                        tool_button(("fullscreen", id), "fullscreen", false, gpui::white(), t)
                            .tooltip(tip(
                                if window.is_fullscreen() {
                                    tr!("Leave full screen (double-click)", "Sair da tela cheia (clique duplo)")
                                } else {
                                    tr!("Full screen (double-click)", "Tela cheia (clique duplo)")
                                },
                                t,
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focused_tile = Some(t2.clone());
                                window.toggle_fullscreen();
                                cx.notify();
                            })),
                    )
                    .when(!local, |d| {
                        let (tile, voice) = (tile.clone(), voice.clone());
                        d.child(
                            tool_button(("close-tile", id), "close", false, gpui::white(), t)
                                .tooltip(tip(tr!("Stop watching", "Parar de assistir"), t))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    if let Some(v) = &voice {
                                        v.update(cx, |v, cx| v.close_tile(&tile, cx));
                                    }
                                })),
                        )
                    }),
            )
            .on_click(cx.listener({
                let tile = tile.clone();
                move |this, e: &gpui::ClickEvent, window, cx| {
                    if e.click_count() == 2 {
                        this.focused_tile = Some(tile.clone());
                        window.toggle_fullscreen();
                        cx.notify();
                    }
                }
            }));
        speaking(el, lit, radius::CARD + 2., t).into_any_element()
    }

    pub fn open_soundpad(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.call_channel(cx) else { return };
        let view = cx.new(|_| Soundpad { session: self.session.clone(), channel, menu: None, recorder: None });
        let _ = window;
        overlay::open_menu(view, at, cx);
    }

    pub fn ensure_stage(&mut self, cx: &mut Context<Self>) {
        self.center = Center::Stage;
        cx.notify();
    }
}

impl Render for Tile {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        match self.image.clone() {
            Some(image) => img(image).size_full().rounded(px(radius::CARD + 1.)).object_fit(gpui::ObjectFit::Contain).into_any_element(),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(icon(if self.kind == TileKind::Screen { "monitor" } else { "camera" }, 28., t.text3.opacity(0.7)))
                .child(caption(
                    if self.failed {
                        tr!("Could not connect to this stream", "Não foi possível conectar a esta transmissão")
                    } else {
                        tr!("Waiting for video…", "Esperando o vídeo…")
                    },
                    t.text3,
                ))
                .into_any_element(),
        }
    }
}

/// The soundboard: the server's clips, played for everyone in the channel. A clip's menu and the
/// hotkey recorder are drawn inside it rather than as layers of their own: the soundboard is a
/// menu itself, and opening another would close it.
struct Soundpad {
    session: Entity<Session>,
    channel: ChannelId,
    /// The clip whose menu is open, and where.
    menu: Option<(Clip, Point<Pixels>)>,
    recorder: Option<Entity<Recorder>>,
}

impl EventEmitter<Dismiss> for Soundpad {}

impl Soundpad {
    fn record(&mut self, clip: &Clip, window: &mut Window, cx: &mut Context<Self>) {
        let server = self.session.read(cx).api.base();
        let what = SharedString::from(clip.name.clone());
        let recorder = cx.new(|cx| Recorder::new(HotkeyAction::Clip(clip.id), what, server, false, cx));
        cx.subscribe(&recorder, |this: &mut Soundpad, _, _: &Dismiss, cx| {
            this.recorder = None;
            cx.notify();
        })
        .detach();
        recorder.focus_handle(cx).focus(window, cx);
        self.recorder = Some(recorder);
        cx.notify();
    }

    /// One clip's menu: its hotkey, for everyone (anyone in the call can play a clip), and an
    /// admin's tools.
    fn clip_menu(&mut self, clip: Clip, at: Point<Pixels>, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.session.read(cx);
        let admin = s.me.role.is_admin();
        let server = s.api.base();
        let order: Vec<i64> = s.clips.iter().map(|c| c.id).collect();
        let id = clip.id;
        let bound = Combo::parse(prefs(cx).hotkey(&server, HotkeyAction::Clip(id)));
        let set: SharedString = match bound {
            Some(c) => trf!("Change hotkey ({})", "Trocar atalho ({})", c.keycaps().join(" + ")).into(),
            None => tr!("Set hotkey", "Definir atalho").into(),
        };
        let to_record = clip.clone();
        let mut card = menu_card(t).occlude().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
            menu_item("clip-hotkey", Some("keyboard"), set, false, t).on_click(cx.listener(move |this, _, window, cx| {
                this.menu = None;
                this.record(&to_record, window, cx);
            })),
        );
        if bound.is_some() {
            card = card.child(menu_item("clip-unbind", Some("close"), tr!("Remove hotkey", "Remover atalho"), false, t).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.menu = None;
                    crate::ui::hotkeys::bind(HotkeyAction::Clip(id), &server, "", cx);
                    cx.notify();
                }),
            ));
        }
        if admin {
            let pos = order.iter().position(|x| *x == id).unwrap_or(0);
            let moved = |delta: isize| {
                let mut o = order.clone();
                let to = (pos as isize + delta).clamp(0, o.len() as isize - 1) as usize;
                let v = o.remove(pos);
                o.insert(to, v);
                o
            };
            let (earlier, later) = (moved(-1), moved(1));
            let (s1, s2, s3, s4) = (self.session.clone(), self.session.clone(), self.session.clone(), self.session.clone());
            card = card
                .child(menu_rule(t))
                .child(menu_item("clip-rename", Some("edit"), tr!("Rename", "Renomear"), false, t).on_click(cx.listener(
                    move |_, _, window, cx| {
                        // Its dialog would open under the soundboard, so the soundboard goes first.
                        cx.emit(Dismiss);
                        rename_clip(s1.clone(), clip.clone(), window, cx);
                    },
                )))
                .child(menu_item("clip-earlier", Some("arrow-left"), tr!("Move earlier", "Mover para antes"), false, t).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        reorder_clips(&s2, earlier.clone(), cx);
                    }),
                ))
                .child(menu_item("clip-later", Some("arrow-right"), tr!("Move later", "Mover para depois"), false, t).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        reorder_clips(&s3, later.clone(), cx);
                    }),
                ))
                .child(menu_rule(t))
                .child(menu_item("clip-delete", Some("delete"), tr!("Delete this sound", "Apagar este som"), true, t).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        s4.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.delete_clip(id).await }), |_, _, _| {}));
                    }),
                ));
        }
        anchored().position(at).snap_to_window_with_margin(px(8.)).child(card).into_any_element()
    }
}

impl Render for Soundpad {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let clips = self.session.read(cx).clips.clone();
        let admin = self.session.read(cx).me.role.is_admin();
        let server = self.session.read(cx).api.base();
        let bound = prefs(cx).server_clip_hotkeys(&server).cloned().unwrap_or_default();
        let volume = prefs(cx).soundpad_volume as f32 / 100.;
        let mut grid = div().id("clips").flex().flex_wrap().gap(px(6.)).max_h(px(300.)).overflow_y_scroll();
        if clips.is_empty() {
            grid = grid.child(caption(
                if admin {
                    tr!("No sounds yet. Add one with the button below.", "Nenhum som ainda. Adicione um com o botão abaixo.")
                } else {
                    tr!("No sounds yet. An admin can add some.", "Nenhum som ainda. Um admin pode adicionar.")
                },
                t.text3,
            ));
        }
        for c in clips {
            let (id, channel, session) = (c.id, self.channel, self.session.clone());
            let combo = bound.get(&id.to_string()).and_then(|a| Combo::parse(a));
            let hover = t.layer_hover;
            grid = grid.child(
                div()
                    .id(("clip", c.id as u64))
                    .w(px(112.))
                    .h(px(58.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .rounded(px(radius::CONTROL))
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .active(|s| s.opacity(0.75))
                    .child(div().text_size(px(18.)).child(c.emoji.clone().unwrap_or_else(|| "🔊".into())))
                    .child(div().max_w(px(100.)).truncate().text_size(px(12.)).text_color(t.text2).child(c.name.clone()))
                    .when_some(combo, |d, k| {
                        d.child(
                            div()
                                .max_w(px(104.))
                                .truncate()
                                .font_family(MONO)
                                .text_size(px(9.))
                                .line_height(px(11.))
                                .text_color(t.text3)
                                .child(k.keycaps().join("+")),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.menu = Some((c.clone(), e.position));
                            cx.notify();
                        }),
                    )
                    .on_click(move |_, _, cx| {
                        super::request(&session, "soundpad:play", serde_json::json!({ "channelId": channel, "clipId": id }), cx);
                    }),
            );
        }
        let menu = self.menu.clone().map(|(clip, at)| self.clip_menu(clip, at, &t, cx));
        let session = self.session.clone();
        let close_menu = |this: &mut Soundpad, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Soundpad>| {
            if this.menu.take().is_some() {
                cx.notify();
            }
        };
        div()
            .w(px(400.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(14.))
            .rounded(px(radius::CONTROL + 2.))
            .bg(t.popover)
            .border_1()
            .border_color(t.stroke_strong)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, cx.listener(close_menu))
            .on_mouse_down(MouseButton::Right, cx.listener(close_menu))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(icon("soundboard", 18., t.accent))
                            .child(title(tr!("Soundboard", "Painel de sons"), t.text)),
                    )
                    .child(mono(format!("{:.0}%", volume * 100.), if volume > 1. { t.caution } else { t.text3 })),
            )
            .map(|d| match self.recorder.clone() {
                Some(recorder) => d.child(recorder),
                None => d
                    .child(
                        VolumeSlider::new(volume, |g, cx| set_prefs(cx, |p| p.soundpad_volume = (g * 100.).round() as u32)).render(&t, cx),
                    )
                    .child(grid)
                    .when(admin, |d| {
                        d.child(icon_label_button("add-clip", "plus", tr!("Add a sound", "Adicionar um som"), Kind::Standard, &t).on_click(
                            cx.listener(move |_, _, window, cx| {
                                // The file picker and the dialog after it would open under the soundboard.
                                cx.emit(Dismiss);
                                add_clip(session.clone(), window, cx)
                            }),
                        ))
                    }),
            })
            .children(menu)
    }
}

fn reorder_clips(session: &Entity<Session>, order: Vec<i64>, cx: &mut App) {
    session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.reorder_clips(&order).await }), |_, _, _| {}));
}

fn rename_clip(session: Entity<Session>, clip: Clip, window: &mut Window, cx: &mut App) {
    let id = clip.id;
    overlay::Ask::open(
        tr!("Rename the sound", "Renomear o som"),
        None,
        tr!("Save", "Salvar"),
        false,
        vec![
            overlay::Field::Text {
                label: tr!("Name", "Nome"),
                value: clip.name.clone(),
                placeholder: "",
                secret: false,
                multiline: false,
                max: 32,
            },
            overlay::Field::Text {
                label: "Emoji",
                value: clip.emoji.clone().unwrap_or_default(),
                placeholder: "",
                secret: false,
                multiline: false,
                max: 4,
            },
        ],
        window,
        cx,
        move |v, _, cx| {
            let (name, emoji) = (v[0].trim().to_string(), v[1].trim().to_string());
            session.update(cx, |s, cx| {
                s.call(
                    cx,
                    move |api| Box::pin(async move { api.rename_clip(id, &name, (!emoji.is_empty()).then_some(emoji.as_str())).await }),
                    |_, _, _| {},
                )
            });
            None
        },
    );
}

pub(super) fn add_clip(session: Entity<Session>, window: &mut Window, cx: &mut App) {
    let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(tr!("Add", "Adicionar").into()),
    });
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = paths.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let name = path.file_stem().map(|s| s.to_string_lossy().chars().take(32).collect::<String>()).unwrap_or_default();
        let file = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let Ok(bytes) = std::fs::read(&path) else { return };
        if bytes.len() > 2 * 1024 * 1024 {
            cx.update(|cx| overlay::toast(tr!("Sounds can be up to 2 MB.", "Os sons podem ter até 2 MB."), cx));
            return;
        }
        let ct = super::chat::content_type_for(&file);
        let _ = window_handle.update(cx, |_, window, cx| {
            overlay::Ask::open(
                tr!("Add a sound", "Adicionar um som"),
                None,
                tr!("Add", "Adicionar"),
                false,
                vec![
                    overlay::Field::Text {
                        label: tr!("Name", "Nome"),
                        value: name.clone(),
                        placeholder: "",
                        secret: false,
                        multiline: false,
                        max: 32,
                    },
                    overlay::Field::Text { label: "Emoji", value: "🔊".into(), placeholder: "", secret: false, multiline: false, max: 4 },
                ],
                window,
                cx,
                move |v, _, cx| {
                    let (name, emoji, bytes, ct) = (v[0].trim().to_string(), v[1].trim().to_string(), bytes.clone(), ct.clone());
                    session.update(cx, |s, cx| {
                        s.call(
                            cx,
                            move |api| {
                                Box::pin(async move {
                                    let up = api.upload(bytes, &ct).await?;
                                    api.add_clip(&name, (!emoji.is_empty()).then_some(emoji.as_str()), &up.hash).await
                                })
                            },
                            |_, _, _| {},
                        )
                    });
                    None
                },
            );
        });
    })
    .detach();
}
