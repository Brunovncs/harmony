//! Closing the window to the notification area (the icon itself is `crate::tray`), and what the
//! icon asks for: the window back, mute and deafen in a call, quitting.

use super::Screen;
use super::overlay::RootHandle;
use super::server::ServerView;
use crate::core::settings::HotkeyAction;
use crate::prefs::{prefs, set_prefs};
use crate::tray::{Event, Instance, Menu, Tray};
use gpui::{AnyWindowHandle, App, Entity, Global, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

struct TrayState {
    tray: Option<Tray>,
    window: AnyWindowHandle,
    /// The tooltip last given to the icon.
    tip: String,
}

impl Global for TrayState {}

pub fn init(window: AnyWindowHandle, instance: &Instance, cx: &mut App) {
    let (tx, rx) = async_channel::unbounded();
    instance.on_show(tx.clone());
    let tray = Tray::start(tx);
    if tray.is_none() {
        log::info!("no notification-area icon here: closing the window quits");
    }
    cx.set_global(TrayState { tray, window, tip: String::new() });
    let _ = window.update(cx, |_, window, cx| window.on_window_should_close(cx, on_close));
    cx.on_app_quit(|cx| {
        if let Some(tray) = &cx.global::<TrayState>().tray {
            tray.remove();
        }
        async {}
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Ok(ev) = rx.recv().await {
            cx.update(|cx| on_event(ev, cx));
        }
    })
    .detach();
}

fn hwnd(window: &Window) -> Option<isize> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// The window's close button. With the icon there and the setting on, the window only hides;
/// otherwise Harmony quits, keeping the window until then so a call is left properly.
fn on_close(window: &mut Window, cx: &mut App) -> bool {
    let hide = cx.global::<TrayState>().tray.is_some() && prefs(cx).close_to_tray;
    let Some(hwnd) = hwnd(window).filter(|_| hide) else {
        cx.quit();
        return false;
    };
    crate::tray::hide_window(hwnd);
    if !prefs(cx).tray_notice_seen {
        set_prefs(cx, |p| p.tray_notice_seen = true);
        if let Some(tray) = &cx.global::<TrayState>().tray {
            tray.notify(
                tr!("Harmony is still running in the tray", "O Harmony continua rodando na bandeja"),
                tr!(
                    "Calls and messages keep going. Click the icon to open it again, or right-click it to quit.",
                    "Chamadas e mensagens continuam. Clique no ícone para abrir de novo, ou com o botão direito para sair."
                ),
            );
        }
    }
    false
}

/// Something needs you while the window is in the background: a call, a private message. The
/// taskbar button flashes and, from the tray, Windows shows a notification. Says only who, never
/// what: a notification is on a screen anyone nearby can read.
pub fn attention(window: &Window, title: &str, text: &str, cx: &App) {
    if window.is_window_active() {
        return;
    }
    if let Some(hwnd) = hwnd(window) {
        crate::tray::flash_window(hwnd);
    }
    if let Some(tray) = cx.try_global::<TrayState>().and_then(|s| s.tray.as_ref()) {
        tray.notify(title, text);
    }
}

fn show(cx: &mut App) {
    let window = cx.global::<TrayState>().window;
    let _ = window.update(cx, |_, window, _| {
        if let Some(hwnd) = hwnd(window) {
            crate::tray::show_window(hwnd);
        }
        window.activate_window();
    });
}

fn server_view(cx: &App) -> Option<Entity<ServerView>> {
    let root = cx.try_global::<RootHandle>()?.0.upgrade()?;
    match &root.read(cx).screen {
        Screen::Server(v) => Some(v.clone()),
        Screen::Connect { .. } => None,
    }
}

/// Muted and deafened, in a call.
fn call_state(cx: &App) -> Option<(bool, bool)> {
    let view = server_view(cx)?;
    let call = view.read(cx).call.as_ref()?.read(cx);
    Some((call.muted, call.deafened))
}

/// "Harmony", then the server and the call, one to a line.
fn tip(cx: &App) -> String {
    let Some(view) = server_view(cx) else { return "Harmony".into() };
    let view = view.read(cx);
    let session = view.session.read(cx);
    let mut tip = format!("Harmony\n{}", session.server_name);
    if let Some(call) = &view.call {
        let call = call.read(cx);
        let channel = session.place_name(call.place);
        tip.push('\n');
        tip.push_str(&trf!("In call: {}", "Em chamada: {}", channel));
        if call.deafened {
            tip.push_str(&format!(" · {}", tr!("Deafened", "Som desativado")));
        } else if call.muted {
            tip.push_str(&format!(" · {}", tr!("Muted", "Silenciado")));
        }
    }
    tip
}

fn on_event(ev: Event, cx: &mut App) {
    match ev {
        Event::Open => show(cx),
        Event::Menu => {
            let menu = Menu { call: call_state(cx) };
            if let Some(tray) = &cx.global::<TrayState>().tray {
                tray.menu(menu);
            }
        }
        Event::Hover => {
            let tip = tip(cx);
            let state = cx.global_mut::<TrayState>();
            if state.tip != tip
                && let Some(tray) = &state.tray
            {
                tray.set_tip(&tip);
                state.tip = tip;
            }
        }
        Event::Mute | Event::Deafen => {
            let action = if matches!(ev, Event::Mute) { HotkeyAction::Mute } else { HotkeyAction::Deafen };
            if let Some(view) = server_view(cx) {
                view.update(cx, |v, cx| v.on_hotkey(action, cx));
            }
        }
        Event::Quit => cx.quit(),
    }
}
