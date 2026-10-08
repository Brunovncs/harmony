//! The rail down the left edge: the servers of the account in use as tiles (their picture, or
//! their initials), the one in use marked, a + to add another, and the account itself at the
//! bottom. A click switches servers, a drag reorders them, and a right-click copies the address or
//! leaves the server. The account opens a menu of every account, each with its servers, to switch
//! between them, add one, or sign out.

use super::Root;
use super::connect::{AddServer, Adding, ConnectEvent, Me};
use super::overlay::{self, Ask, Dismiss, menu_card, menu_item, menu_rule};
use super::server::sidebar::{Menu, MenuEntry};
use crate::core::api::Api;
use crate::core::cache::Cache;
use crate::core::settings::{Account, SavedServer, same_server};
use crate::core::{self};
use crate::prefs::{prefs, set_prefs};
use crate::session::{Session, decode_picture};
use crate::theme::{FONT, GUTTER, Theme, current, px, radius};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, Render, RenderImage, SharedString, StatefulInteractiveElement, Styled, WeakEntity,
    Window, div, img,
};
use std::collections::HashSet;
use std::sync::Arc;

const TILE: f32 = 40.;
/// Server pictures and avatars are decoded at this, enough for the rail at the largest UI size.
const ICON: (u32, u32) = (96, 96);

impl Root {
    pub(super) fn render_rail(&mut self, t: &Theme, cx: &mut Context<Root>) -> gpui::Div {
        let p = prefs(cx);
        let active = p.active_server();
        let account = p.account(&p.active_account).cloned();
        let servers: Vec<(usize, SavedServer)> = p.account_servers(&p.active_account).map(|(i, s)| (i, s.clone())).collect();
        // Every account's picture is asked for now, so the account menu opens with them.
        let accounts = p.accounts.clone();
        for a in &accounts {
            self.account_picture(a, cx);
        }
        let mut list = div().id("rail-servers").flex().flex_col().gap(px(8.)).py(px(2.)).min_h(px(0.)).overflow_y_scroll();
        for (i, s) in servers {
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
                rail_slot("rail-add", false, t)
                    .child(
                        div()
                            .size(px(TILE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(radius::CARD))
                            .border_1()
                            .border_dashed()
                            .bg(t.control)
                            .border_color(t.stroke_strong)
                            .hover(move |s| s.bg(plus_hover))
                            .child(icon("plus", 18., t.text2)),
                    )
                    .tooltip(tip(tr!("Add a server", "Adicionar um servidor"), t))
                    .on_click(cx.listener(|this, _, window, cx| this.add(Adding::Server, window, cx))),
            )
            .child(div().flex_1())
            .when_some(account, |d, a| d.child(self.account_button(&a, t, cx)))
    }

    fn account_button(&mut self, a: &Account, t: &Theme, cx: &mut Context<Root>) -> gpui::Stateful<gpui::Div> {
        let picture = self.account_picture(a, cx);
        let tip_text = (SharedString::from(a.name().to_string()), SharedString::from(format!("@{}", a.username)));
        let theme = *t;
        div()
            .id("rail-account")
            .flex()
            .justify_center()
            .cursor_pointer()
            .child(div().rounded_full().hover(|s| s.opacity(0.85)).child(avatar(a.name(), picture, 36., None)))
            .when(!self.overlay.has_menu(), |d| {
                d.tooltip(move |_, cx| cx.new(|_| ServerTip { name: tip_text.0.clone(), host: tip_text.1.clone(), theme }).into())
            })
            .on_click(cx.listener(|this, _, window, cx| this.account_menu(window, cx)))
    }

    fn rail_tile(&mut self, i: usize, s: SavedServer, on: bool, t: &Theme, cx: &mut Context<Root>) -> gpui::Stateful<gpui::Div> {
        let host = s.host();
        let name = if s.name.is_empty() { host.clone() } else { s.name.clone() };
        let picture = self.rail_picture(&s.icon, &s, false, cx);
        let dragged = DraggedServer { url: s.url.clone(), username: s.username.clone(), name: name.clone(), picture: picture.clone() };
        let (url, url2, user) = (s.url.clone(), s.url.clone(), s.username.clone());
        let tile = server_tile(&name, picture, TILE, on, t);
        let tip_text = (SharedString::from(name.clone()), SharedString::from(host));
        let theme = *t;
        let accent = t.accent;
        // Named by address, so a tooltip or hover never sticks to a slot that now holds another server.
        rail_slot(SharedString::from(format!("rail-server-{}", s.url)), on, t)
            .child(tile)
            // Not over the tile's own menu, which already names the server.
            .when(!self.overlay.has_menu(), |d| {
                d.tooltip(move |_, cx| cx.new(|_| ServerTip { name: tip_text.0.clone(), host: tip_text.1.clone(), theme }).into())
            })
            .on_click(cx.listener(move |this, _, window, cx| this.switch_server(&url, &user, window, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &MouseDownEvent, _, cx| this.server_menu(&url2, e.position, cx)))
            .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
            // The dragged server takes this one's place.
            .drag_over::<DraggedServer>(move |s, _, _, _| s.bg(accent.opacity(0.14)))
            .on_drop(cx.listener(move |_, d: &DraggedServer, _, cx| set_prefs(cx, |p| p.move_server(&d.url, &d.username, i))))
    }

    /// An account's picture, from the server that last showed it.
    fn account_picture(&mut self, a: &Account, cx: &mut Context<Root>) -> Option<Arc<RenderImage>> {
        let server = prefs(cx).saved_server_as(&a.avatar_server, &a.username)?.clone();
        self.rail_picture(&a.avatar, &server, true, cx)
    }

    /// A picture by its upload hash: from the media cache, so it draws offline, or else from
    /// `server`, which hands its own picture out before anyone signs in and an avatar to the
    /// account signed in there.
    fn rail_picture(&mut self, hash: &str, server: &SavedServer, avatar: bool, cx: &mut Context<Root>) -> Option<Arc<RenderImage>> {
        if hash.is_empty() {
            return None;
        }
        if let Some(known) = self.rail_icons.get(hash) {
            return known.clone();
        }
        self.rail_icons.insert(hash.to_string(), None);
        let cache = Cache::shared(prefs(cx).media_cache_mb);
        let api = Api::new();
        api.set_server(&server.url, &server.password);
        api.set_token(&server.session_token);
        let hash = hash.to_string();
        cx.spawn(async move |this, cx| {
            let h = hash.clone();
            let got = core::run(async move {
                cache.fetch(&h, || async { if avatar { api.download(&h).await } else { api.server_icon(&h).await } }).await
            })
            .await;
            let picture = match got {
                Ok((bytes, ct)) => {
                    cx.background_executor().spawn(async move { decode_picture(&bytes, &ct, ICON) }).await.ok().map(|(p, _)| p)
                }
                Err(e) => {
                    log::info!("rail picture {hash} did not load: {e}");
                    None
                }
            };
            let _ = this.update(cx, |root, cx| {
                root.rail_icons.insert(hash, picture);
                // Pictures nothing in the rail shows any more leave the GPU.
                let p = prefs(cx);
                let shown: HashSet<&str> =
                    p.saved_servers.iter().map(|s| s.icon.as_str()).chain(p.accounts.iter().map(|a| a.avatar.as_str())).collect();
                let gone: Vec<String> = root.rail_icons.keys().filter(|h| !shown.contains(h.as_str())).cloned().collect();
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
    pub(super) fn switch_server(&mut self, url: &str, username: &str, window: &mut Window, cx: &mut Context<Root>) {
        let p = prefs(cx);
        if p.username == username && same_server(&p.server_url, url) {
            return;
        }
        let mut found = false;
        set_prefs(cx, |p| found = p.switch_server(url, username));
        if !found {
            return;
        }
        let p = prefs(cx);
        let note = p.session_token.is_empty().then(|| {
            let name =
                p.saved_server_as(url, username).map(|s| if s.name.is_empty() { s.host() } else { s.name.clone() }).unwrap_or_default();
            trf!("Type your password to sign back in to {}.", "Digite sua senha para entrar de novo em {}.", name)
        });
        self.show_connect(note, window, cx);
    }

    /// Shows another account's servers in the rail, and goes to the one it used last.
    fn switch_account(&mut self, username: &str, window: &mut Window, cx: &mut Context<Root>) {
        if let Some(url) = prefs(cx).account_home(username) {
            self.switch_server(&url, username, window, cx);
        }
    }

    /// The small dialog that adds a server, an account, or a new account. The ones for accounts
    /// start from the server in use, the likeliest place for another name.
    fn add(&mut self, kind: Adding, window: &mut Window, cx: &mut Context<Root>) {
        let p = prefs(cx);
        let account = p.account(&p.active_account).cloned();
        let server = match kind {
            Adding::Server => String::new(),
            _ if !p.server_url.is_empty() => p.server_url.clone(),
            _ => p.account_home(&p.active_account).unwrap_or_default(),
        };
        let me = account.map(|a| Me { picture: self.account_picture(&a, cx), name: a.name().to_string(), username: a.username });
        let view = cx.new(|cx| AddServer::new(kind, me, &server, window, cx));
        cx.subscribe_in(&view, window, |this, _, ev: &ConnectEvent, window, cx| match ev {
            ConnectEvent::Connected(done) => this.enter(done, window, cx),
        })
        .detach();
        // Dialogs open through the root, which is busy running this.
        window.defer(cx, move |window, cx| overlay::open_dialog(view, window, cx));
    }

    fn account_menu(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let p = prefs(cx).clone();
        let active = p.active_server();
        let mut rows = Vec::new();
        for a in &p.accounts {
            let servers = p
                .account_servers(&a.username)
                .map(|(i, s)| {
                    let name = if s.name.is_empty() { s.host() } else { s.name.clone() };
                    ServerRow { url: s.url.clone(), name, picture: self.rail_picture(&s.icon, s, false, cx), on: active == Some(i) }
                })
                .collect();
            let picture = self.account_picture(a, cx);
            rows.push(AccountRow { username: a.username.clone(), name: a.name().to_string(), picture, servers });
        }
        let Some(at) = rows.iter().position(|r| r.username == p.active_account) else { return };
        let me = rows.remove(at);
        let root = cx.entity().downgrade();
        let menu = cx.new(|_| AccountMenu { root, me, others: rows });
        // Beside the rail, its foot level with the rail's.
        let size = window.viewport_size();
        let corner = gpui::point(px(GUTTER + TILE + 20. + 6.), size.height - px(GUTTER) + px(10.));
        cx.defer(move |cx| overlay::open_menu_above(menu, corner, cx));
    }

    fn server_menu(&mut self, url: &str, at: Point<Pixels>, cx: &mut Context<Root>) {
        let p = prefs(cx);
        let Some(s) = p.saved_server_as(url, &p.active_account).cloned() else { return };
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

    /// Takes one of the account's servers off the rail and ends this computer's sign-in there.
    fn leave_server(&mut self, url: &str, window: &mut Window, cx: &mut Context<Root>) {
        let p = prefs(cx);
        let user = p.active_account.clone();
        let Some(index) = p.saved_servers.iter().position(|s| s.username == user && same_server(&s.url, url)) else { return };
        let in_use = p.active_server() == Some(index);
        let mut gone = None;
        set_prefs(cx, |p| gone = p.forget_server(url, &user));
        sign_out(gone, cx);
        if in_use {
            self.move_on(Some((&user, index)), window, cx);
        }
        cx.notify();
    }

    /// Signs out of every server of the account in use and takes them off the rail.
    fn sign_out_account(&mut self, username: &str, window: &mut Window, cx: &mut Context<Root>) {
        let mut gone = Vec::new();
        set_prefs(cx, |p| gone = p.forget_account(username));
        // Off the session first, so it does not see its own sign-in end.
        self.move_on(None, window, cx);
        for s in gone {
            sign_out(Some(s), cx);
        }
        cx.notify();
    }

    /// After the server in use went away, so the window is never left on nothing while others
    /// wait: the account's next server, or else where the active account was last, or the first
    /// screen when no server is left.
    fn move_on(&mut self, from: Option<(&str, usize)>, window: &mut Window, cx: &mut Context<Root>) {
        let p = prefs(cx);
        let same = from.and_then(|(user, at)| {
            let list: Vec<(usize, String)> = p.account_servers(user).map(|(i, s)| (i, s.url.clone())).collect();
            list.iter().find(|(i, _)| *i >= at).or(list.last()).map(|(_, url)| (url.clone(), user.to_string()))
        });
        match same.or_else(|| p.account_home(&p.active_account).map(|url| (url, p.active_account.clone()))) {
            Some((url, user)) => self.switch_server(&url, &user, window, cx),
            None => self.show_connect(None, window, cx),
        }
    }
}

/// Ends a saved sign-in on its server, in the background.
fn sign_out(server: Option<SavedServer>, cx: &mut App) {
    let Some(s) = server.filter(|s| !s.session_token.is_empty()) else { return };
    let api = Api::new();
    api.set_server(&s.url, &s.password);
    api.set_token(&s.session_token);
    cx.background_executor()
        .spawn(async move {
            let _ = core::run(async move { api.logout().await }).await;
        })
        .detach();
}

/// Keeps the rail's copy of the server in use, and of who you are there, as the session last had
/// them.
pub(super) fn keep_session_news(session: &Entity<Session>, cx: &mut App) {
    let s = session.read(cx);
    let (url, name, icon) = (s.api.base(), s.server_name.clone(), s.server_icon.clone().unwrap_or_default());
    let (user, display, avatar) = (s.me.nickname.clone(), s.me.name().to_string(), s.me.avatar_hash.clone().unwrap_or_default());
    let p = prefs(cx);
    if !p.knows_server(&url, &name, &icon) || !p.knows_identity(&user, &url, &display, &avatar) {
        set_prefs(cx, |p| {
            p.note_server(&url, &name, &icon);
            p.note_identity(&user, &url, &display, &avatar);
        });
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

/// A server's picture, or its initials on the accent, `size` across.
fn server_tile(name: &str, picture: Option<Arc<RenderImage>>, size: f32, on: bool, t: &Theme) -> gpui::Div {
    let corner = radius::CARD * size / TILE;
    let hover = t.accent.opacity(if t.dark { 0.28 } else { 0.22 });
    match picture {
        Some(image) => div()
            .flex_none()
            .size(px(size))
            .rounded(px(corner))
            .overflow_hidden()
            .when(!on, |d| d.opacity(0.85).hover(|s| s.opacity(1.)))
            .child(img(image).size_full().rounded(px(corner))),
        None => div()
            .flex_none()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(corner))
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(size * 0.375))
            .when(on, |d| d.bg(t.accent).text_color(t.on_accent))
            .when(!on, |d| d.bg(t.accent_soft).text_color(t.accent).hover(move |s| s.bg(hover)))
            .child(initials(name)),
    }
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
    username: String,
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

struct ServerRow {
    url: String,
    name: String,
    picture: Option<Arc<RenderImage>>,
    on: bool,
}

struct AccountRow {
    username: String,
    name: String,
    picture: Option<Arc<RenderImage>>,
    servers: Vec<ServerRow>,
}

/// Every account with its servers: the one in use first, the others a click away, then adding
/// one and signing out.
struct AccountMenu {
    root: WeakEntity<Root>,
    me: AccountRow,
    others: Vec<AccountRow>,
}

impl EventEmitter<Dismiss> for AccountMenu {}

impl AccountMenu {
    /// Closes the menu and does `f` on the root.
    fn then(&self, cx: &mut Context<Self>, window: &mut Window, f: impl FnOnce(&mut Root, &mut Window, &mut Context<Root>)) {
        cx.emit(Dismiss);
        if let Some(root) = self.root.upgrade() {
            root.update(cx, |r, cx| f(r, window, cx));
        }
    }

    fn server_line(&self, id: (&'static str, usize), username: &str, s: &ServerRow, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let (url, user) = (s.url.clone(), username.to_string());
        let hover = t.layer_hover;
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(30.))
            .px(px(10.))
            .rounded(px(radius::INNER))
            .cursor_pointer()
            .hover(move |d| d.bg(hover))
            .child(server_tile(&s.name, s.picture.clone(), 20., s.on, t))
            .child(div().flex_1().min_w(px(0.)).truncate().child(body(s.name.clone(), if s.on { t.text } else { t.text2 })))
            .when(s.on, |d| d.child(icon("check", 14., t.accent)))
            .on_click(
                cx.listener(move |this, _, window, cx| this.then(cx, window, |r, window, cx| r.switch_server(&url, &user, window, cx))),
            )
    }
}

fn who(a: &AccountRow, size: f32, t: &Theme) -> gpui::Div {
    div().flex().items_center().gap(px(10.)).min_w(px(0.)).child(avatar(&a.name, a.picture.clone(), size, None)).child(
        div()
            .flex()
            .flex_col()
            .min_w(px(0.))
            .child(div().truncate().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.text).child(a.name.clone()))
            .child(mono(format!("@{}", a.username), t.text3)),
    )
}

impl Render for AccountMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let mut card = menu_card(&t).w(px(280.)).child(div().px(px(10.)).pt(px(8.)).pb(px(6.)).child(who(&self.me, 34., &t)));
        for (i, s) in self.me.servers.iter().enumerate() {
            card = card.child(self.server_line(("account-server", i), &self.me.username, s, &t, cx));
        }
        if !self.others.is_empty() {
            card = card
                .child(menu_rule(&t))
                .child(div().px(px(10.)).pt(px(4.)).pb(px(2.)).child(label(tr!("Other accounts", "Outras contas"), &t)));
        }
        let hover = t.layer_hover;
        let mut n = 0;
        for (i, a) in self.others.iter().enumerate() {
            let user = a.username.clone();
            card = card.child(
                div()
                    .id(("account", i))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(radius::INNER))
                    .cursor_pointer()
                    .hover(move |d| d.bg(hover))
                    .child(who(a, 28., &t))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.then(cx, window, |r, window, cx| r.switch_account(&user, window, cx))),
                    ),
            );
            for s in &a.servers {
                card = card.child(div().pl(px(28.)).child(self.server_line(("other-server", n), &a.username, s, &t, cx)));
                n += 1;
            }
        }
        let me = (self.me.username.clone(), self.me.name.clone());
        card.child(menu_rule(&t))
            .child(
                menu_item("account-add", Some("arrow-right"), tr!("Add an account", "Adicionar conta"), false, &t).on_click(cx.listener(
                    |this, _, window, cx| this.then(cx, window, |r, window, cx| r.add(Adding::Account, window, cx)),
                )),
            )
            .child(
                menu_item("account-new", Some("plus"), tr!("Create an account", "Criar conta"), false, &t).on_click(cx.listener(
                    |this, _, window, cx| this.then(cx, window, |r, window, cx| r.add(Adding::NewAccount, window, cx)),
                )),
            )
            .child(menu_rule(&t))
            .child(menu_item("account-out", Some("log-out"), tr!("Sign out of this account", "Sair desta conta"), true, &t).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.emit(Dismiss);
                    let (root, (user, name)) = (this.root.clone(), me.clone());
                    Ask::confirm_action(
                        trf!("Sign out of {}?", "Sair de {}?", name),
                        trf!(
                            "This computer signs out of every server where you are @{} and they leave the bar. The accounts stay on the servers, so you can sign in again later.",
                            "Este computador sai de todos os servidores onde você é @{} e eles saem da barra. As contas continuam nos servidores, então dá para entrar de novo depois.",
                            user
                        ),
                        tr!("Sign out", "Sair"),
                        window,
                        cx,
                        move |window, cx| {
                            if let Some(root) = root.upgrade() {
                                root.update(cx, |r, cx| r.sign_out_account(&user, window, cx));
                            }
                        },
                    );
                }),
            ))
    }
}
