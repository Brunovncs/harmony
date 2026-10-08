//! The building blocks every screen uses: text styles, panes, buttons, chips, the switch, a row
//! of choices, sliders, keycaps, avatars and the status dot.

use crate::theme::{FONT, MONO, Theme, px, radius, text};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, Context, Div, DragMoveEvent, ElementId, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, canvas, div,
};
use std::cell::Cell;
use std::rc::Rc;

pub fn label(s: impl Into<SharedString>, t: &Theme) -> Div {
    // Small caps in mono: the section labels Texel uses.
    div()
        .font_family(MONO)
        .text_size(px(text::LABEL.0))
        .line_height(px(text::LABEL.1))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text3)
        .child(s.into().to_uppercase())
}

pub fn caption(s: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_size(px(text::CAPTION.0)).line_height(px(text::CAPTION.1)).text_color(color).child(s.into())
}

pub fn body(s: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_size(px(text::BODY.0)).line_height(px(text::BODY.1)).text_color(color).child(s.into())
}

pub fn title(s: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_size(px(text::TITLE.0)).line_height(px(text::TITLE.1)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(s.into())
}

pub fn display(s: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_size(px(text::DISPLAY.0)).line_height(px(text::DISPLAY.1)).font_weight(FontWeight::BOLD).text_color(color).child(s.into())
}

pub fn mono(s: impl Into<SharedString>, color: Hsla) -> Div {
    div().font_family(MONO).text_size(px(text::CAPTION.0)).line_height(px(text::CAPTION.1)).text_color(color).child(s.into())
}

pub fn icon(name: &'static str, size: f32, color: Hsla) -> Div {
    div().flex_none().size(px(size)).child(gpui::img(crate::icons::image(name, color)).size(px(size)))
}

/// A pane: one of the rounded cards the window is laid out in.
pub fn pane(t: &Theme) -> Div {
    div().flex().flex_col().min_h(px(0.)).rounded(px(radius::PANE)).bg(t.pane).border_1().border_color(t.stroke).overflow_hidden()
}

/// A hint shown when the pointer rests on something.
pub fn tip(text: impl Into<SharedString>, t: &Theme) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    let t = *t;
    move |_, cx| cx.new(|_| Tip { text: text.clone(), theme: t }).into()
}

struct Tip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .font_family(FONT)
            .max_w(px(300.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(t.popover)
            .border_1()
            .border_color(t.stroke_strong)
            .shadow_md()
            .child(caption(self.text.clone(), t.text))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Standard,
    Subtle,
    Danger,
}

pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: Kind, t: &Theme) -> Stateful<Div> {
    button_base(id, kind, t).child(label.into())
}

/// A button with an icon before its label.
pub fn icon_label_button(
    id: impl Into<ElementId>,
    glyph: &'static str,
    label: impl Into<SharedString>,
    kind: Kind,
    t: &Theme,
) -> Stateful<Div> {
    let fg = fg(kind, t);
    button_base(id, kind, t).pl(px(12.)).child(icon(glyph, 15., fg)).child(label.into())
}

fn fg(kind: Kind, t: &Theme) -> Hsla {
    match kind {
        Kind::Primary => t.on_accent,
        Kind::Danger => gpui::white(),
        _ => t.text,
    }
}

fn button_base(id: impl Into<ElementId>, kind: Kind, t: &Theme) -> Stateful<Div> {
    let (bg, hover, border) = match kind {
        Kind::Primary => (t.accent, t.accent.opacity(0.86), t.accent),
        Kind::Standard => (t.control, t.control_hover, t.stroke_strong),
        Kind::Subtle => (gpui::transparent_black(), t.control, gpui::transparent_black()),
        Kind::Danger => (t.critical, t.critical.opacity(0.86), t.critical),
    };
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .h(px(36.))
        .px(px(14.))
        .rounded(px(radius::CONTROL))
        .bg(bg)
        .border_1()
        .border_color(border)
        .text_size(px(text::BODY.0))
        .font_weight(if matches!(kind, Kind::Primary | Kind::Danger) { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .text_color(fg(kind, t))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .active(|s| s.opacity(0.82))
}

/// A square button holding only an icon.
pub fn icon_button(id: impl Into<ElementId>, glyph: &'static str, t: &Theme) -> Stateful<Div> {
    tool_button(id, glyph, false, t.text2, t)
}

/// An icon button that can be switched on, like mute or the camera.
pub fn tool_button(id: impl Into<ElementId>, glyph: &'static str, on: bool, color: Hsla, t: &Theme) -> Stateful<Div> {
    let hover = t.control_hover;
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(32.))
        .rounded(px(8.))
        .cursor_pointer()
        .when(on, |d| d.bg(t.tint(color)))
        .when(!on, |d| d.hover(move |s| s.bg(hover)))
        .active(|s| s.opacity(0.75))
        .child(icon(glyph, 17., color))
}

/// A small label with a tinted background: a role, a state.
pub fn chip(s: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(18.))
        .px(px(6.))
        .rounded(px(5.))
        .bg(bg)
        .font_family(MONO)
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(s.into().to_uppercase())
}

/// A dot that glows in its colour: connected, speaking, live.
pub fn status_dot(color: Hsla, glow: bool) -> Div {
    div().flex_none().size(px(8.)).rounded_full().bg(color).when(glow, |d| {
        d.shadow(vec![gpui::BoxShadow {
            color: color.opacity(0.6),
            offset: gpui::point(px(0.), px(0.)),
            blur_radius: px(8.),
            spread_radius: px(0.),
            inset: false,
        }])
    })
}

/// A toggle switch.
pub fn switch(on: bool, t: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .w(px(36.))
        .h(px(20.))
        .rounded(px(10.))
        .px(px(3.))
        .when(on, |d| d.justify_end().bg(t.accent))
        .when(!on, |d| d.bg(t.well).border_1().border_color(t.stroke_strong))
        .child(div().size(px(14.)).rounded_full().bg(if on { t.on_accent } else { t.text2 }))
}

/// A row of choices, one of them picked: the segmented control from Texel.
pub fn segmented<T: Copy + PartialEq + 'static, V: 'static>(
    id: &'static str,
    choices: Vec<(T, SharedString)>,
    picked: T,
    t: &Theme,
    cx: &mut Context<V>,
    on_pick: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Div {
    let on_pick = Rc::new(on_pick);
    let mut d = div().flex().gap(px(2.)).p(px(3.)).rounded(px(radius::CONTROL)).bg(t.well).border_1().border_color(t.stroke);
    for (i, (value, label)) in choices.into_iter().enumerate() {
        let on = value == picked;
        let on_pick = on_pick.clone();
        let hover = t.text;
        d = d.child(
            div()
                .id((id, i))
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .h(px(28.))
                .px(px(10.))
                .rounded(px(radius::INNER))
                .cursor_pointer()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .when(on, |d| d.bg(t.accent_soft).text_color(t.accent))
                .when(!on, |d| d.text_color(t.text2).hover(move |s| s.text_color(hover)))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| on_pick(this, value, window, cx))),
        );
    }
    d
}

pub fn keycap(s: impl Into<SharedString>, t: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .px(px(6.))
        .min_w(px(22.))
        .h(px(22.))
        .rounded(px(5.))
        .bg(t.control)
        .border_1()
        .border_b_2()
        .border_color(t.stroke_strong)
        .font_family(MONO)
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text)
        .child(s.into())
}

/// A round picture, or the person's initials on a colour of their own when there is none.
pub fn avatar(name: &str, image: Option<std::sync::Arc<gpui::RenderImage>>, size: f32, ring: Option<Hsla>) -> Div {
    let initials: String = name.chars().filter(|c| c.is_alphanumeric()).take(2).collect::<String>().to_uppercase();
    let hue = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    let bg = gpui::hsla(hue as f32 / 360., 0.45, 0.42, 1.);
    let inner = match image {
        Some(img) => div().size_full().rounded_full().overflow_hidden().child(gpui::img(img).size_full().rounded_full()),
        None => div()
            .size_full()
            .rounded_full()
            .bg(bg)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px((size * 0.38).max(9.)))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(gpui::white())
            .child(initials),
    };
    div().flex_none().relative().size(px(size)).rounded_full().child(inner).when_some(ring, |d, c| d.child(ring_over(c, size / 2., 3.5)))
}

/// A 2 px ring in `color` floating over its parent, which must be `relative`: turning it on moves
/// nothing. `outset` puts it that far outside the parent's edge, a gap included.
fn ring_over(color: Hsla, radius: f32, outset: f32) -> Div {
    div()
        .absolute()
        .top(px(-outset))
        .left(px(-outset))
        .right(px(-outset))
        .bottom(px(-outset))
        .rounded(px(radius + outset))
        .border_2()
        .border_color(color)
}

/// Lights up a card or a video tile while its person speaks: its edge and a ring just inside it
/// in the success colour, and a soft glow. The element must be `relative` with a 1 px border.
pub fn speaking<E: Styled + ParentElement>(el: E, on: bool, radius: f32, t: &Theme) -> E {
    if !on {
        return el;
    }
    el.border_color(t.success).child(ring_over(t.success, radius - 1., 0.)).shadow(vec![gpui::BoxShadow {
        color: t.success.opacity(0.25),
        offset: gpui::point(px(0.), px(0.)),
        blur_radius: px(14.),
        spread_radius: px(0.),
        inset: false,
    }])
}

/// A horizontal slider. `value` is 0..=1 along the track; `on_change` gets the new value while
/// dragging. `mark` draws a tick, for 100% on a volume that goes past it.
pub struct Slider {
    pub value: f32,
    pub mark: Option<f32>,
    pub color: Hsla,
}

/// What a slider drag carries: which slider it is. GPUI keeps it for the length of the drag, so
/// the drag survives the redraws its own changes cause and carries on past the track's ends.
struct SliderDrag(ElementId);

impl Render for SliderDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub fn slider<V: 'static>(
    id: impl Into<ElementId>,
    s: Slider,
    t: &Theme,
    cx: &mut Context<V>,
    on_change: impl Fn(&mut V, f32, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let id = id.into();
    let bounds: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
    let on_change = Rc::new(on_change);
    let fraction = |b: Bounds<Pixels>, x: Pixels| -> f32 {
        let w: f32 = b.size.width.into();
        if w <= 0. {
            return 0.;
        }
        (f32::from(x - b.origin.x) / w).clamp(0., 1.)
    };
    let value = s.value.clamp(0., 1.);
    let (b1, b2) = (bounds.clone(), bounds);
    let (c1, c2) = (on_change.clone(), on_change);
    let mine = id.clone();
    div()
        .id(id.clone())
        .relative()
        .h(px(20.))
        .flex()
        .items_center()
        .cursor_pointer()
        .child(canvas(move |b, _, _| b2.set(b), |_, _, _, _| {}).absolute().inset_0())
        .child(
            div()
                .relative()
                .w_full()
                .h(px(4.))
                .rounded(px(2.))
                .bg(t.well)
                .child(div().absolute().left_0().top_0().h_full().rounded(px(2.)).bg(s.color).w(gpui::relative(value)))
                .when_some(s.mark, |d, m| {
                    d.child(div().absolute().top(px(-3.)).h(px(10.)).w(px(2.)).rounded(px(1.)).bg(t.text3).left(gpui::relative(m)))
                }),
        )
        .child(
            div()
                .absolute()
                .top(px(3.))
                .size(px(14.))
                .ml(px(-7.))
                .left(gpui::relative(value))
                .rounded_full()
                .bg(gpui::white())
                .border_2()
                .border_color(s.color)
                .shadow_sm(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                c1(this, fraction(b1.get(), e.position.x), cx);
                cx.stop_propagation();
            }),
        )
        .on_drag(SliderDrag(id), |d, _, _, cx| cx.new(|_| SliderDrag(d.0.clone())))
        .on_drag_move(cx.listener(move |this, e: &DragMoveEvent<SliderDrag>, _, cx| {
            if e.drag(cx).0 == mine {
                c2(this, fraction(e.bounds, e.event.position.x), cx);
            }
        }))
}
