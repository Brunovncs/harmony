//! Global hotkeys in the window: what is registered right now, the recorder that asks for a
//! combination, and what a press does. Mute and deafen are bound in Settings › Hotkeys, a
//! soundboard clip from its right-click menu.

use super::Screen;
use super::overlay::{self, Dismiss, dialog_card};
use crate::core::settings::HotkeyAction;
use crate::hotkeys::{Combo, Event, Failure, Registrar};
use crate::prefs::{prefs, set_prefs};
use crate::theme::{Theme, current, px, radius};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, BorrowAppContext, Context, EntityId, EventEmitter, FocusHandle, Focusable, Global, InteractiveElement, IntoElement,
    KeyDownEvent, ModifiersChangedEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity,
    Window, div,
};
use std::collections::HashMap;
use std::time::Duration;

pub struct Hotkeys {
    registrar: Option<Registrar>,
    /// The server open in the window, and its clips: only those clips' bindings are registered.
    server: Option<(String, Vec<i64>)>,
    wanted: Vec<(HotkeyAction, Combo)>,
    sent: Option<Vec<(HotkeyAction, Combo)>>,
    /// Recorders open, newest last. While any is, nothing is registered: Windows hands a
    /// registered combination to us instead of to the window, so the recorder would never see it.
    recorders: Vec<(EntityId, WeakEntity<Recorder>)>,
    watching: bool,
    failures: HashMap<HotkeyAction, Failure>,
    /// A binding just made, whose failure gets a toast.
    report: Option<HotkeyAction>,
}

impl Global for Hotkeys {}

impl Hotkeys {
    fn flush(&mut self) {
        let recording = !self.recorders.is_empty();
        if recording != self.watching
            && let Some(r) = &self.registrar
        {
            r.record(recording);
        }
        self.watching = recording;
        let want = if recording { Vec::new() } else { self.wanted.clone() };
        if self.sent.as_ref() == Some(&want) {
            return;
        }
        if let Some(r) = &self.registrar {
            r.set(want.clone());
        }
        self.sent = Some(want);
    }
}

pub fn init(cx: &mut App) {
    let (tx, rx) = async_channel::unbounded();
    let registrar = Registrar::start(tx);
    if registrar.is_none() {
        log::info!("global hotkeys are not available here");
    }
    cx.set_global(Hotkeys {
        registrar,
        server: None,
        wanted: Vec::new(),
        sent: None,
        recorders: Vec::new(),
        watching: false,
        failures: HashMap::new(),
        report: None,
    });
    cx.spawn(async move |cx| {
        while let Ok(ev) = rx.recv().await {
            cx.update(|cx| on_event(ev, cx));
        }
    })
    .detach();
    sync(cx);
}

/// Registers what the settings bind, for the server open now.
pub fn sync(cx: &mut App) {
    let p = prefs(cx);
    let server = cx.global::<Hotkeys>().server.clone();
    let base = server.as_ref().map(|s| s.0.as_str()).unwrap_or("");
    let mut wanted: Vec<(HotkeyAction, Combo)> =
        [HotkeyAction::Mute, HotkeyAction::Deafen].into_iter().filter_map(|a| Some((a, Combo::parse(p.hotkey(base, a))?))).collect();
    // Clips that still exist only: a deleted one's binding stays in the file but holds no key.
    if let Some((base, clips)) = &server
        && let Some(bound) = p.server_clip_hotkeys(base)
    {
        for id in clips {
            if let Some(c) = bound.get(&id.to_string()).and_then(|a| Combo::parse(a)) {
                wanted.push((HotkeyAction::Clip(*id), c));
            }
        }
    }
    cx.update_global::<Hotkeys, _>(|h, _| {
        h.wanted = wanted;
        h.flush();
    });
}

/// The server the window shows (its address and clip ids), or `None` when none is open.
pub fn set_server(server: Option<(String, Vec<i64>)>, cx: &mut App) {
    if cx.global::<Hotkeys>().server != server {
        cx.global_mut::<Hotkeys>().server = server;
        sync(cx);
    }
}

pub fn failure(action: HotkeyAction, cx: &App) -> Option<Failure> {
    cx.global::<Hotkeys>().failures.get(&action).copied()
}

fn on_event(ev: Event, cx: &mut App) {
    match ev {
        Event::Registered(results) => {
            let h = cx.global_mut::<Hotkeys>();
            h.failures = results.into_iter().filter_map(|(a, f)| Some((a, f?))).collect();
            let failed = h.report.take().and_then(|a| Some((a, *h.failures.get(&a)?)));
            if let Some((action, f)) = failed {
                let server = h.server.as_ref().map(|s| s.0.clone()).unwrap_or_default();
                let keys = Combo::parse(prefs(cx).hotkey(&server, action)).map(|c| c.keycaps().join(" + ")).unwrap_or_default();
                overlay::toast(format!("{keys}: {}", f.message()), cx);
            }
            cx.refresh_windows();
        }
        Event::Taken(combo) => {
            if let Some(recorder) = cx.global::<Hotkeys>().recorders.last().and_then(|r| r.1.upgrade()) {
                recorder.update(cx, |r, cx| r.taken(combo, cx));
            }
        }
        Event::Pressed(action) => {
            let root = cx.try_global::<overlay::RootHandle>().and_then(|h| h.0.upgrade());
            let view = root.and_then(|r| match &r.read(cx).screen {
                Screen::Server(v) => Some(v.clone()),
                Screen::Connect { .. } => None,
            });
            if let Some(view) = view {
                view.update(cx, |v, cx| v.on_hotkey(action, cx));
            }
        }
    }
}

/// A combination as keycaps.
pub fn keys(caps: &[String], t: &Theme) -> gpui::Div {
    div().flex().items_center().gap(px(4.)).children(caps.iter().map(|k| keycap(k.clone(), t)))
}

/// Asks for a key combination, from the window's key presses. Emits `Dismiss` when done.
pub struct Recorder {
    focus: FocusHandle,
    action: HotkeyAction,
    what: SharedString,
    server: String,
    /// In a dialog of its own, or drawn inside something else (the soundboard).
    framed: bool,
    shown: Vec<String>,
    /// Modifiers held now, and whether shown ends in a key (refused or taken) rather than only
    /// the modifiers being held.
    mods: usize,
    keyed: bool,
    error: Option<&'static str>,
    caught: bool,
    _resume: Subscription,
}

impl EventEmitter<Dismiss> for Recorder {}

impl Focusable for Recorder {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Opens the recorder as a dialog.
pub fn edit(action: HotkeyAction, what: impl Into<SharedString>, server: String, window: &mut Window, cx: &mut App) {
    let what = what.into();
    let view = cx.new(|cx| Recorder::new(action, what, server, true, cx));
    overlay::open_dialog(view, window, cx);
}

/// Binds a combination (or unbinds, with ""), and says so if it can't be registered.
pub fn bind(action: HotkeyAction, server: &str, accelerator: &str, cx: &mut App) {
    cx.global_mut::<Hotkeys>().report = (!accelerator.is_empty()).then_some(action);
    set_prefs(cx, |p| p.bind_hotkey(server, action, accelerator));
    sync(cx);
}

impl Recorder {
    pub fn new(action: HotkeyAction, what: SharedString, server: String, framed: bool, cx: &mut Context<Self>) -> Recorder {
        let (id, me) = (cx.entity_id(), cx.weak_entity());
        cx.update_global::<Hotkeys, _>(|h, _| {
            h.recorders.push((id, me));
            h.flush();
        });
        // However it closes (a button, Escape, a click outside), the hotkeys come back.
        let resume = cx.on_release(move |_, cx| {
            cx.update_global::<Hotkeys, _>(|h, _| {
                h.recorders.retain(|r| r.0 != id);
                h.flush();
            })
        });
        Recorder {
            focus: cx.focus_handle(),
            action,
            what,
            server,
            framed,
            shown: Vec::new(),
            mods: 0,
            keyed: false,
            error: None,
            caught: false,
            _resume: resume,
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Taken here, so Escape doesn't close what is behind and Ctrl+, opens nothing.
        cx.stop_propagation();
        let k = &ev.keystroke;
        if self.caught {
            return;
        }
        let m = k.modifiers;
        if k.key == "escape" && !(m.control || m.alt || m.shift || m.platform) {
            cx.emit(Dismiss);
            return;
        }
        match Combo::from_keystroke(k) {
            Ok(combo) => {
                self.shown = combo.keycaps();
                self.keyed = true;
                self.error = None;
                self.caught = true;
                let (action, server) = (self.action, self.server.clone());
                // A beat to show what was caught before it goes.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(300)).await;
                    let _ = this.update(cx, |_, cx| {
                        bind(action, &server, &combo.accelerator(), cx);
                        cx.emit(Dismiss);
                    });
                })
                .detach();
            }
            Err(refusal) => {
                let mut shown: Vec<String> = Combo { ctrl: m.control, alt: m.alt, shift: m.shift, win: m.platform, vk: 0 }
                    .modifiers()
                    .into_iter()
                    .map(String::from)
                    .collect();
                shown.push(k.key.to_uppercase());
                self.shown = shown;
                self.keyed = true;
                self.error = Some(refusal.message());
            }
        }
        cx.notify();
    }

    /// Another program holds what was pressed: shown, and not bound.
    fn taken(&mut self, combo: Combo, cx: &mut Context<Self>) {
        if self.caught {
            return;
        }
        self.shown = combo.keycaps();
        self.keyed = true;
        self.error = Some(Failure::InUse.message());
        cx.notify();
    }

    fn on_modifiers(&mut self, ev: &ModifiersChangedEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.caught {
            return;
        }
        let m = ev.modifiers;
        let held = Combo { ctrl: m.control, alt: m.alt, shift: m.shift, win: m.platform, vk: 0 }.modifiers();
        // Pressing one starts over; letting go keeps what was shown, so it can be read.
        let pressed = held.len() > self.mods;
        self.mods = held.len();
        if pressed || !self.keyed {
            self.keyed = false;
            self.shown = held.into_iter().map(String::from).collect();
        }
        cx.notify();
    }

    fn content(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let bound = !prefs(cx).hotkey(&self.server, self.action).is_empty();
        let capture = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .h(px(64.))
            .rounded(px(radius::CARD))
            .bg(t.well)
            .border_1()
            .border_color(if self.caught {
                t.success
            } else if self.error.is_some() {
                t.critical
            } else {
                t.stroke
            })
            .map(|d| match self.shown.is_empty() {
                true => d.child(body(tr!("Waiting for keys…", "Esperando as teclas…"), t.text3)),
                false => d.child(keys(&self.shown, t)).when(!self.keyed && self.mods > 0, |d| d.child(mono("+ …", t.text3))),
            });
        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .when(bound, |d| {
                d.child(button("hotkey-clear", tr!("Remove hotkey", "Remover atalho"), Kind::Subtle, t).on_click(cx.listener(
                    |this, _, _, cx| {
                        bind(this.action, &this.server, "", cx);
                        cx.emit(Dismiss);
                    },
                )))
            })
            .child(
                button("hotkey-cancel", tr!("Cancel", "Cancelar"), Kind::Standard, t).on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
            );
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(title(trf!("Hotkey: {}", "Atalho: {}", self.what), t.text))
            .child(caption(
                tr!(
                    "Press the combination you want. It works even while a game has focus. Esc cancels.",
                    "Pressione a combinação que você quer. Ela funciona até com um jogo em primeiro plano. Esc cancela."
                ),
                t.text2,
            ))
            .child(capture)
            .when_some(self.error, |d, e| d.child(caption(e, t.critical)))
            .child(buttons)
    }
}

impl Render for Recorder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let content = self.content(&t, cx);
        let el = if self.framed { dialog_card(&t, 420.).p(px(22.)).child(content) } else { content };
        el.id("hotkey-recorder")
            .key_context("HotkeyRecorder")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_modifiers_changed(cx.listener(Self::on_modifiers))
    }
}
