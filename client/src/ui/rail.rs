//! The rail down the left edge: every saved server as a tile (its picture, or its initials), the
//! one in use marked, and a + to add another. A click switches servers, a drag reorders them, and
//! a right-click copies the address or leaves the server.

use super::overlay::{self, Ask};
use super::server::sidebar::{Menu, MenuEntry};
use super::{Root, Screen};
use crate::core::api::Api;
use crate::core::cache::Cache;
use crate::core::settings::{SavedServer, same_server};
use crate::core::{self};
use crate::prefs::{prefs, set_prefs};
use crate::session::decode_picture;
use crate::theme::{FONT, GUTTER, Theme, current, px, radius};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AppContext, ClipboardItem, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, RenderImage, SharedString, StatefulInteractiveElement, Styled, Window, div, img,
};
use std::collections::HashSet;
use std::sync::Arc;

const TILE: f32 = 40.;
/// Server pictures are decoded at this, enough for the tile at the largest UI size.
const ICON: (u32, u32) = (96, 96);

impl Root {
    fn adding(&self) -> bool {
        matches!(self.screen, Screen::Connect { adding: true, .. })
    }

    pub(super) fn render_rail(&mut self, t: &Theme, cx: &mut Context<Root>) -> gpui::Div {
        let p = prefs(cx);
        let adding = self.adding();
        let active = if adding { None } else { p.active_server() };
        let servers = p.saved_servers.clone();
        let mut list = div().id("rail-servers").flex().flex_col().gap(px(8.)).py(px(2.)).min_h(px(0.)).overflow_y_scroll();
        for (i, s) in servers.into_iter().enumerate() {
            list = list.child(self.rail_tile(i, s, active == Some(i), t, cx));
        }
        let plus_hover = t.control_hover;
        pane(t)
            .w(px(TILE + 20.))
            .flex_none()
            .my(px(GUTTER))
            .ml(px(GUTTER))
            .py(px(GUTTER))
            .gap(px(8.))
            .child(list)
            .child(div().mx(px(14.)).h(px(1.)).flex_none().bg(t.stroke))
            .child(
                rail_slot("rail-add", adding, t)
                    .child(
                        div()
                            .size(px(TILE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(radius::CARD))
                            .border_1()
                            .border_dashed()
                            .when(adding, |d| d.bg(t.accent_soft).border_color(t.accent.opacity(0.6)))
                            .when(!adding, |d| d.bg(t.control).border_color(t.stroke_strong).hover(move |s| s.bg(plus_hover)))
                            .child(icon("plus", 18., if adding { t.accent } else { t.text2 })),
                    )
                    .tooltip(tip(tr!("Add a server", "Adicionar um servidor"), t))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.adding() {
                            this.open_connect(None, true, window, cx);
                        }
                    })),
            )
    }

    fn rail_tile(&mut self, i: usize, s: SavedServer, on: bool, t: &Theme, cx: &mut Context<Root>) -> gpui::Stateful<gpui::Div> {
        let host = s.host();
        let name = if s.name.is_empty() { host.clone() } else { s.name.clone() };
        let hover = t.accent.opacity(if t.dark { 0.28 } else { 0.22 });
        let accent = t.accent;
        let picture = self.rail_icon(&s, cx);
        let dragged = DraggedServer { url: s.url.clone(), name: name.clone(), picture: picture.clone() };
        let (url, url2) = (s.url.clone(), s.url.clone());
        let tile = match picture {
            Some(image) => div()
                .size(px(TILE))
                .rounded(px(radius::CARD))
                .overflow_hidden()
                .when(!on, |d| d.opacity(0.85).hover(|s| s.opacity(1.)))
                .child(img(image).size_full().rounded(px(radius::CARD))),
            None => div()
                .size(px(TILE))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::CARD))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(15.))
                .when(on, |d| d.bg(t.accent).text_color(t.on_accent))
                .when(!on, |d| d.bg(t.accent_soft).text_color(t.accent).hover(move |s| s.bg(hover)))
                .child(initials(&name)),
        };
        let tip_text = (SharedString::from(name.clone()), SharedString::from(host));
        let theme = *t;
        // Named by address, so a tooltip or hover never sticks to a slot that now holds another server.
        rail_slot(SharedString::from(format!("rail-server-{}", s.url)), on, t)
            .child(tile)
            // Not over the tile's own menu, which already names the server.
            .when(!self.overlay.has_menu(), |d| {
                d.tooltip(move |_, cx| cx.new(|_| ServerTip { name: tip_text.0.clone(), host: tip_text.1.clone(), theme }).into())
            })
            .on_click(cx.listener(move |this, _, window, cx| this.switch_server(&url, window, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &MouseDownEvent, _, cx| this.server_menu(&url2, e.position, cx)))
            .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
            // The dragged server takes this one's place.
            .drag_over::<DraggedServer>(move |s, _, _, _| s.bg(accent.opacity(0.14)))
            .on_drop(cx.listener(move |_, d: &DraggedServer, _, cx| set_prefs(cx, |p| p.move_server(&d.url, i))))
    }

    /// A saved server's picture: from the media cache, so it draws offline, or else from the
    /// server, which hands its picture out before anyone signs in.
    fn rail_icon(&mut self, s: &SavedServer, cx: &mut Context<Root>) -> Option<Arc<RenderImage>> {
        if s.icon.is_empty() {
            return None;
        }
        if let Some(known) = self.rail_icons.get(&s.icon) {
            return known.clone();
        }
        self.rail_icons.insert(s.icon.clone(), None);
        let cache = Cache::shared(prefs(cx).media_cache_mb);
        let api = Api::new();
        api.set_server(&s.url, &s.password);
        let hash = s.icon.clone();
        cx.spawn(async move |this, cx| {
            let h = hash.clone();
            let got = core::run(async move { cache.fetch(&h, || api.server_icon(&h)).await }).await;
            let picture = match got {
                Ok((bytes, ct)) => {
                    cx.background_executor().spawn(async move { decode_picture(&bytes, &ct, ICON) }).await.ok().map(|(p, _)| p)
                }
                Err(e) => {
                    log::info!("server picture {hash} did not load: {e}");
                    None
                }
            };
            let _ = this.update(cx, |root, cx| {
                root.rail_icons.insert(hash, picture);
                // Pictures no saved server shows any more leave the GPU.
                let shown: HashSet<String> = prefs(cx).saved_servers.iter().map(|s| s.icon.clone()).collect();
                let gone: Vec<String> = root.rail_icons.keys().filter(|h| !shown.contains(*h)).cloned().collect();
                for h in gone {
                    if let Some(Some(old)) = root.rail_icons.remove(&h) {
                        cx.drop_image(old, None);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// Goes to a saved server: ends the session in use, then signs in with what was kept.
    pub(super) fn switch_server(&mut self, url: &str, window: &mut Window, cx: &mut Context<Root>) {
        if !self.adding() && same_server(&prefs(cx).server_url, url) {
            return;
        }
        let mut found = false;
        set_prefs(cx, |p| found = p.switch_server(url));
        if !found {
            return;
        }
        let p = prefs(cx);
        let note = p.session_token.is_empty().then(|| {
            let name = p.saved_server(url).map(|s| if s.name.is_empty() { s.host() } else { s.name.clone() }).unwrap_or_default();
            trf!("Type your password to sign back in to {}.", "Digite sua senha para entrar de novo em {}.", name)
        });
        self.open_connect(note, false, window, cx);
    }

    fn server_menu(&mut self, url: &str, at: Point<Pixels>, cx: &mut Context<Root>) {
        let Some(s) = prefs(cx).saved_server(url).cloned() else { return };
        let root = cx.entity().downgrade();
        let name = if s.name.is_empty() { s.host() } else { s.name.clone() };
        let header = (name.clone(), s.host());
        let copy = s.url.clone();
        let leave = s.url.clone();
        let menu = cx.new(|_| Menu {
            items: vec![
                MenuEntry::custom(move |t, _, _| {
                    div()
                        .flex()
                        .flex_col()
                        .px(px(10.))
                        .pt(px(6.))
                        .pb(px(4.))
                        .child(div().truncate().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child(header.0.clone()))
                        .child(mono(header.1.clone(), t.text3))
                        .into_any_element()
                }),
                MenuEntry::rule(),
                MenuEntry::item("paperclip", tr!("Copy address", "Copiar endereço"), false, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    overlay::toast(tr!("Address copied.", "Endereço copiado."), cx);
                }),
                MenuEntry::item("log-out", tr!("Remove server", "Remover servidor"), true, move |window, cx| {
                    let (root, url) = (root.clone(), leave.clone());
                    Ask::confirm_action(
                        trf!("Remove {}?", "Remover {}?", name),
                        tr!(
                            "It comes off the bar and this computer signs out of it. Your account there stays, so you can add it again with the +.",
                            "Ele sai da barra e este computador sai da sua conta lá. A conta continua existindo, então dá para adicionar de novo pelo +."
                        ),
                        tr!("Remove", "Remover"),
                        window,
                        cx,
                        move |window, cx| {
                            if let Some(root) = root.upgrade() {
                                root.update(cx, |r, cx| r.leave_server(&url, window, cx));
                            }
                        },
                    );
                }),
            ],
        });
        // Menus open through the root, which is busy running this listener.
        cx.defer(move |cx| overlay::open_menu(menu, at, cx));
    }

    /// Takes a server off the rail and ends this computer's sign-in there. Leaving the one in use
    /// moves on to the next saved server, so the window is never left on nothing while others wait.
    fn leave_server(&mut self, url: &str, window: &mut Window, cx: &mut Context<Root>) {
        let p = prefs(cx);
        let Some(index) = p.saved_servers.iter().position(|s| same_server(&s.url, url)) else { return };
        let in_use = !self.adding() && p.active_server() == Some(index);
        let mut gone = None;
        set_prefs(cx, |p| gone = p.forget_server(url));
        if let Some(gone) = gone.filter(|g| !g.session_token.is_empty()) {
            let api = Api::new();
            api.set_server(&gone.url, &gone.password);
            api.set_token(&gone.session_token);
            cx.background_executor()
                .spawn(async move {
                    let _ = core::run(async move { api.logout().await }).await;
                })
                .detach();
        }
        if in_use {
            let list = &prefs(cx).saved_servers;
            match list.get(index).or(list.last()).map(|s| s.url.clone()) {
                Some(next) => self.switch_server(&next, window, cx),
                None => self.open_connect(None, false, window, cx),
            }
        }
        cx.notify();
    }
}

/// A row as wide as the rail, with the active marker on its edge as the settings rail has it.
fn rail_slot(id: impl Into<gpui::ElementId>, on: bool, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let group: SharedString = format!("{id:?}").into();
    div().id(id).group(group.clone()).relative().w_full().flex().justify_center().cursor_pointer().child(
        div()
            .absolute()
            .left(px(0.))
            .top(px((TILE - 20.) / 2.))
            .w(px(3.))
            .h(px(20.))
            .rounded(px(2.))
            .bg(t.accent)
            .when(!on, |d| d.top(px((TILE - 8.) / 2.)).h(px(8.)).bg(t.text3).invisible().group_hover(group, |s| s.visible())),
    )
}

/// One or two letters for a tile without a picture: the first of each of the first two words.
fn initials(name: &str) -> String {
    name.split_whitespace().filter_map(|w| w.chars().find(|c| c.is_alphanumeric())).take(2).collect::<String>().to_uppercase()
}

struct ServerTip {
    name: SharedString,
    host: SharedString,
    theme: Theme,
}

impl Render for ServerTip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .font_family(FONT)
            .max_w(px(300.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(t.popover)
            .border_1()
            .border_color(t.stroke_strong)
            .shadow_md()
            .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text).child(self.name.clone()))
            .child(mono(self.host.clone(), t.text3))
    }
}

/// A server tile being dragged to a new place.
#[derive(Clone)]
struct DraggedServer {
    url: String,
    name: String,
    picture: Option<Arc<RenderImage>>,
}

impl Render for DraggedServer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        div()
            .size(px(TILE))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(radius::CARD))
            .bg(t.accent)
            .text_color(t.on_accent)
            .font_family(FONT)
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(15.))
            .shadow_lg()
            .opacity(0.9)
            .map(|d| match self.picture.clone() {
                Some(image) => d.overflow_hidden().child(img(image).size_full().rounded(px(radius::CARD))),
                None => d.child(initials(&self.name)),
            })
    }
}
