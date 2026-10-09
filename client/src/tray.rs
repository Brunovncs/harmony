//! The notification-area icon, and one Harmony at a time. While the icon is there, closing the
//! window only hides it: the session and any call go on, a click on the icon brings the window
//! back and its menu quits for real. As in OpenController, starting Harmony again shows the
//! running copy's window instead; "one" is per settings folder, so copies started with their own
//! `HARMONY_DATA_DIR` still run side by side.
//!
//! Plain Win32. The icon lives on a thread of its own with a hidden window for the shell's
//! messages, so its menu, which runs a modal loop until it closes, never holds up the window.
//! Elsewhere than Windows there is no icon and closing the window quits.

/// What the icon, and a second start, ask of the window.
pub enum Event {
    /// A click on the icon or on its notice, "Open" in its menu, or a second start.
    Open,
    /// A right click on the icon: the menu waits for `Tray::menu`.
    Menu,
    /// The pointer is over the icon, so its tooltip is about to show.
    Hover,
    Mute,
    Deafen,
    Quit,
}

/// What the menu offers besides opening and quitting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Menu {
    /// In a call: whether muted, and whether deafened.
    pub call: Option<(bool, bool)>,
}

pub use platform::{Instance, Tray, flash_window, hide_window, show_window};

#[cfg(windows)]
mod platform {
    use super::{Event, Menu};
    use parking_lot::Mutex;
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
        NIN_BALLOONUSERCLICK, NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, GetMessageW, GetSystemMetrics, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, MENU_ITEM_FLAGS, MF_CHECKED,
        MF_SEPARATOR, MF_STRING, MSG, PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_SHOW,
        SetForegroundWindow, SetMenuDefaultItem, ShowWindowAsync, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
        TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_MOUSEMOVE, WM_NULL, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    /// The shell's messages about the icon.
    const CALLBACK: u32 = WM_APP + 1;
    /// "Show `pending.tip`", "pop up the menu `pending.menu` describes", "show `pending.notice`".
    const SET_TIP: u32 = WM_APP + 2;
    const POPUP: u32 = WM_APP + 3;
    const NOTICE: u32 = WM_APP + 4;
    const ICON_ID: u32 = 1;
    /// The icon chosen with the keyboard and Enter; the crate has no name for it.
    const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
    /// The menu's commands.
    const OPEN: usize = 1;
    const MUTE: usize = 2;
    const DEAFEN: usize = 3;
    const QUIT: usize = 4;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Copies `s` into one of the shell's fixed buffers, cut to fit.
    fn fill(buf: &mut [u16], s: &str) {
        let room = buf.len() - 1;
        let mut n = 0;
        for (slot, unit) in buf.iter_mut().zip(s.encode_utf16().take(room)) {
            *slot = unit;
            n += 1;
        }
        buf[n] = 0;
    }

    /// The settings folder's claim: a mutex this (the main) thread holds until Harmony exits,
    /// and the event another start signals to have the window shown.
    pub struct Instance {
        show: Option<isize>,
    }

    impl Instance {
        /// `None` when another Harmony runs on this settings folder: it has been asked to show its
        /// window and this one should exit. After an update restarts Harmony (`wait`), the copy
        /// being replaced is given time to finish quitting first.
        pub fn claim(data_dir: &Path, wait: bool) -> Option<Instance> {
            let hash = hex::encode(Sha256::digest(data_dir.to_string_lossy().to_lowercase().as_bytes()));
            let name = format!(r"Local\Harmony-{}", &hash[..16]);
            let (mutex, show) = (wide(&name), wide(&format!("{name}-show")));
            // SAFETY: plain Win32 calls with nul-terminated names that outlive them. The handles
            // stay open for the life of the process.
            unsafe {
                let Ok(m) = CreateMutexW(None, false, PCWSTR(mutex.as_ptr())) else {
                    log::warn!("single instance: no mutex, running anyway");
                    return Some(Instance { show: None });
                };
                // An abandoned mutex is one whose holder exited without letting go: it is ours.
                let got = WaitForSingleObject(m, if wait { 15_000 } else { 0 });
                if got != WAIT_OBJECT_0 && got != WAIT_ABANDONED {
                    if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(show.as_ptr())) {
                        // The running copy may take the foreground, which this start has from the
                        // click that launched it.
                        let _ = AllowSetForegroundWindow(ASFW_ANY);
                        let _ = SetEvent(ev);
                    }
                    return None;
                }
                let show = CreateEventW(None, false, false, PCWSTR(show.as_ptr())).ok().map(|h| h.0 as isize);
                Some(Instance { show })
            }
        }

        /// Sends `Event::Open` each time another start asks for the window.
        pub fn on_show(&self, events: async_channel::Sender<Event>) {
            let Some(show) = self.show else { return };
            let spawned = std::thread::Builder::new().name("harmony-instance".into()).spawn(move || {
                // SAFETY: the event stays open for the life of the process.
                while unsafe { WaitForSingleObject(HANDLE(show as _), INFINITE) } == WAIT_OBJECT_0 {
                    if events.send_blocking(Event::Open).is_err() {
                        break;
                    }
                }
            });
            if spawned.is_err() {
                log::warn!("single instance: no thread to hear other starts");
            }
        }
    }

    /// What the window asked for last, read by the icon's thread when the message saying so
    /// arrives.
    #[derive(Default)]
    struct Pending {
        tip: String,
        menu: Menu,
        notice: (String, String),
    }

    struct Shared {
        events: async_channel::Sender<Event>,
        pending: Mutex<Pending>,
        icon: isize,
        /// Sent to every top-level window when Explorer starts again, which forgets every icon.
        taskbar_created: u32,
    }

    static SHARED: OnceLock<Shared> = OnceLock::new();

    thread_local! {
        /// Where the shell asked for the menu: the pointer, or the icon when asked from the keyboard.
        static ANCHOR: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
        static HOVERED: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    /// The icon's thread and window.
    pub struct Tray {
        hwnd: isize,
    }

    impl Tray {
        pub fn start(events: async_channel::Sender<Event>) -> Option<Tray> {
            let (ready, hwnd) = std::sync::mpsc::channel();
            std::thread::Builder::new().name("harmony-tray".into()).spawn(move || run(events, ready)).ok()?;
            hwnd.recv().ok().flatten().map(|hwnd| Tray { hwnd })
        }

        fn post(&self, msg: u32, f: impl FnOnce(&mut Pending)) {
            if let Some(s) = SHARED.get() {
                f(&mut s.pending.lock());
            }
            // SAFETY: the icon's window lives as long as the process.
            if unsafe { PostMessageW(Some(HWND(self.hwnd as _)), msg, WPARAM(0), LPARAM(0)) }.is_err() {
                log::warn!("tray: the icon's thread did not take a message");
            }
        }

        pub fn set_tip(&self, tip: &str) {
            self.post(SET_TIP, |p| p.tip = tip.into());
        }

        /// Pops up the menu the last right click asked for.
        pub fn menu(&self, menu: Menu) {
            self.post(POPUP, |p| p.menu = menu);
        }

        /// A notification from the icon, which Windows shows as a toast.
        pub fn notify(&self, title: &str, text: &str) {
            self.post(NOTICE, |p| p.notice = (title.into(), text.into()));
        }

        /// Takes the icon away now: one left behind stays until the pointer passes over it.
        pub fn remove(&self) {
            // SAFETY: the shell is told by window and id; any thread may do so.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data(HWND(self.hwnd as _)));
            }
        }
    }

    pub fn hide_window(hwnd: isize) {
        // SAFETY: asynchronous, so the window's own procedure does not run inside the caller's
        // update of the app.
        unsafe {
            let _ = ShowWindowAsync(HWND(hwnd as _), SW_HIDE);
        }
    }

    pub fn show_window(hwnd: isize) {
        // SAFETY: as in `hide_window`.
        unsafe {
            let _ = ShowWindowAsync(HWND(hwnd as _), SW_SHOW);
        }
    }

    /// Flashes the window's taskbar button until it is brought to the front, as a call should.
    pub fn flash_window(hwnd: isize) {
        use windows::Win32::UI::WindowsAndMessaging::{FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, FlashWindowEx};
        let info = FLASHWINFO {
            cbSize: size_of::<FLASHWINFO>() as u32,
            hwnd: HWND(hwnd as _),
            dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        // SAFETY: `info` is a complete FLASHWINFO for this window, read during the call only.
        unsafe {
            let _ = FlashWindowEx(&info);
        }
    }

    fn data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ICON_ID, ..Default::default() }
    }

    fn add(hwnd: HWND, s: &Shared) -> bool {
        let mut d = data(hwnd);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.uCallbackMessage = CALLBACK;
        d.hIcon = HICON(s.icon as _);
        fill(&mut d.szTip, &s.pending.lock().tip);
        d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: `d` is a complete NOTIFYICONDATAW for this window's icon.
        unsafe { Shell_NotifyIconW(NIM_ADD, &d).as_bool() && Shell_NotifyIconW(NIM_SETVERSION, &d).as_bool() }
    }

    fn run(events: async_channel::Sender<Event>, ready: std::sync::mpsc::Sender<Option<isize>>) {
        // SAFETY: plain Win32 calls on this thread's own window and queue; every pointer passed
        // outlives its call.
        unsafe {
            let Ok(module) = GetModuleHandleW(None) else {
                let _ = ready.send(None);
                return;
            };
            // Resource 1 is Harmony's own icon, at the size the notification area draws.
            let icon = LoadImageW(
                Some(module.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            )
            .map_or(0, |h| h.0 as isize);
            let _ = SHARED.set(Shared {
                events,
                pending: Mutex::new(Pending { tip: "Harmony".into(), ..Default::default() }),
                icon,
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            });
            let class = w!("HarmonyTray");
            let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: module.into(), lpszClassName: class, ..Default::default() };
            RegisterClassW(&wc);
            // A real top-level window, never shown, rather than a message-only one: only those
            // hear that Explorer restarted.
            let hwnd = match CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class,
                w!("Harmony tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(module.into()),
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    log::warn!("tray: no window ({e})");
                    let _ = ready.send(None);
                    return;
                }
            };
            if icon == 0 || !SHARED.get().is_some_and(|s| add(hwnd, s)) {
                log::warn!("tray: the notification area did not take the icon");
                let _ = DestroyWindow(hwnd);
                let _ = ready.send(None);
                return;
            }
            let _ = ready.send(Some(hwnd.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let Some(s) = SHARED.get() else { return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) } };
        let send = |e| {
            let _ = s.events.try_send(e);
        };
        // SAFETY: the shell's structures are filled for this window's own icon.
        unsafe {
            match msg {
                // With version 4 the low word is what happened and the anchor is in wParam.
                CALLBACK => match (lparam.0 & 0xFFFF) as u32 {
                    NIN_SELECT | NIN_KEYSELECT | NIN_BALLOONUSERCLICK => send(Event::Open),
                    WM_CONTEXTMENU => {
                        ANCHOR.set(((wparam.0 & 0xFFFF) as i16 as i32, ((wparam.0 >> 16) & 0xFFFF) as i16 as i32));
                        send(Event::Menu);
                    }
                    // Plenty while the pointer rests there; once a second keeps the tooltip current.
                    WM_MOUSEMOVE if HOVERED.get().is_none_or(|t| t.elapsed() > Duration::from_secs(1)) => {
                        HOVERED.set(Some(Instant::now()));
                        send(Event::Hover);
                    }
                    _ => {}
                },
                SET_TIP => {
                    let mut d = data(hwnd);
                    d.uFlags = NIF_TIP | NIF_SHOWTIP;
                    fill(&mut d.szTip, &s.pending.lock().tip);
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
                }
                POPUP => popup(hwnd, s),
                NOTICE => {
                    let (title, text) = std::mem::take(&mut s.pending.lock().notice);
                    let mut d = data(hwnd);
                    d.uFlags = NIF_INFO;
                    fill(&mut d.szInfoTitle, &title);
                    fill(&mut d.szInfo, &text);
                    d.dwInfoFlags = NIIF_USER;
                    d.hBalloonIcon = HICON(s.icon as _);
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
                }
                m if m == s.taskbar_created => {
                    add(hwnd, s);
                }
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        LRESULT(0)
    }

    /// Runs the menu where the shell asked for it, and sends what was picked.
    fn popup(hwnd: HWND, s: &Shared) {
        let menu = s.pending.lock().menu;
        // SAFETY: the menu is made, shown and destroyed here, on the thread that owns `hwnd`.
        unsafe {
            let Ok(m) = CreatePopupMenu() else { return };
            let item = |flags: MENU_ITEM_FLAGS, id: usize, text: &str| {
                let text = wide(text);
                let _ = AppendMenuW(m, MF_STRING | flags, id, PCWSTR(text.as_ptr()));
            };
            let separator = || {
                let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
            };
            item(MENU_ITEM_FLAGS(0), OPEN, tr!("Open Harmony", "Abrir Harmony"));
            let _ = SetMenuDefaultItem(m, OPEN as u32, 0);
            if let Some((muted, deafened)) = menu.call {
                let check = |on: bool| if on { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
                separator();
                item(check(muted), MUTE, tr!("Mute", "Silenciar"));
                item(check(deafened), DEAFEN, tr!("Deafen", "Desativar o som"));
            }
            separator();
            item(MENU_ITEM_FLAGS(0), QUIT, tr!("Quit Harmony", "Sair do Harmony"));
            // Without the foreground the menu would not close when clicking elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let (x, y) = ANCHOR.get();
            let picked = TrackPopupMenuEx(m, (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY).0, x, y, hwnd, None);
            // The menu closes properly on the next message the window gets.
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(m);
            let event = match picked.0 as usize {
                OPEN => Event::Open,
                MUTE => Event::Mute,
                DEAFEN => Event::Deafen,
                QUIT => Event::Quit,
                _ => return,
            };
            let _ = s.events.try_send(event);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Event, Menu};
    use std::path::Path;

    pub struct Instance;

    impl Instance {
        pub fn claim(_: &Path, _: bool) -> Option<Instance> {
            Some(Instance)
        }

        pub fn on_show(&self, _: async_channel::Sender<Event>) {}
    }

    pub struct Tray;

    impl Tray {
        pub fn start(_: async_channel::Sender<Event>) -> Option<Tray> {
            None
        }

        pub fn set_tip(&self, _: &str) {}

        pub fn menu(&self, _: Menu) {}

        pub fn notify(&self, _: &str, _: &str) {}

        pub fn remove(&self) {}
    }

    pub fn hide_window(_: isize) {}

    pub fn show_window(_: isize) {}

    pub fn flash_window(_: isize) {}
}
