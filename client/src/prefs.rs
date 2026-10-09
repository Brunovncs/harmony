//! The settings file as a GPUI global: read anywhere with `prefs(cx)`, changed with `set_prefs`,
//! which lets observers redraw and saves. Saving happens off the UI thread; a slider's level is
//! saved once it settles rather than on every step of a drag.

use crate::core::settings::{self, Settings, Store};
use gpui::{App, BorrowAppContext, Global, Task};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// How long a dragged level has to stay put before it is written.
const SETTLE: Duration = Duration::from_millis(400);

pub struct Prefs {
    store: Store,
    /// Whether the last change needs the whole window drawn again; a level does not.
    pub redraw: bool,
    /// A level waiting to settle; replaced by each change.
    pending: Option<Task<()>>,
    /// The newest snapshot taken and the newest written, so a slow write never lands over a
    /// newer one.
    taken: u64,
    written: Arc<Mutex<u64>>,
}

impl Global for Prefs {}

impl Prefs {
    pub fn new(store: Store) -> Prefs {
        Prefs { store, redraw: true, pending: None, taken: 0, written: Default::default() }
    }

    /// Writes the values as they are now on the background executor.
    fn save(&mut self, cx: &App) {
        self.pending = None;
        let (text, generation, written) = self.snapshot();
        let path = self.store.path().to_path_buf();
        cx.background_executor().spawn(async move { write(&path, &text, generation, &written) }).detach();
    }

    fn snapshot(&mut self) -> (String, u64, Arc<Mutex<u64>>) {
        self.taken += 1;
        (self.store.snapshot(), self.taken, self.written.clone())
    }
}

fn write(path: &std::path::Path, text: &str, generation: u64, written: &Mutex<u64>) {
    let mut last = written.lock();
    if generation > *last {
        settings::write_file(path, text);
        *last = generation;
    }
}

pub fn prefs(cx: &App) -> &Settings {
    &cx.global::<Prefs>().store.values
}

pub fn set_prefs(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    cx.update_global::<Prefs, _>(|p, cx| {
        let before = p.store.values.clone();
        f(&mut p.store.values);
        p.store.values.sync_active();
        if p.store.values == before {
            p.redraw = false;
            return;
        }
        p.redraw = !p.store.values.same_but_levels(&before);
        if p.redraw {
            p.save(cx);
        } else {
            p.pending = Some(cx.spawn(async |cx| {
                cx.background_executor().timer(SETTLE).await;
                cx.update(|cx| cx.update_global::<Prefs, _>(|p, cx| p.save(cx)));
            }));
        }
    });
}

/// Writes a level still settling before Harmony quits.
pub fn init(cx: &mut App) {
    cx.on_app_quit(|cx| {
        let p = cx.global_mut::<Prefs>();
        if p.pending.take().is_some() {
            let (text, generation, written) = p.snapshot();
            write(p.store.path(), &text, generation, &written);
        }
        async {}
    })
    .detach();
}
