//! The Profile section: who you are on this server, what your role lets you do, how you are
//! connected, and the account's own actions. Only what the client knows or the server says;
//! nothing is guessed.

use super::{Section, Settings};
use crate::core::types::Role;
use crate::media::rtc::Route;
use crate::session::{Link, Session};
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{MONO, Theme, px, radius};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, Entity, Focusable, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, linear_color_stop, linear_gradient,
};

/// The editing state the section keeps between frames.
pub struct Profile {
    name: Entity<TextField>,
    editing_name: bool,
    /// Current, new, and the new one again.
    password: [Entity<TextField>; 3],
    password_open: bool,
    password_busy: bool,
    /// What the last attempt came to: done, or what went wrong.
    password_note: Option<(bool, String)>,
}

impl Profile {
    pub fn new(session: &Entity<Session>, cx: &mut Context<Settings>) -> Profile {
        let nickname = session.read(cx).me.nickname.clone();
        let name = cx.new(|cx| TextField::new(cx, false, 32).placeholder(nickname));
        cx.subscribe(&name, |s: &mut Settings, _, ev: &TextFieldEvent, cx| {
            if let TextFieldEvent::Submit = ev {
                s.save_name(cx);
            }
        })
        .detach();
        let secret = |cx: &mut Context<Settings>| {
            let f = cx.new(|cx| {
                let mut f = TextField::new(cx, false, 128);
                f.set_masked(true);
                f
            });
            cx.subscribe(&f, |s: &mut Settings, _, ev: &TextFieldEvent, cx| match ev {
                TextFieldEvent::Submit => s.save_password(cx),
                TextFieldEvent::Changed => {
                    if s.profile.password_note.take().is_some() {
                        cx.notify();
                    }
                }
            })
            .detach();
            f
        };
        let password = [secret(cx), secret(cx), secret(cx)];
        Profile { name, editing_name: false, password, password_open: false, password_busy: false, password_note: None }
    }
}

impl Settings {
    fn edit_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me.clone();
        let current = if me.custom_name { me.name().to_string() } else { String::new() };
        self.profile.name.update(cx, |f, cx| f.set_text(&current, cx));
        self.profile.editing_name = true;
        self.profile.name.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn save_name(&mut self, cx: &mut Context<Self>) {
        if !self.profile.editing_name {
            return;
        }
        let name = self.profile.name.read(cx).text().trim().to_string();
        let me = &self.session.read(cx).me;
        let unchanged = if me.custom_name { name == me.name() } else { name.is_empty() };
        if !unchanged && let Some(server) = self.server.upgrade() {
            server.update(cx, |s, cx| s.set_display_name(name, cx));
        }
        self.profile.editing_name = false;
        cx.notify();
    }

    fn toggle_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let p = &mut self.profile;
        p.password_open = !p.password_open;
        p.password_note = None;
        for f in &p.password {
            f.update(cx, |f, cx| f.clear(cx));
        }
        if p.password_open {
            p.password[0].focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn save_password(&mut self, cx: &mut Context<Self>) {
        if self.profile.password_busy || !self.profile.password_open {
            return;
        }
        let [current, new, again] = self.profile.password.clone().map(|f| f.read(cx).text());
        let problem = if current.is_empty() {
            Some(tr!("Type your current password first.", "Digite primeiro a sua senha atual."))
        } else if new.chars().count() < 6 {
            Some(tr!("Use at least 6 characters.", "Use pelo menos 6 caracteres."))
        } else if new != again {
            Some(tr!("The two new passwords are not the same.", "As duas senhas novas não são iguais."))
        } else {
            None
        };
        if let Some(problem) = problem {
            self.profile.password_note = Some((false, problem.into()));
            cx.notify();
            return;
        }
        let Some(server) = self.server.upgrade() else { return };
        let task = server.update(cx, |s, cx| s.change_password(current, new, cx));
        self.profile.password_busy = true;
        cx.spawn(async move |this, cx| {
            let failed = task.await;
            let _ = this.update(cx, |s, cx| {
                let p = &mut s.profile;
                p.password_busy = false;
                p.password_note = Some(match failed {
                    Some(e) => (false, e),
                    None => {
                        p.password_open = false;
                        for f in &p.password {
                            f.update(cx, |f, cx| f.clear(cx));
                        }
                        (
                            true,
                            tr!(
                                "Password changed. Your other computers will need to sign in again.",
                                "Senha trocada. Seus outros computadores vão precisar entrar de novo."
                            )
                            .into(),
                        )
                    }
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn profile_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let me = self.session.read(cx).me.clone();
        let img = self.session.update(cx, |s, cx| s.avatar(me.id, cx));
        let s = self.session.read(cx);
        let (server_name, base, link_up) = (s.server_name.clone(), s.api.base(), s.link == Link::Up);
        let call = self.voice.as_ref().map(|v| v.read(cx)).map(|v| (s.channel(v.channel).map(|c| c.name.clone()), v.ping_ms, v.route));
        let p = crate::prefs::prefs(cx).clone();

        let (state, state_color) = match (&call, link_up) {
            (_, false) => (tr!("Reconnecting…", "Reconectando…").to_string(), t.caution),
            (Some((Some(ch), _, _)), true) => (trf!("In voice · {}", "Na voz · {}", ch), t.success),
            _ => (tr!("Online", "Online").to_string(), t.success),
        };
        let (role_name, role_color, role_note) = match me.role {
            Role::Owner => (tr!("Owner", "Dono"), t.caution, tr!("Full control of this server", "Controle total deste servidor")),
            Role::Admin => (tr!("Admin", "Admin"), t.accent, tr!("Helps run this server", "Ajuda a cuidar deste servidor")),
            Role::Member => (tr!("Member", "Membro"), t.text2, tr!("Chats, talks and shares here", "Conversa, fala e compartilha aqui")),
        };

        // The header: a band in the accent, your picture over it, your name and where you stand.
        let ring = t.layer;
        let picture = div()
            .id("profile-avatar")
            .group("profile-avatar")
            .relative()
            .flex_none()
            .size(px(96.))
            .p(px(4.))
            .rounded_full()
            .bg(ring)
            .cursor_pointer()
            .child(avatar(me.name(), img, 88., None))
            .child(
                div()
                    .absolute()
                    .top(px(4.))
                    .left(px(4.))
                    .size(px(88.))
                    .rounded_full()
                    .bg(gpui::black().opacity(0.55))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(2.))
                    .opacity(0.)
                    .group_hover("profile-avatar", |s| s.opacity(1.))
                    .child(icon("camera", 20., gpui::white()))
                    .child(
                        div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(gpui::white()).child(tr!("Change", "Trocar")),
                    ),
            )
            .on_click(cx.listener(|s, _, _, cx| s.pick_avatar(cx)));

        let name_row = if self.profile.editing_name {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().w(px(240.)).child(self.profile.name.clone()))
                        .child(
                            button("name-save", tr!("Save", "Salvar"), Kind::Primary, t)
                                .on_click(cx.listener(|s, _, _, cx| s.save_name(cx))),
                        )
                        .child(button("name-cancel", tr!("Cancel", "Cancelar"), Kind::Subtle, t).on_click(cx.listener(|s, _, _, cx| {
                            s.profile.editing_name = false;
                            cx.notify();
                        }))),
                )
                .child(caption(
                    trf!(
                        "What everyone sees. Leave it empty to show your username, {}.",
                        "O que todos veem. Deixe vazio para mostrar seu nome de usuário, {}.",
                        me.nickname
                    ),
                    t.text3,
                ))
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .min_w(px(0.))
                .child(
                    div()
                        .truncate()
                        .text_size(px(22.))
                        .line_height(px(28.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(t.text)
                        .child(me.name().to_string()),
                )
                .child(
                    icon_button("name-edit", "edit", t)
                        .tooltip(tip(tr!("Change your display name", "Trocar seu nome de exibição"), t))
                        .on_click(cx.listener(|s, _, window, cx| s.edit_name(window, cx))),
                )
                .into_any_element()
        };

        let header = card(t)
            .child(div().h(px(78.)).bg(linear_gradient(
                100.,
                linear_color_stop(t.accent.opacity(0.75), 0.),
                linear_color_stop(t.accent.opacity(0.18), 1.),
            )))
            .child(
                div().flex().items_start().gap(px(16.)).px(px(18.)).child(div().flex_none().mt(px(-46.)).child(picture)).child(
                    div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(4.)).pt(px(10.)).child(name_row).child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(mono(format!("@{}", me.nickname), t.text2))
                            .child(chip(role_name, role_color))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .h(px(20.))
                                    .px(px(8.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(state_color.opacity(0.4))
                                    .child(status_dot(state_color, true))
                                    .child(div().font_family(MONO).text_size(px(10.5)).text_color(t.text2).child(state)),
                            ),
                    ),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(18.))
                    .pt(px(14.))
                    .pb(px(16.))
                    .child(
                        icon_label_button("avatar-pick", "image", tr!("Change picture", "Trocar foto"), Kind::Standard, t)
                            .on_click(cx.listener(|s, _, _, cx| s.pick_avatar(cx))),
                    )
                    .when(me.avatar_hash.is_some(), |d| {
                        d.child(
                            button("avatar-remove", tr!("Remove picture", "Remover foto"), Kind::Subtle, t)
                                .on_click(cx.listener(|s, _, _, cx| s.remove_avatar(cx))),
                        )
                    })
                    .child(div().flex_1())
                    .child(caption(tr!("A square picture works best.", "Uma imagem quadrada fica melhor."), t.text3)),
            );

        // The facts, as tiles.
        let (joined, ago) = joined(me.created_at);
        let secure = base.starts_with("https://");
        let (voice_value, voice_note): (SharedString, SharedString) = match &call {
            Some((_, Some(ms), route)) => (
                trf!("{} ms · {}", "{} ms · {}", ms, quality(*ms)).into(),
                match route {
                    Some(Route::Udp) => tr!("Direct, over UDP", "Direta, por UDP"),
                    Some(Route::Tcp) => tr!("Over TCP, a little slower", "Por TCP, um pouco mais lenta"),
                    Some(Route::Relay) => tr!("Through a relay server", "Por um servidor de retransmissão"),
                    None => tr!("Working out the route…", "Descobrindo o caminho…"),
                }
                .into(),
            ),
            Some((_, None, _)) => (tr!("Measuring…", "Medindo…").into(), tr!("Takes a few seconds", "Leva alguns segundos").into()),
            None => (
                tr!("Not in a call", "Fora de chamada").into(),
                tr!("Join a voice channel to see your ping", "Entre num canal de voz para ver seu ping").into(),
            ),
        };
        let tiles = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(div().flex().gap(px(10.)).child(tile(tr!("Member since", "Membro desde"), joined, ago, t)).child(tile(
                tr!("Role", "Cargo"),
                role_name,
                role_note,
                t,
            )))
            .child(div().flex().gap(px(10.)).child(tile(tr!("Server", "Servidor"), server_name.clone(), base.clone(), t)).child(tile(
                tr!("Connection", "Conexão"),
                if link_up { tr!("Connected", "Conectado") } else { tr!("Reconnecting…", "Reconectando…") },
                if secure {
                    tr!("Encrypted (HTTPS)", "Criptografada (HTTPS)")
                } else {
                    tr!("Not encrypted (HTTP)", "Sem criptografia (HTTP)")
                },
                t,
            )))
            .child(div().flex().gap(px(10.)).child(tile(tr!("Voice", "Voz"), voice_value, voice_note, t)).child(tile(
                tr!("App", "Aplicativo"),
                format!("Harmony {}", env!("CARGO_PKG_VERSION")),
                tr!("The version on this computer", "A versão neste computador"),
                t,
            )))
            .when(crate::core::api::sends_in_the_clear(&base), |d| d.child(crate::ui::connect::clear_text_warning(t)));

        // This computer's devices, each a way into its section.
        let default = || tr!("Windows default", "Padrão do Windows").to_string();
        let mic = self.inputs.iter().find(|d| d.id == p.voice_input_id).map(|d| d.name.clone()).unwrap_or_else(default);
        let out = self.outputs.iter().find(|d| d.id == p.voice_output_id).map(|d| d.name.clone()).unwrap_or_else(default);
        let cam = self
            .cameras
            .iter()
            .find(|d| d.id == p.voice_camera_id)
            .or(self.cameras.first())
            .map(|d| d.name.clone())
            .unwrap_or_else(|| tr!("No camera found", "Nenhuma câmera encontrada").to_string());
        let devices = card(t)
            .child(device_row("dev-mic", "mic", tr!("Microphone", "Microfone"), mic, Section::Voice, false, t, cx))
            .child(device_row("dev-out", "headphones", tr!("Speakers", "Alto-falantes"), out, Section::Voice, true, t, cx))
            .child(device_row("dev-cam", "camera", tr!("Camera", "Câmera"), cam, Section::Camera, true, t, cx));

        // What the role allows, in plain words; what it doesn't, dimmed, with who can.
        let level = match me.role {
            Role::Owner => 2,
            Role::Admin => 1,
            Role::Member => 0,
        };
        let rights: [(u8, &str); 7] = [
            (0, tr!("Chat, react, pin messages and send files", "Conversar, reagir, fixar mensagens e enviar arquivos")),
            (0, tr!("Talk, turn on your camera and share your screen", "Falar, ligar a câmera e compartilhar a tela")),
            (0, tr!("Add custom emoji and play soundboard clips", "Adicionar emojis e tocar sons no painel de sons")),
            (
                1,
                tr!(
                    "Create, rename and arrange channels, and manage the soundboard",
                    "Criar, renomear e organizar canais, e cuidar do painel de sons"
                ),
            ),
            (
                1,
                tr!(
                    "Mute, move or disconnect people in voice, and delete anyone's messages",
                    "Silenciar, mover ou desconectar pessoas na voz, e apagar mensagens de qualquer um"
                ),
            ),
            (1, tr!("Make members admins", "Tornar membros admins")),
            (
                2,
                tr!(
                    "Rename the server, set its password, change any role and remove accounts",
                    "Renomear o servidor, definir a senha dele, mudar qualquer cargo e remover contas"
                ),
            ),
        ];
        let mut can = card(t).py(px(6.));
        for (need, what) in rights {
            let ok = level >= need;
            can = can.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(7.))
                    .child(icon(if ok { "check" } else { "lock" }, 15., if ok { t.success } else { t.text3 }))
                    .child(div().flex_1().min_w(px(0.)).child(body(what, if ok { t.text } else { t.text3 })))
                    .when(!ok, |d| {
                        d.child(match need {
                            1 => chip(tr!("Admin", "Admin"), t.text3),
                            _ => chip(tr!("Owner", "Dono"), t.text3),
                        })
                    }),
            );
        }

        // The account itself.
        let note = self.profile.password_note.clone();
        let busy = self.profile.password_busy;
        let password = card(t)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(12.))
                    .child(div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(2.)).child(body(tr!("Password", "Senha"), t.text)).child(
                        caption(
                            tr!(
                                "Changing it signs you out on your other computers.",
                                "Trocar a senha desconecta você dos outros computadores."
                            ),
                            t.text2,
                        ),
                    ))
                    .child(
                        button(
                            "password-toggle",
                            if self.profile.password_open { tr!("Cancel", "Cancelar") } else { tr!("Change password", "Trocar senha") },
                            if self.profile.password_open { Kind::Subtle } else { Kind::Standard },
                            t,
                        )
                        .on_click(cx.listener(|s, _, window, cx| s.toggle_password(window, cx))),
                    ),
            )
            .when(self.profile.password_open, |d| {
                let labels = [
                    tr!("Current password", "Senha atual"),
                    tr!("New password", "Nova senha"),
                    tr!("New password again", "Nova senha de novo"),
                ];
                let mut form = div().flex().flex_col().gap(px(12.)).px(px(14.)).pt(px(12.)).pb(px(14.)).border_t_1().border_color(t.stroke);
                for (l, f) in labels.into_iter().zip(self.profile.password.iter()) {
                    form = form.child(div().flex().flex_col().gap(px(6.)).child(label(l, t)).child(f.clone()));
                }
                d.child(
                    form.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                button(
                                    "password-save",
                                    if busy {
                                        tr!("Saving…", "Salvando…")
                                    } else {
                                        tr!("Save the new password", "Salvar a nova senha")
                                    },
                                    Kind::Primary,
                                    t,
                                )
                                .when(busy, |b| b.opacity(0.6))
                                .on_click(cx.listener(|s, _, _, cx| s.save_password(cx))),
                            )
                            .when_some(note.clone().filter(|n| !n.0), |d, (_, e)| d.child(caption(e, t.critical))),
                    ),
                )
            })
            .when_some(note.filter(|n| n.0 && !self.profile.password_open), |d, (_, text)| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(14.))
                        .py(px(10.))
                        .border_t_1()
                        .border_color(t.stroke)
                        .bg(t.tint(t.success))
                        .child(icon("check", 15., t.success))
                        .child(caption(text, t.text)),
                )
            });
        let sign_out = card(t).child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(12.))
                .child(
                    div().flex().flex_col().flex_1().min_w(px(0.)).gap(px(2.)).child(body(tr!("Sign out", "Sair da conta"), t.text)).child(
                        caption(
                            trf!(
                                "Leaves {} on this computer. You can sign back in with your password.",
                                "Desconecta você de {} neste computador. Dá para entrar de novo com a sua senha.",
                                server_name
                            ),
                            t.text2,
                        ),
                    ),
                )
                .child(
                    icon_label_button("sign-out", "log-out", tr!("Sign out", "Sair da conta"), Kind::Danger, t)
                        .on_click(cx.listener(|s, _, window, cx| s.sign_out(window, cx))),
                ),
        );

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(header)
            .child(tiles)
            .child(group(tr!("This computer", "Este computador"), devices, t))
            .child(group(tr!("What you can do here", "O que você pode fazer aqui"), can, t))
            .child(group(tr!("Account", "Conta"), div().flex().flex_col().gap(px(10.)).child(password).child(sign_out), t))
            .into_any_element()
    }

    fn pick_avatar(&mut self, cx: &mut Context<Self>) {
        if let Some(server) = self.server.upgrade() {
            server.update(cx, |s, cx| s.pick_avatar(cx));
        }
    }

    fn remove_avatar(&mut self, cx: &mut Context<Self>) {
        if let Some(server) = self.server.upgrade() {
            server.update(cx, |s, cx| s.remove_avatar(cx));
        }
    }

    fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(server) = self.server.upgrade() {
            server.update(cx, |s, cx| s.sign_out(window, cx));
        }
    }
}

fn card(t: &Theme) -> gpui::Div {
    div().flex().flex_col().rounded(px(radius::CARD)).bg(t.layer).border_1().border_color(t.stroke).overflow_hidden()
}

fn group(name: &'static str, content: impl IntoElement, t: &Theme) -> gpui::Div {
    div().flex().flex_col().gap(px(8.)).child(label(name, t)).child(content)
}

fn tile(name: &'static str, value: impl Into<SharedString>, note: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    card(t)
        .flex_1()
        .min_w(px(0.))
        .gap(px(4.))
        .px(px(14.))
        .py(px(12.))
        .child(label(name, t))
        .child(
            div()
                .truncate()
                .text_size(px(15.))
                .line_height(px(20.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text)
                .child(value.into()),
        )
        .child(div().truncate().text_size(px(12.)).line_height(px(16.)).text_color(t.text2).child(note.into()))
}

#[allow(clippy::too_many_arguments)]
fn device_row(
    id: &'static str,
    glyph: &'static str,
    name: &'static str,
    device: String,
    section: Section,
    rule: bool,
    t: &Theme,
    cx: &mut Context<Settings>,
) -> impl IntoElement {
    let hover = t.layer_hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .py(px(10.))
        .cursor_pointer()
        .when(rule, |d| d.border_t_1().border_color(t.stroke))
        .hover(move |s| s.bg(hover))
        .child(
            div()
                .flex_none()
                .size(px(30.))
                .rounded(px(radius::INNER))
                .bg(t.well)
                .flex()
                .items_center()
                .justify_center()
                .child(icon(glyph, 16., t.text2)),
        )
        .child(div().flex().flex_col().flex_1().min_w(px(0.)).child(label(name, t)).child(div().truncate().child(body(device, t.text))))
        .child(icon("chevron-right", 14., t.text3))
        .on_click(cx.listener(move |s, _, _, cx| s.enter(section, cx)))
}

/// Ping in a word, on the same scale as the call panel's bars.
fn quality(ms: u32) -> &'static str {
    match ms {
        0..=60 => tr!("excellent", "excelente"),
        61..=150 => tr!("good", "boa"),
        _ => tr!("slow", "lenta"),
    }
}

/// The day the account was made, and how long ago in words.
fn joined(created_ms: i64) -> (String, String) {
    use chrono::{Local, TimeZone};
    let Some(when) = Local.timestamp_millis_opt(created_ms).single() else {
        return ("–".into(), String::new());
    };
    let date = crate::i18n::date(&when);
    let days = (Local::now() - when).num_days().max(0);
    let ago = match days {
        0 => tr!("Joined today", "Entrou hoje").to_string(),
        1 => tr!("Joined yesterday", "Entrou ontem").to_string(),
        2..=44 => trf!("{} days ago", "Há {} dias", days),
        45..=364 => trf!("About {} months ago", "Há uns {} meses", (days + 15) / 30),
        _ if days < 730 => tr!("Over a year ago", "Há mais de um ano").to_string(),
        _ => trf!("{} years ago", "Há {} anos", days / 365),
    };
    (date, ago)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_gives_the_day_and_how_long_ago() {
        let ms = chrono::Local::now().timestamp_millis() - 3 * 86_400_000;
        let (date, ago) = joined(ms);
        assert!(
            date.contains(&chrono::Datelike::year(&chrono::Local::now()).to_string())
                || date.contains(&(chrono::Datelike::year(&chrono::Local::now()) - 1).to_string())
        );
        assert!(ago.contains('3'));
        assert_eq!(joined(i64::MAX).0, "–");
    }
}
