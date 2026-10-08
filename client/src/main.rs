#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// `ApiError` carries the server's reply for callers that branch on it; boxing it everywhere
// would only add noise.
#![allow(clippy::result_large_err)]

//! Harmony's desktop client: voice channels with cameras and screen sharing, text channels, and
//! the server's people, drawn with GPUI.

#[macro_use]
mod i18n;
mod core;
mod emoji;
mod icons;
mod markdown;
mod media;
mod prefs;
mod session;
mod text_field;
mod theme;
mod ui;
mod widgets;

use gpui::{AppContext, Bounds, QuitMode, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions, size};

/// 1180×760, or less on a small screen, so the whole window and its title bar start in view
/// with room around them.
fn first_size(cx: &gpui::App) -> gpui::Size<gpui::Pixels> {
    let want = size(gpui::px(1180.), gpui::px(760.));
    let Some(display) = cx.primary_display() else { return want };
    let room = display.visible_bounds().size;
    size(want.width.min(room.width * 0.85), want.height.min(room.height * 0.8))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("harmony=info,warn")).init();
    let smoke = std::env::args().any(|a| a == "--smoke");
    let store = core::settings::Store::load();
    i18n::set(i18n::resolve(&store.values.language));

    gpui_platform::application().with_quit_mode(QuitMode::LastWindowClosed).run(move |cx: &mut gpui::App| {
        if let Err(e) = cx.text_system().add_fonts(theme::fonts()) {
            log::warn!("fonts did not load: {e}");
        }
        text_field::bind_keys(cx);
        cx.set_global(prefs::Prefs(store));
        ui::updates::init(cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, first_size(cx), cx))),
            titlebar: Some(TitlebarOptions { title: Some("Harmony".into()), ..Default::default() }),
            app_id: Some("harmony".into()),
            window_min_size: Some(size(gpui::px(640.), gpui::px(480.))),
            window_background: WindowBackgroundAppearance::Opaque,
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let root = cx.new(|cx| ui::Root::new(window, cx));
            cx.set_global(ui::overlay::RootHandle(root.downgrade()));
            root
        })
        .expect("open the window");
        cx.activate(true);

        if smoke {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                println!("smoke: window opened");
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
}
