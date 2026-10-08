//! Turning your camera and your screen on and off in a call.

use super::ServerView;
use super::picker::Picker;
use crate::ui::camera::CameraDialog;
use crate::ui::overlay::{self, toast};
use gpui::{AppContext, Context, Window};

impl ServerView {
    fn voice(&self, cx: &gpui::App) -> Option<gpui::Entity<crate::media::voice::Voice>> {
        self.call.as_ref().and_then(|c| c.read(cx).voice.clone())
    }

    /// Off in one click; on through the preview, to pick the camera and background first.
    pub fn toggle_camera(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(voice) = self.voice(cx) else {
            toast(tr!("Join a voice channel first.", "Entre em um canal de voz primeiro."), cx);
            return;
        };
        if voice.read(cx).camera_on() {
            voice.update(cx, |v, cx| v.stop_camera(cx));
            return;
        }
        let this = cx.entity().downgrade();
        CameraDialog::open(voice, window, cx, move |cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.ensure_stage(cx));
            }
        });
    }

    pub fn toggle_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(voice) = self.voice(cx) else {
            toast(tr!("Join a voice channel first.", "Entre em um canal de voz primeiro."), cx);
            return;
        };
        if voice.read(cx).screen_on() {
            voice.update(cx, |v, cx| v.stop_screen(cx));
            return;
        }
        self.pick_screen(window, cx);
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let voice = self.voice(cx);
        crate::ui::settings::open(voice, self.session.clone(), cx.entity().downgrade(), window, cx);
    }

    /// The picker, to start sharing or to change what is shared.
    pub fn pick_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(voice) = self.voice(cx) else { return };
        let current = voice.read(cx).screen_choice();
        let this = cx.entity().downgrade();
        let picker = cx.new(|cx| {
            Picker::new(current, cx, move |choice, _, cx| {
                voice.update(cx, |v, cx| v.start_screen(choice, cx));
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| this.ensure_stage(cx));
                }
            })
        });
        overlay::open_dialog(picker, window, cx);
    }
}
