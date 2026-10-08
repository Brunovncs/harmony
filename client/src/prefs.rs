//! The settings file as a GPUI global: read anywhere with `prefs(cx)`, changed with `set_prefs`,
//! which saves and lets observers redraw.

use crate::core::settings::{Settings, Store};
use gpui::{App, BorrowAppContext, Global};

pub struct Prefs(pub Store);

impl Global for Prefs {}

pub fn prefs(cx: &App) -> &Settings {
    &cx.global::<Prefs>().0.values
}

pub fn set_prefs(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    cx.update_global::<Prefs, _>(|p, _| p.0.update(f));
}
