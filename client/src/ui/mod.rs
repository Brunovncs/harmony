//! The window's root: the sign-in screen or the server, the theme, and what floats over them
//! (dialogs, menus, toasts).

pub mod camera;
pub mod connect;
pub mod hotkeys;
pub mod overlay;
mod rail;
pub mod server;
pub mod settings;
pub mod tray;
pub mod updates;

use crate::prefs::prefs;
use crate::session::{Session, SessionEvent};
use crate::theme::{self, CustomColors, FONT, Theme, px, radius};
use crate::widgets::*;
use connect::{ConnectEvent, ConnectView, Connected};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    Render, RenderImage, SharedString, Styled, Subscription, Window, div,
};
use overlay::{Overlay, Toast};
use server::ServerView;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

enum Screen {
    Connect { view: Entity<ConnectView> },
    Server(Entity<ServerView>),
}

pub struct Root {
    focus: FocusHandle,
    screen: Screen,
    pub overlay: Overlay,
    toasts: Vec<Toast>,
    next_toast: usize,
    _subs: Vec<Subscription>,
    /// What the current screen listens to, dropped with it.
    screen_subs: Vec<Subscription>,
    /// The rail's server pictures by hash; `None` while one loads or when it would not.
    rail_icons: HashMap<String, Option<Arc<RenderImage>>>,
}

impl Focusable for Root {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The theme the settings ask for, on this window.
pub fn theme_for(window: &Window, cx: &App) -> Theme {
    let p = prefs(cx);
    let forced = std::env::var("HARMONY_THEME").ok();
    let custom = p.custom_theme.as_ref().and_then(CustomColors::from_settings);
    Theme::new(forced.as_deref().unwrap_or(&p.theme), window.appearance(), custom.as_ref())
}

impl Root {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Root {
        // The theme and the interface size can change with either; parts of the window that are
        // cached (the chat, the channel and member lists) would keep the old ones, so everything
        // draws again.
        let mut subs = vec![cx.observe_window_appearance(window, |_, window, _| window.refresh())];
        subs.push(cx.observe_global_in::<crate::prefs::Prefs>(window, |_, window, cx| {
            hotkeys::sync(cx);
            window.refresh();
        }));
        let view = cx.new(|cx| ConnectView::new(None, window, cx));
        let mut root = Root {
            focus: cx.focus_handle(),
            screen: Screen::Connect { view: view.clone() },
            overlay: Overlay::default(),
            toasts: Vec::new(),
            next_toast: 0,
            _subs: subs,
            screen_subs: Vec::new(),
            rail_icons: HashMap::new(),
        };
        root.subscribe_connect(&view, window, cx);
        root
    }

    pub fn toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.next_toast += 1;
        let id = self.next_toast;
        self.toasts.push(Toast { id, text: text.into() });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(5000)).await;
            let _ = this.update(cx, |this, cx| {
                this.toasts.retain(|t| t.id != id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" && self.overlay.close_top() {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let _ = window;
    }
}

/// The Harmony mark: a microphone on a stand inside a rounded square, in the accent, as the app
/// icon draws it.
pub fn brand_mark(size: f32, t: &Theme) -> gpui::Div {
    let bar = |h: f32| div().w(px(size * 0.085)).h(px(size * h)).rounded(px(size * 0.05)).bg(t.on_accent);
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size * 0.26))
        .bg(t.accent)
        .flex()
        .items_center()
        .justify_center()
        .gap(px(size * 0.055))
        .children([0.18, 0.36, 0.52, 0.36, 0.18].map(bar))
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        theme::set_scale(prefs(cx).ui_scale as f32 / 100.);
        let t = theme_for(window, cx);
        theme::set_current(t);
        let screen: AnyElement = match &self.screen {
            Screen::Connect { view, .. } => view.clone().into_any_element(),
            Screen::Server(v) => v.clone().into_any_element(),
        };
        let rail = (!prefs(cx).saved_servers.is_empty()).then(|| self.render_rail(&t, cx));
        div()
            .id("root")
            .key_context("Root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .size_full()
            .font_family(FONT)
            .text_size(px(14.))
            .text_color(t.text)
            .bg(t.base)
            .flex()
            .children(rail)
            .child(div().flex_1().min_w(px(0.)).h_full().child(screen))
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .right(px(12.))
                    .text_size(px(10.))
                    .line_height(px(10.))
                    .text_color(t.text3)
                    .opacity(0.6)
                    .child(concat!("v", env!("CARGO_PKG_VERSION"))),
            )
            .children(self.render_overlay(&t, cx))
            .when(!self.toasts.is_empty(), |d| {
                d.child(div().absolute().bottom(px(24.)).left_0().right_0().flex().flex_col().items_center().gap(px(8.)).children(
                    self.toasts.iter().map(|toast| {
                        div()
                            .px(px(16.))
                            .py(px(10.))
                            .max_w(px(520.))
                            .rounded(px(radius::CONTROL))
                            .bg(t.popover)
                            .border_1()
                            .border_color(t.stroke_strong)
                            .shadow_lg()
                            .child(body(toast.text.clone(), t.text))
                    }),
                ))
            })
    }
}

impl Root {
    /// The sign-in screen for the active server, which goes straight in when a sign-in is kept.
    pub fn show_connect(&mut self, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Server(v) = &self.screen {
            v.update(cx, |v, cx| v.shutdown(cx));
        }
        self.screen_subs.clear();
        let view = cx.new(|cx| ConnectView::new(error, window, cx));
        self.subscribe_connect(&view, window, cx);
        self.screen = Screen::Connect { view };
        self.overlay = Overlay::default();
        cx.notify();
    }

    fn subscribe_connect(&mut self, view: &Entity<ConnectView>, window: &mut Window, cx: &mut Context<Self>) {
        let sub = cx.subscribe_in(view, window, |this, _, ev: &ConnectEvent, window, cx| match ev {
            ConnectEvent::Connected(done) => this.enter(done, window, cx),
        });
        self.screen_subs.push(sub);
    }

    /// Starts the session a sign-in handed over, in place of whatever was open.
    fn enter(&mut self, done: &Connected, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Server(v) = &self.screen {
            v.update(cx, |v, cx| v.shutdown(cx));
        }
        let session = Session::start(
            done.api.clone(),
            done.cache.clone(),
            done.me.clone(),
            done.server_name.clone(),
            done.server_icon.clone(),
            done.ice_servers.clone(),
            cx,
        );
        self.open_server(session, window, cx);
    }

    fn open_server(&mut self, session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        self.screen_subs.clear();
        let sub = cx.subscribe_in(&session, window, |this, _, ev: &SessionEvent, window, cx| match ev {
            SessionEvent::SignedOut(why) => {
                crate::prefs::set_prefs(cx, |s| s.session_token.clear());
                this.show_connect(Some(why.message().into()), window, cx);
            }
            SessionEvent::Toast(text) => this.toast(text.clone(), cx),
            _ => {}
        });
        self.screen_subs.push(sub);
        self.screen_subs.push(cx.observe(&session, |_, session, cx| rail::keep_session_news(&session, cx)));
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| ServerView::new(session, root, window, cx));
        self.screen = Screen::Server(view);
        cx.notify();
    }
}
