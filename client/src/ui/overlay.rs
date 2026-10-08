//! What floats over the window: dialogs on a scrim, menus at the pointer, toasts. Any view
//! reaches them through `ui::overlay::open_dialog` and `open_menu`, which find the root window
//! view by a global handle.

use super::Root;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{Theme, current, px, radius};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, anchored,
    deferred, div,
};
use std::rc::Rc;

pub struct Toast {
    pub id: usize,
    pub text: SharedString,
}

/// Emitted by a dialog or menu to close itself.
pub struct Dismiss;

enum Layer {
    Dialog(AnyView),
    Menu { at: Point<Pixels>, anchor: gpui::Anchor, view: AnyView },
}

#[derive(Default)]
pub struct Overlay {
    layers: Vec<Layer>,
}

impl Overlay {
    /// Closes the top layer; false when there was none.
    pub fn close_top(&mut self) -> bool {
        self.layers.pop().is_some()
    }

    pub fn has_menu(&self) -> bool {
        self.layers.iter().any(|l| matches!(l, Layer::Menu { .. }))
    }
}

pub struct RootHandle(pub WeakEntity<Root>);

impl Global for RootHandle {}

fn with_root(cx: &mut App, f: impl FnOnce(&mut Root, &mut Context<Root>)) {
    let Some(root) = cx.try_global::<RootHandle>().and_then(|h| h.0.upgrade()) else { return };
    root.update(cx, f);
}

pub fn toast(text: impl Into<SharedString>, cx: &mut App) {
    let text = text.into();
    with_root(cx, |root, cx| root.toast(text, cx));
}

/// Shows a dialog over everything. It closes on Escape, a click on the scrim, or `Dismiss`.
pub fn open_dialog<V: Render + EventEmitter<Dismiss> + Focusable>(view: Entity<V>, window: &mut Window, cx: &mut App) {
    view.focus_handle(cx).focus(window, cx);
    let any: AnyView = view.clone().into();
    with_root(cx, move |root, cx| {
        cx.subscribe(&view, |root: &mut Root, v, _: &Dismiss, cx| {
            let id = v.entity_id();
            root.overlay.layers.retain(|l| match l {
                Layer::Dialog(d) | Layer::Menu { view: d, .. } => d.entity_id() != id,
            });
            cx.notify();
        })
        .detach();
        root.overlay.layers.push(Layer::Dialog(any));
        cx.notify();
    });
}

/// Shows a menu with its corner at `at`. It closes on a click anywhere else.
pub fn open_menu<V: Render + EventEmitter<Dismiss>>(view: Entity<V>, at: Point<Pixels>, cx: &mut App) {
    show_menu(view, at, gpui::Anchor::TopLeft, cx);
}

/// A menu that grows upward from `at`, for buttons along the bottom of the window, where the
/// button's own tooltip would otherwise sit on the first item.
pub fn open_menu_above<V: Render + EventEmitter<Dismiss>>(view: Entity<V>, at: Point<Pixels>, cx: &mut App) {
    show_menu(view, at - gpui::point(px(0.), px(10.)), gpui::Anchor::BottomLeft, cx);
}

fn show_menu<V: Render + EventEmitter<Dismiss>>(view: Entity<V>, at: Point<Pixels>, anchor: gpui::Anchor, cx: &mut App) {
    let any: AnyView = view.clone().into();
    with_root(cx, move |root, cx| {
        cx.subscribe(&view, |root: &mut Root, v, _: &Dismiss, cx| {
            let id = v.entity_id();
            root.overlay.layers.retain(|l| match l {
                Layer::Dialog(d) | Layer::Menu { view: d, .. } => d.entity_id() != id,
            });
            cx.notify();
        })
        .detach();
        root.overlay.layers.retain(|l| !matches!(l, Layer::Menu { .. }));
        root.overlay.layers.push(Layer::Menu { at, anchor, view: any });
        cx.notify();
    });
}

impl Root {
    pub fn render_overlay(&self, t: &Theme, cx: &mut Context<Root>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let top = self.overlay.layers.len();
        for (i, layer) in self.overlay.layers.iter().enumerate() {
            match layer {
                Layer::Dialog(view) => out.push(
                    div()
                        .id(("scrim", i))
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(px(24.))
                        .bg(t.scrim)
                        .occlude()
                        .when(i + 1 == top, |d| {
                            d.on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|root, _, _, cx| {
                                    root.overlay.close_top();
                                    cx.notify();
                                }),
                            )
                        })
                        .child(
                            div()
                                .id(("dialog", i))
                                .max_h_full()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(view.clone()),
                        )
                        .into_any_element(),
                ),
                Layer::Menu { at, anchor, view } => {
                    out.push(
                        deferred(
                            div()
                                .id(("menu-catcher", i))
                                .absolute()
                                .inset_0()
                                .occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|root, _, _, cx| {
                                        root.overlay.layers.retain(|l| !matches!(l, Layer::Menu { .. }));
                                        cx.notify();
                                    }),
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(|root, _, _, cx| {
                                        root.overlay.layers.retain(|l| !matches!(l, Layer::Menu { .. }));
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    anchored().anchor(*anchor).position(*at).snap_to_window_with_margin(px(8.)).child(
                                        div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(view.clone()),
                                    ),
                                ),
                        )
                        .with_priority(1)
                        .into_any_element(),
                    )
                }
            }
        }
        out
    }
}

/// The card a dialog sits on.
pub fn dialog_card(t: &Theme, width: f32) -> gpui::Div {
    div()
        .w(px(width))
        .max_w_full()
        .flex()
        .flex_col()
        .rounded(px(radius::PANE + 2.))
        .bg(t.pane)
        .border_1()
        .border_color(t.stroke_strong)
        .shadow_lg()
        .overflow_hidden()
}

/// A menu's surface.
pub fn menu_card(t: &Theme) -> gpui::Div {
    div()
        .min_w(px(200.))
        .max_w(px(320.))
        .flex()
        .flex_col()
        .p(px(5.))
        .rounded(px(radius::CONTROL + 2.))
        .bg(t.popover)
        .border_1()
        .border_color(t.stroke_strong)
        .shadow_lg()
}

pub fn menu_item(
    id: impl Into<gpui::ElementId>,
    glyph: Option<&'static str>,
    text: impl Into<SharedString>,
    danger: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let color = if danger { t.critical } else { t.text };
    let hover = if danger { t.tint(t.critical) } else { t.layer_hover };
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap(px(10.))
        .h(px(32.))
        .px(px(10.))
        .rounded(px(radius::INNER))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .when_some(glyph, |d, g| d.child(icon(g, 15., if danger { t.critical } else { t.text2 })))
        .child(body(text, color))
}

pub fn menu_rule(t: &Theme) -> gpui::Div {
    div().my(px(4.)).mx(px(6.)).h(px(1.)).bg(t.stroke)
}

// The general-purpose prompt: a title, some text, a few fields, OK and Cancel.

pub enum Field {
    Text {
        label: &'static str,
        value: String,
        placeholder: &'static str,
        secret: bool,
        multiline: bool,
        max: usize,
    },
    Choice {
        label: &'static str,
        options: Vec<(String, String)>,
        picked: usize,
    },
    /// A one-line field that may be left empty.
    Optional {
        label: &'static str,
        value: String,
        placeholder: &'static str,
        max: usize,
    },
}

type OnOk = Rc<dyn Fn(Vec<String>, &mut Window, &mut App) -> Option<String>>;

pub struct Ask {
    focus: FocusHandle,
    title: SharedString,
    text: Option<SharedString>,
    ok: SharedString,
    danger: bool,
    fields: Vec<AskField>,
    error: Option<String>,
    on_ok: OnOk,
}

enum AskField {
    Text { label: &'static str, field: Entity<TextField>, required: bool },
    Choice { label: &'static str, options: Vec<(String, String)>, picked: usize },
}

impl EventEmitter<Dismiss> for Ask {}

impl Focusable for Ask {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.fields
            .iter()
            .find_map(|f| match f {
                AskField::Text { field, .. } => Some(field.focus_handle(cx)),
                _ => None,
            })
            .unwrap_or_else(|| self.focus.clone())
    }
}

impl Ask {
    /// `on_ok` gets the fields' values in order; returning `Some(error)` keeps the dialog open
    /// with that message.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        title: impl Into<SharedString>,
        text: Option<String>,
        ok: impl Into<SharedString>,
        danger: bool,
        fields: Vec<Field>,
        window: &mut Window,
        cx: &mut App,
        on_ok: impl Fn(Vec<String>, &mut Window, &mut App) -> Option<String> + 'static,
    ) {
        let (title, ok) = (title.into(), ok.into());
        let view = cx.new(|cx: &mut Context<Ask>| {
            let fields = fields
                .into_iter()
                .map(|f| match f {
                    Field::Text { label, value, placeholder, secret, multiline, max } => {
                        let field = cx.new(|cx| {
                            let mut f = TextField::new(cx, multiline, max).placeholder(placeholder);
                            if multiline {
                                f = f.lines(3, 10);
                            }
                            f.set_masked(secret);
                            f.set_text(&value, cx);
                            f
                        });
                        cx.subscribe_in(&field, window, |this: &mut Ask, _, ev: &TextFieldEvent, window, cx| {
                            if let TextFieldEvent::Submit = ev {
                                this.confirm(window, cx);
                            }
                            this.error = None;
                        })
                        .detach();
                        AskField::Text { label, field, required: !secret }
                    }
                    Field::Choice { label, options, picked } => AskField::Choice { label, options, picked },
                    Field::Optional { label, value, placeholder, max } => {
                        let field = cx.new(|cx| {
                            let mut f = TextField::new(cx, false, max).placeholder(placeholder);
                            f.set_text(&value, cx);
                            f
                        });
                        cx.subscribe_in(&field, window, |this: &mut Ask, _, ev: &TextFieldEvent, window, cx| {
                            if let TextFieldEvent::Submit = ev {
                                this.confirm(window, cx);
                            }
                        })
                        .detach();
                        AskField::Text { label, field, required: false }
                    }
                })
                .collect();
            Ask { focus: cx.focus_handle(), title, text: text.map(Into::into), ok, danger, fields, error: None, on_ok: Rc::new(on_ok) }
        });
        open_dialog(view, window, cx);
    }

    /// A yes-or-no question.
    pub fn confirm_action(
        title: impl Into<SharedString>,
        text: impl Into<String>,
        ok: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
        on_ok: impl Fn(&mut Window, &mut App) + 'static,
    ) {
        Ask::open(title, Some(text.into()), ok, true, Vec::new(), window, cx, move |_, w, cx| {
            on_ok(w, cx);
            None
        });
    }

    fn values(&self, cx: &App) -> Vec<String> {
        self.fields
            .iter()
            .map(|f| match f {
                AskField::Text { field, .. } => field.read(cx).text(),
                AskField::Choice { options, picked, .. } => options.get(*picked).map(|o| o.0.clone()).unwrap_or_default(),
            })
            .collect()
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let values = self.values(cx);
        let missing =
            self.fields.iter().zip(&values).any(|(f, v)| matches!(f, AskField::Text { required: true, .. }) && v.trim().is_empty());
        let single_field_required = self.fields.len() == 1 && missing;
        if single_field_required {
            self.error = Some(tr!("This can't be empty.", "Isso não pode ficar vazio.").into());
            cx.notify();
            return;
        }
        let on_ok = self.on_ok.clone();
        match on_ok(values, window, cx) {
            Some(err) => {
                self.error = Some(err);
                cx.notify();
            }
            None => cx.emit(Dismiss),
        }
    }
}

impl Render for Ask {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let mut content = div().flex().flex_col().gap(px(14.)).p(px(22.)).child(title(self.title.clone(), t.text));
        if let Some(text) = &self.text {
            content = content.child(body(text.clone(), t.text2));
        }
        for (i, f) in self.fields.iter().enumerate() {
            content = content.child(match f {
                AskField::Text { label: l, field, .. } => div().flex().flex_col().gap(px(6.)).child(label(*l, &t)).child(field.clone()),
                AskField::Choice { label: l, options, picked } => {
                    let choices: Vec<(usize, SharedString)> = options.iter().enumerate().map(|(i, o)| (i, o.1.clone().into())).collect();
                    let id: &'static str = ["ask-choice-0", "ask-choice-1", "ask-choice-2", "ask-choice-3"][i.min(3)];
                    div().flex().flex_col().gap(px(6.)).child(label(*l, &t)).child(segmented(
                        id,
                        choices,
                        *picked,
                        &t,
                        cx,
                        move |this: &mut Ask, v, _, cx| {
                            if let Some(AskField::Choice { picked, .. }) = this.fields.get_mut(i) {
                                *picked = v;
                            }
                            cx.notify();
                        },
                    ))
                }
            });
        }
        if let Some(e) = &self.error {
            content = content.child(caption(e.clone(), t.critical));
        }
        dialog_card(&t, 440.).key_context("Ask").track_focus(&self.focus).child(content).child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .px(px(22.))
                .py(px(14.))
                .bg(t.layer)
                .border_t_1()
                .border_color(t.stroke)
                .child(
                    button("ask-cancel", tr!("Cancel", "Cancelar"), Kind::Standard, &t)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                )
                .child(
                    button("ask-ok", self.ok.clone(), if self.danger { Kind::Danger } else { Kind::Primary }, &t)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
        )
    }
}
