//! You and the server: your picture and display name, signing out. The server's own settings
//! live in `admin.rs`.

use super::ServerView;
use crate::core::types::{ErrorCode, User};
use crate::core::{self};
use crate::prefs::set_prefs;
use crate::session::Session;
use crate::ui::overlay::{Ask, toast};
use gpui::{Context, PathPromptOptions, Task, Window};

fn set_me(s: &mut Session, user: User, cx: &mut Context<Session>) {
    s.me = user.clone();
    s.users.insert(user.id, user);
    cx.notify();
}

impl ServerView {
    /// Picks a picture, crops it to a 256 px square, and makes it your avatar.
    pub fn pick_avatar(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("Use this picture", "Usar esta imagem").into()),
        });
        let session = self.session.clone();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let jpeg = core::run(async move { avatar_jpeg(&path) }).await;
            cx.update(|cx| match jpeg {
                Ok(bytes) => session.update(cx, |s, cx| {
                    s.call(
                        cx,
                        move |api| {
                            Box::pin(async move {
                                let up = api.upload(bytes, "image/jpeg").await?;
                                api.set_avatar(Some(&up.hash)).await
                            })
                        },
                        set_me,
                    )
                }),
                Err(e) => toast(e, cx),
            });
        })
        .detach();
    }

    pub fn remove_avatar(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |s, cx| s.call(cx, |api| Box::pin(async move { api.set_avatar(None).await }), set_me));
    }

    /// An empty name goes back to the username.
    pub fn set_display_name(&mut self, name: String, cx: &mut Context<Self>) {
        self.session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.set_display_name(&name).await }), set_me));
    }

    /// Resolves to what went wrong, or `None` once the password is changed. The server ends every
    /// sign-in of the account, so this one carries on with the new token it hands back.
    pub fn change_password(&mut self, current: String, password: String, cx: &mut Context<Self>) -> Task<Option<String>> {
        let api = self.session.read(cx).api.clone();
        cx.spawn(async move |_, cx| {
            let a = api.clone();
            match core::run(async move { a.change_password(&current, &password).await }).await {
                Ok(token) => {
                    api.set_token(&token);
                    cx.update(|cx| {
                        set_prefs(cx, |p| {
                            if !p.session_token.is_empty() {
                                p.session_token = token;
                            }
                        })
                    });
                    None
                }
                Err(e) => Some(match e.code {
                    ErrorCode::BadCredentials => tr!("Your current password is not right.", "A senha atual está errada.").into(),
                    ErrorCode::WeakPassword => tr!("Use at least 6 characters.", "Use pelo menos 6 caracteres.").into(),
                    _ => e.message,
                }),
            }
        })
    }

    /// Asks first, since the saved sign-in on this computer goes with it.
    pub fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let root = self.root.clone();
        let server = session.read(cx).server_name.clone();
        Ask::confirm_action(
            tr!("Sign out?", "Sair da conta?"),
            trf!("You will need your password to get back into {}.", "Você vai precisar da sua senha para entrar de novo em {}.", server),
            tr!("Sign out", "Sair da conta"),
            window,
            cx,
            move |window, cx| {
                let api = session.read(cx).api.clone();
                cx.background_executor()
                    .spawn(async move {
                        let _ = core::run(async move { api.logout().await }).await;
                    })
                    .detach();
                set_prefs(cx, |p| p.session_token.clear());
                if let Some(root) = root.upgrade() {
                    root.update(cx, |r, cx| r.show_connect(None, window, cx));
                }
            },
        );
    }
}

/// The old client's avatar: centre-cropped to 256×256, JPEG at 85%, or 60% if that is over 256 KB.
/// The server's picture is made the same way and held to the same limit.
pub(super) fn avatar_jpeg(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let img = image::open(path).map_err(|_| {
        tr!("That file is not a picture Harmony can read.", "Esse arquivo não é uma imagem que o Harmony consiga ler.").to_string()
    })?;
    let side = img.width().min(img.height());
    let (x, y) = ((img.width() - side) / 2, (img.height() - side) / 2);
    let square = img.crop_imm(x, y, side, side).resize_exact(256, 256, image::imageops::FilterType::Lanczos3).to_rgb8();
    for quality in [85u8, 60] {
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        enc.encode_image(&square).map_err(|e| e.to_string())?;
        if out.len() <= 256 * 1024 || quality == 60 {
            return Ok(out);
        }
    }
    unreachable!()
}
