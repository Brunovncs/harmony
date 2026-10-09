//! Your camera before anyone sees it: the live preview and the background picker that settings
//! and the camera dialog share, and that dialog, which opens when you turn the camera on.

use crate::core::settings::Background;
use crate::media::camera::{self, Capture};
use crate::media::video::{Tile, TileKind};
use crate::media::voice::Voice;
use crate::prefs::{prefs, set_prefs};
use crate::theme::{Theme, current, px, radius};
use crate::ui::overlay::{self, Dismiss, dialog_card};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled, StyledImage, Window, div, img, surface,
};
use std::rc::Rc;
use std::sync::Arc;

/// A live, mirrored preview of a camera. While the call is sending your camera it shows the
/// call's own tile instead, so the device is never opened twice.
#[derive(Default)]
pub struct Preview {
    capture: Option<Capture>,
    tile: Option<Entity<Tile>>,
}

impl Preview {
    pub fn start<V: 'static>(&mut self, voice: Option<&Entity<Voice>>, device: &str, bg: Background, cx: &mut Context<V>) {
        if self.tile.is_some() {
            return;
        }
        let tile = match voice.and_then(|v| v.read(cx).shares.camera.as_ref().map(|c| c.tile.clone())) {
            Some(shared) => shared,
            None => {
                let capture = Capture::start(device, bg);
                let tile = Tile::local(0, -1, TileKind::Camera, capture.preview.clone(), cx);
                self.capture = Some(capture);
                tile
            }
        };
        cx.observe(&tile, |_, _, cx| cx.notify()).detach();
        self.tile = Some(tile);
    }

    /// Lets go of the device, if this preview opened it.
    pub fn stop(&mut self, cx: &mut App) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
            if let Some(t) = &self.tile {
                t.update(cx, |t, cx| t.close(cx));
            }
        }
        self.tile = None;
    }

    pub fn set_background(&self, bg: Background) {
        if let Some(c) = &self.capture {
            c.set_background(bg);
        }
    }

    /// The picture, or why there is none, filling a rounded frame the caller sizes.
    pub fn frame(&self, t: &Theme, cx: &App) -> Div {
        let frame = self.tile.as_ref().and_then(|t| t.read(cx).frame.clone());
        let error = self.capture.as_ref().and_then(|c| c.error.lock().clone());
        div()
            .rounded(px(radius::CARD + 2.))
            .overflow_hidden()
            .bg(t.stage)
            .border_1()
            .border_color(t.stroke)
            .flex()
            .items_center()
            .justify_center()
            .child(match (frame, error) {
                (Some(f), _) => {
                    surface(f).size_full().rounded(px(radius::CARD + 1.)).object_fit(gpui::ObjectFit::Contain).into_any_element()
                }
                (None, Some(e)) => div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .child(icon("camera-off", 26., t.text3))
                    .child(caption(e, t.text3))
                    .into_any_element(),
                (None, None) => caption(tr!("Starting the camera…", "Ligando a câmera…"), t.text3).into_any_element(),
            })
    }
}

thread_local! {
    static THUMBS: std::cell::RefCell<std::collections::HashMap<String, Option<Arc<gpui::Image>>>> = Default::default();
}

fn cached_thumb(bg: &Background) -> Option<Arc<gpui::Image>> {
    let key = serde_json::to_string(bg).unwrap_or_default();
    THUMBS.with(|t| t.borrow_mut().entry(key).or_insert_with(|| camera::thumbnail(bg)).clone())
}

fn builtin_title(name: &str, english: &'static str) -> &'static str {
    match name {
        "dusk" => tr!("Dusk", "Entardecer"),
        "studio" => tr!("Studio", "Estúdio"),
        "ocean" => tr!("Ocean", "Oceano"),
        _ => english,
    }
}

/// The backgrounds to pick from as thumbnails: none, blur, the built-in pictures, yours, and a
/// tile to add a picture. `compact` draws them small, named by a tooltip instead of a caption.
pub fn backgrounds<V: 'static>(
    picked: &Background,
    compact: bool,
    t: &Theme,
    cx: &mut Context<V>,
    on_pick: impl Fn(&mut V, Background, &mut Context<V>) + 'static,
) -> Div {
    let on_pick = Rc::new(on_pick);
    let strength = if let Background::Blur { strength } = picked { *strength } else { 12 };
    let mut options: Vec<(Background, SharedString, Option<Arc<gpui::Image>>)> =
        vec![(Background::None, tr!("None", "Nenhum").into(), None), (Background::Blur { strength }, tr!("Blur", "Desfoque").into(), None)];
    for (name, title, _) in camera::BUILTIN {
        let bg = Background::Builtin { name: name.into() };
        let thumb = cached_thumb(&bg);
        options.push((bg, builtin_title(name, title).into(), thumb));
    }
    for path in &prefs(cx).custom_backgrounds {
        let bg = Background::Image { path: path.clone() };
        let thumb = cached_thumb(&bg);
        let name = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        options.push((bg, name.into(), thumb));
    }
    let (w, h, glyph) = if compact { (72., 41., 17.) } else { (104., 58., 20.) };
    let tile = |id: gpui::ElementId, name: SharedString, face: Div, on: bool| {
        let d = div().id(id).w(px(w)).flex().flex_col().gap(px(4.)).cursor_pointer().child(face.h(px(h)).rounded(px(radius::CONTROL)));
        if compact {
            d.tooltip(tip(name, t))
        } else {
            d.child(div().text_center().truncate().text_size(px(12.)).text_color(if on { t.text } else { t.text2 }).child(name))
        }
    };
    let mut grid = div().flex().flex_wrap().gap(px(8.));
    for (i, (bg, name, thumb)) in options.into_iter().enumerate() {
        let on = match (&bg, picked) {
            (Background::Blur { .. }, Background::Blur { .. }) => true,
            (a, b) => a == b,
        };
        let hover = t.stroke_strong;
        let face = div()
            .overflow_hidden()
            .border_2()
            .border_color(if on { t.accent } else { t.stroke })
            .when(!on, |d| d.hover(move |s| s.border_color(hover)))
            .bg(t.stage)
            .flex()
            .items_center()
            .justify_center()
            .child(match (&bg, thumb) {
                (_, Some(i)) => img(i).size_full().rounded(px(radius::CONTROL - 2.)).object_fit(gpui::ObjectFit::Cover).into_any_element(),
                (Background::None, _) => icon("ban", glyph, t.text3).into_any_element(),
                (Background::Blur { .. }, _) => icon("blur", glyph + 2., t.text2).into_any_element(),
                _ => icon("image", glyph, t.text3).into_any_element(),
            });
        let on_pick = on_pick.clone();
        grid = grid.child(tile(("bg", i).into(), name, face, on).on_click(cx.listener(move |v, _, _, cx| on_pick(v, bg.clone(), cx))));
    }
    let add = div()
        .border_2()
        .border_dashed()
        .border_color(t.stroke_strong)
        .flex()
        .items_center()
        .justify_center()
        .child(icon("plus", glyph, t.text2));
    grid.child(tile("bg-add".into(), tr!("Your picture", "Sua imagem").into(), add, false).on_click(cx.listener(move |_, _, _, cx| {
        let on_pick = on_pick.clone();
        add_picture(cx, move |v, bg, cx| on_pick(v, bg, cx));
    })))
}

/// Asks for a picture, keeps it with your backgrounds and hands it on.
fn add_picture<V: 'static>(cx: &mut Context<V>, then: impl FnOnce(&mut V, Background, &mut Context<V>) + 'static) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(tr!("Use as background", "Usar como fundo").into()),
    });
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(paths))) = paths.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let path = path.to_string_lossy().to_string();
        let _ = this.update(cx, |v, cx| {
            set_prefs(cx, |p| {
                p.custom_backgrounds.retain(|x| *x != path);
                p.custom_backgrounds.insert(0, path.clone());
                p.custom_backgrounds.truncate(8);
            });
            then(v, Background::Image { path }, cx);
        });
    })
    .detach();
}

/// Turning the camera on: a look at yourself first, with the camera and background to use.
pub struct CameraDialog {
    focus: FocusHandle,
    voice: Entity<Voice>,
    cameras: Option<Vec<camera::Device>>,
    device: String,
    background: Background,
    preview: Preview,
    on_started: Rc<dyn Fn(&mut App)>,
}

impl EventEmitter<Dismiss> for CameraDialog {}

impl Focusable for CameraDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CameraDialog {
    pub fn open(voice: Entity<Voice>, window: &mut Window, cx: &mut App, on_started: impl Fn(&mut App) + 'static) {
        let view = cx.new(|cx| {
            let (device, background) = (prefs(cx).voice_camera_id.clone(), prefs(cx).camera_background.clone());
            let mut preview = Preview::default();
            preview.start::<Self>(None, &device, background.clone(), cx);
            cx.spawn(async move |this, cx| {
                let list = cx.background_executor().spawn(async move { camera::devices() }).await;
                let _ = this.update(cx, |d: &mut Self, cx| {
                    // A saved camera that is gone falls back to the first, as the capture does.
                    if !list.iter().any(|c| c.id == d.device)
                        && let Some(first) = list.first()
                    {
                        d.device = first.id.clone();
                    }
                    d.cameras = Some(list);
                    cx.notify();
                });
            })
            .detach();
            CameraDialog { focus: cx.focus_handle(), voice, cameras: None, device, background, preview, on_started: Rc::new(on_started) }
        });
        overlay::open_dialog(view, window, cx);
    }

    fn set_device(&mut self, id: String, cx: &mut Context<Self>) {
        if id == self.device {
            return;
        }
        self.device = id;
        // The new capture waits for this one to let go of its device before opening its own.
        self.preview.stop(cx);
        self.preview.start(None, &self.device, self.background.clone(), cx);
        cx.notify();
    }

    fn set_background(&mut self, bg: Background, cx: &mut Context<Self>) {
        self.preview.set_background(bg.clone());
        self.background = bg;
        cx.notify();
    }

    fn turn_on(&mut self, cx: &mut Context<Self>) {
        if self.cameras.as_ref().is_some_and(|c| c.is_empty()) {
            return;
        }
        let (device, bg) = (self.device.clone(), self.background.clone());
        set_prefs(cx, |p| {
            p.voice_camera_id = device.clone();
            p.camera_background = bg.clone();
        });
        // Hands the device over: the call's capture opens it once the preview's has closed it.
        self.preview.stop(cx);
        self.voice.update(cx, |v, cx| v.start_camera(&device, bg, cx));
        (self.on_started)(cx);
        cx.emit(Dismiss);
    }
}

impl Render for CameraDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let cameras = self.cameras.clone().unwrap_or_default();
        let none = self.cameras.as_ref().is_some_and(|c| c.is_empty());
        let picked_name = cameras
            .iter()
            .find(|d| d.id == self.device)
            .or_else(|| cameras.first())
            .map(|d| d.name.clone())
            .unwrap_or_else(|| if none { tr!("No camera found", "Nenhuma câmera encontrada") } else { "…" }.into());
        let options: Vec<(String, String)> = cameras.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
        let field =
            |name: &'static str, control: gpui::AnyElement| div().flex().flex_col().gap(px(6.)).child(label(name, &t)).child(control);
        let width = 560.;
        dialog_card(&t, width)
            .track_focus(&self.focus)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .p(px(22.))
                    .child(div().flex().flex_col().gap(px(2.)).child(title(tr!("Turn on your camera", "Ligar a câmera"), t.text)).child(
                        caption(
                            tr!("Check how you look before anyone sees you.", "Veja como você aparece antes que os outros vejam."),
                            t.text2,
                        ),
                    ))
                    .child(self.preview.frame(&t, cx).w_full().h(px((width - 44.) * 9. / 16.)))
                    .child(field(
                        tr!("Camera", "Câmera"),
                        crate::ui::settings::select("camera-pick", picked_name, options, &t, cx, |d: &mut Self, id, cx| {
                            d.set_device(id, cx)
                        })
                        .into_any_element(),
                    ))
                    .child(field(
                        tr!("Background", "Fundo"),
                        backgrounds(&self.background, true, &t, cx, |d: &mut Self, bg, cx| d.set_background(bg, cx)).into_any_element(),
                    )),
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
                    .child(div().flex_1().child(caption(tr!("Only you see it mirrored.", "Só você vê a imagem espelhada."), t.text2)))
                    .child(
                        button("camera-cancel", tr!("Cancel", "Cancelar"), Kind::Standard, &t)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                    )
                    .child(
                        icon_label_button("camera-start", "camera", tr!("Turn on camera", "Ligar a câmera"), Kind::Primary, &t)
                            .when(none, |b| b.opacity(0.5))
                            .on_click(cx.listener(|d, _, _, cx| d.turn_on(cx))),
                    ),
            )
    }
}
