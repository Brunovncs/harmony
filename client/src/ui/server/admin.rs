//! Server settings, for admins and the owner: the name, picture and door password, members and
//! their roles, channels and groups, custom emoji, the soundboard. Laid out like the app's settings.
//!
//! The server decides who may do what (`server/src/index.js`); this only mirrors it. What only
//! the owner may do is still shown to admins, locked with the reason, so they know it exists and
//! whom to ask.

use super::{account, emoji_picker, stage};
use crate::core::api::ApiError;
use crate::core::types::*;
use crate::core::{self};
use crate::prefs::{prefs, set_prefs};
use crate::session::{Picture, Session};
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{Theme, current, px, radius, text};
use crate::ui::overlay::{self, Ask, Dismiss, Field, dialog_card, toast};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ObjectFit, ParentElement, PathPromptOptions, Render, RenderImage, SharedString, StatefulInteractiveElement, Styled, StyledImage,
    Subscription, Window, div, img,
};
use std::rc::Rc;
use std::sync::Arc;

/// The server's cap on custom emoji (`MAX_EMOJIS` in `server/src/chat.js`).
const MAX_EMOJIS: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Overview,
    Members,
    Channels,
    Emoji,
    Soundboard,
}

pub struct ServerAdmin {
    focus: FocusHandle,
    session: Entity<Session>,
    section: Section,
    name: Entity<TextField>,
    door: Entity<TextField>,
    search: Entity<TextField>,
    /// The member whose actions are unfolded.
    picked: Option<UserId>,
    /// What the name field was last filled with, so a rename made elsewhere replaces it unless
    /// someone is typing a new one.
    shown_name: String,
    /// The upload quota and its use, once the server has said.
    storage: Option<Storage>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<Dismiss> for ServerAdmin {}

impl Focusable for ServerAdmin {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What stands before the server's name in the top bar: its picture, or the Harmony mark.
pub fn mark(picture: Option<Arc<RenderImage>>, t: &Theme) -> AnyElement {
    match picture {
        Some(image) => img(image).flex_none().size(px(24.)).rounded(px(24. * 0.24)).into_any_element(),
        None => crate::ui::brand_mark(24., t).into_any_element(),
    }
}

/// The server's name in the top bar; for admins and the owner, with the gear that opens this.
pub fn entry(name: String, role: Role, t: &Theme, cx: &mut Context<super::ServerView>) -> AnyElement {
    if !role.is_admin() {
        return title(name, t.text).into_any_element();
    }
    let hover = t.layer_hover;
    div()
        .id("server-admin")
        .flex()
        .items_center()
        .gap(px(7.))
        .h(px(32.))
        .px(px(8.))
        .ml(px(-6.))
        .rounded(px(radius::INNER + 1.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(title(name, t.text))
        .child(icon("settings", 15., t.text2))
        .tooltip(tip(tr!("Server settings", "Configurações do servidor"), t))
        .on_click(cx.listener(|this, _, window, cx| open(this.session.clone(), window, cx)))
        .into_any_element()
}

pub fn open(session: Entity<Session>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ServerAdmin::new(session, cx));
    overlay::open_dialog(view, window, cx);
}

impl ServerAdmin {
    fn new(session: Entity<Session>, cx: &mut Context<Self>) -> ServerAdmin {
        let shown_name = session.read(cx).server_name.clone();
        let name = cx.new(|cx| {
            let mut f = TextField::new(cx, false, 32);
            f.set_text(&shown_name, cx);
            f
        });
        let door = cx.new(|cx| {
            let mut f = TextField::new(cx, false, 128).placeholder(tr!("New password", "Nova senha"));
            f.set_masked(true);
            f
        });
        let search = cx.new(|cx| TextField::new(cx, false, 40).placeholder(tr!("Find someone", "Encontrar alguém")));
        let subs = vec![
            cx.observe(&session, |this, _, cx| this.on_session(cx)),
            cx.subscribe(&search, |_, _, _: &TextFieldEvent, cx| cx.notify()),
            cx.subscribe(&name, |this, _, ev: &TextFieldEvent, cx| {
                if let TextFieldEvent::Submit = ev {
                    this.save_name(cx);
                }
            }),
        ];
        // Whether the door is locked is not in the session until asked for, or until it changes.
        session.update(cx, |s, cx| {
            s.call(
                cx,
                |api| Box::pin(async move { api.server().await }),
                |s, info, cx| {
                    s.set_server_info(info, cx);
                    cx.notify();
                },
            )
        });
        let admin = ServerAdmin {
            focus: cx.focus_handle(),
            session,
            section: Section::Overview,
            name,
            door,
            search,
            picked: None,
            shown_name,
            storage: None,
            _subs: subs,
        };
        admin.refresh_storage(cx);
        admin
    }

    fn refresh_storage(&self, cx: &mut Context<Self>) {
        let api = self.session.read(cx).api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.storage().await }).await;
            let _ = this.update(cx, |this, cx| {
                this.storage = got.ok();
                cx.notify();
            });
        })
        .detach();
    }

    fn on_session(&mut self, cx: &mut Context<Self>) {
        let (role, name) = {
            let s = self.session.read(cx);
            (s.me.role, s.server_name.clone())
        };
        // Demoted while it was open: nothing here is theirs to change any more.
        if !role.is_admin() {
            cx.emit(Dismiss);
            return;
        }
        if name != self.shown_name {
            if self.name.read(cx).text() == self.shown_name {
                self.name.update(cx, |f, cx| f.set_text(&name, cx));
            }
            self.shown_name = name;
        }
        cx.notify();
    }

    fn save_name(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).me.role != Role::Owner {
            return;
        }
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            toast(tr!("Give the server a name.", "Dê um nome ao servidor."), cx);
            return;
        }
        self.session.update(cx, |s, cx| {
            s.call(
                cx,
                move |api| Box::pin(async move { api.update_server(&name, None).await }),
                |s, info, cx| {
                    s.set_server_info(info, cx);
                    toast(tr!("Server renamed.", "Servidor renomeado."), cx);
                    cx.notify();
                },
            )
        });
    }

    /// Sets the door password, or takes it off with an empty one, after asking.
    fn change_door(&mut self, password: String, window: &mut Window, cx: &mut Context<Self>) {
        let (heading, text, ok) = if password.is_empty() {
            (
                tr!("Remove the server password?", "Remover a senha do servidor?"),
                tr!(
                    "Anyone who has the address can reach the sign-in screen. Accounts keep their own passwords.",
                    "Qualquer pessoa com o endereço chega à tela de entrada. As contas continuam com as próprias senhas."
                ),
                tr!("Remove", "Remover"),
            )
        } else {
            (
                tr!("Change the server password?", "Trocar a senha do servidor?"),
                tr!(
                    "Everyone else needs the new password to connect, starting now. Tell them before you change it.",
                    "A partir de agora, todo mundo vai precisar da nova senha para conectar. Avise antes de trocar."
                ),
                tr!("Change", "Trocar"),
            )
        };
        let (session, door) = (self.session.clone(), self.door.clone());
        Ask::confirm_action(heading, text, ok, window, cx, move |_, cx| {
            let (password, door) = (password.clone(), door.clone());
            session.update(cx, |s, cx| {
                let name = s.server_name.clone();
                let sent = password.clone();
                s.call(
                    cx,
                    move |api| Box::pin(async move { api.update_server(&name, Some(&sent)).await }),
                    move |s, info, cx| {
                        // This window keeps working: it now knocks with the new key.
                        s.api.set_server(&s.api.base(), &password);
                        set_prefs(cx, |p| p.password = password);
                        door.update(cx, |f, cx| f.clear(cx));
                        toast(
                            if info.password_required {
                                tr!("Server password changed.", "Senha do servidor trocada.")
                            } else {
                                tr!("Server password removed.", "Senha do servidor removida.")
                            },
                            cx,
                        );
                        s.set_server_info(info, cx);
                        cx.notify();
                    },
                )
            });
        });
    }

    /// Picks a picture, crops it to a square as avatars are, and makes it the server's.
    fn pick_icon(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("Use this picture", "Usar esta imagem").into()),
        });
        let (api, cache) = {
            let s = self.session.read(cx);
            (s.api.clone(), s.cache.clone())
        };
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let got = core::run(async move {
                let bytes = account::avatar_jpeg(&path).map_err(|message| ApiError {
                    status: 0,
                    code: ErrorCode::Unknown,
                    message,
                    body: Default::default(),
                })?;
                // Already here, so the top bar and the rail need not download it.
                cache.insert(&bytes, "image/jpeg");
                let up = api.upload(bytes, "image/jpeg").await?;
                api.set_server_icon(Some(&up.hash)).await
            })
            .await;
            let _ = this.update(cx, |this, cx| this.icon_changed(got, tr!("Server picture changed.", "Foto do servidor trocada."), cx));
        })
        .detach();
    }

    fn remove_icon(&mut self, cx: &mut Context<Self>) {
        let api = self.session.read(cx).api.clone();
        cx.spawn(async move |this, cx| {
            let got = core::run(async move { api.set_server_icon(None).await }).await;
            let _ = this.update(cx, |this, cx| this.icon_changed(got, tr!("Server picture removed.", "Foto do servidor removida."), cx));
        })
        .detach();
    }

    /// The server took the new picture or refused it; either way the storage tile is asked again.
    fn icon_changed(&mut self, got: Result<ServerInfo, ApiError>, done: &'static str, cx: &mut Context<Self>) {
        match got {
            Ok(info) => {
                self.session.update(cx, |s, cx| {
                    s.set_server_info(info, cx);
                    cx.notify();
                });
                toast(done, cx);
            }
            Err(e) => toast(e.message, cx),
        }
        self.refresh_storage(cx);
    }

    // Sections.

    fn overview(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let picture = self.session.update(cx, |s, cx| s.server_picture(cx));
        let s = self.session.read(cx);
        let has_icon = s.server_icon.is_some();
        let owner = s.me.role == Role::Owner;
        let info = s.server_info.clone();
        let current_name = s.server_name.clone();
        let in_voice: usize = s.rosters.values().map(Vec::len).sum();
        let text_channels = s.channels.iter().filter(|c| c.kind == ChannelKind::Text).count();
        let voice_channels = s.channels.len() - text_channels;
        let stats = [
            (tr!("Members", "Membros"), s.users.len(), trf!("{} online", "{} online", s.online.len())),
            (tr!("In voice", "Na voz"), in_voice, tr!("right now", "agora").to_string()),
            (
                tr!("Channels", "Canais"),
                s.channels.len(),
                trf!("{} text · {} voice", "{} de texto · {} de voz", text_channels, voice_channels),
            ),
            (tr!("Groups", "Grupos"), s.groups.len(), tr!("of channels", "de canais").to_string()),
            (tr!("Emoji", "Emojis"), s.emojis.len(), trf!("of {}", "de {}", MAX_EMOJIS)),
            (tr!("Sounds", "Sons"), s.clips.len(), tr!("on the soundboard", "no painel de sons").to_string()),
        ];

        let mut name_block = div().flex().flex_col().gap(px(8.)).child(label(tr!("Server name", "Nome do servidor"), t));
        name_block = if owner {
            name_block
                .child(div().flex().items_center().gap(px(8.)).child(div().flex_1().min_w(px(0.)).child(self.name.clone())).child(
                    button("save-name", tr!("Save", "Salvar"), Kind::Primary, t).on_click(cx.listener(|this, _, _, cx| this.save_name(cx))),
                ))
                .child(caption(
                    tr!(
                        "Everyone sees it at the top of the window. Up to 32 characters.",
                        "Todos veem no topo da janela. Até 32 caracteres."
                    ),
                    t.text3,
                ))
        } else {
            name_block
                .child(readonly(current_name, t))
                .child(lock_note(tr!("Only the owner can rename the server.", "Só o dono pode renomear o servidor."), t))
        };

        let preview = match picture {
            Some(image) => img(image).flex_none().size(px(56.)).rounded(px(radius::CARD)).into_any_element(),
            None => div()
                .flex_none()
                .size(px(56.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::CARD))
                .bg(t.accent_soft)
                .border_1()
                .border_color(t.stroke)
                .child(icon("image", 22., t.accent))
                .into_any_element(),
        };
        let pick_text = if has_icon { tr!("Change picture", "Trocar foto") } else { tr!("Choose a picture", "Escolher uma foto") };
        let owner_only = tr!("Only the owner can change the server picture.", "Só o dono pode trocar a foto do servidor.");
        let controls = if owner {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    icon_label_button("server-icon-pick", "image", pick_text, Kind::Standard, t)
                        .on_click(cx.listener(|this, _, _, cx| this.pick_icon(cx))),
                )
                .when(has_icon, |d| {
                    d.child(
                        button("server-icon-remove", tr!("Remove", "Remover"), Kind::Subtle, t)
                            .on_click(cx.listener(|this, _, _, cx| this.remove_icon(cx))),
                    )
                })
        } else {
            div().flex().child(locked_button("server-icon-pick", pick_text.into(), t).tooltip(tip(owner_only, t)))
        };
        let icon_block = div().flex().flex_col().gap(px(8.)).child(label(tr!("Server picture", "Foto do servidor"), t)).child(
            div().flex().items_center().gap(px(14.)).child(preview).child(
                div().flex().flex_col().gap(px(6.)).flex_1().min_w(px(0.)).child(controls).child(if owner {
                    caption(
                        tr!(
                            "Shown in the bar on the left and at the top of the window. Cropped to a square.",
                            "Aparece na barra à esquerda e no topo da janela. Recortada em quadrado."
                        ),
                        t.text3,
                    )
                } else {
                    lock_note(owner_only, t)
                }),
            ),
        );

        let required = info.as_ref().map(|i| i.password_required);
        let status = match required {
            Some(true) => chip(tr!("On", "Ativada"), t.success, t.tint(t.success)),
            Some(false) => chip(tr!("Off", "Desativada"), t.text2, t.layer_hover),
            None => chip("…", t.text3, t.layer_hover),
        };
        let mut door_card = card(t)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(icon("lock", 16., t.text2))
                    .child(div().flex_1().child(body(tr!("Server password", "Senha do servidor"), t.text)))
                    .child(status),
            )
            .child(caption(
                tr!(
                    "Asked once, before anyone can sign in. Accounts still have their own passwords.",
                    "Pedida uma vez, antes de qualquer pessoa entrar. As contas continuam com as próprias senhas."
                ),
                t.text2,
            ));
        door_card = if owner {
            door_card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w(px(0.)).child(self.door.clone()))
                    .child(
                        button(
                            "set-door",
                            if required == Some(true) { tr!("Change", "Trocar") } else { tr!("Set", "Definir") },
                            Kind::Standard,
                            t,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            let p = this.door.read(cx).text();
                            if p.is_empty() {
                                toast(tr!("Type the new password first.", "Digite a nova senha primeiro."), cx);
                            } else {
                                this.change_door(p, window, cx);
                            }
                        })),
                    )
                    .when(required == Some(true), |d| {
                        d.child(
                            button("remove-door", tr!("Remove", "Remover"), Kind::Subtle, t)
                                .text_color(t.critical)
                                .on_click(cx.listener(|this, _, window, cx| this.change_door(String::new(), window, cx))),
                        )
                    }),
            )
        } else {
            door_card
                .child(lock_note(tr!("Only the owner can change the server password.", "Só o dono pode mudar a senha do servidor."), t))
        };
        if info.as_ref().is_some_and(|i| i.restart_required) {
            door_card = door_card.child(caption(
                tr!(
                    "Restart the server so streams outside voice channels follow the new setting too.",
                    "Reinicie o servidor para as transmissões fora dos canais de voz seguirem a nova configuração também."
                ),
                t.caution,
            ));
        }

        let mut tiles = div().flex().flex_wrap().gap(px(8.));
        for (name, n, sub) in stats {
            tiles = tiles.child(
                div()
                    .flex_1()
                    .min_w(px(160.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(12.))
                    .rounded(px(radius::CARD))
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .child(label(name, t))
                    .child(div().text_size(px(22.)).line_height(px(28.)).font_weight(gpui::FontWeight::SEMIBOLD).child(n.to_string()))
                    .child(caption(sub, t.text3)),
            );
        }
        if let Some(st) = self.storage {
            let used = if st.quota_bytes == 0 { 0. } else { (st.used_bytes as f32 / st.quota_bytes as f32).clamp(0., 1.) };
            let fill = if used >= 0.9 {
                t.critical
            } else if used >= 0.75 {
                t.caution
            } else {
                t.accent
            };
            tiles = tiles.child(
                div()
                    .flex_1()
                    .min_w(px(160.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(12.))
                    .rounded(px(radius::CARD))
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .child(label(tr!("Storage", "Armazenamento"), t))
                    .child(
                        div()
                            .text_size(px(22.))
                            .line_height(px(28.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(byte_size(st.used_bytes)),
                    )
                    .child(
                        div()
                            .h(px(4.))
                            .my(px(2.))
                            .rounded(px(2.))
                            .bg(t.well)
                            .child(div().h_full().rounded(px(2.)).bg(fill).w(gpui::relative(used))),
                    )
                    .child(caption(
                        trf!("of {} for files and pictures", "de {} para arquivos e imagens", byte_size(st.quota_bytes)),
                        t.text3,
                    )),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(name_block)
            .child(icon_block)
            .child(door_card)
            .child(div().flex().flex_col().gap(px(8.)).child(label(tr!("At a glance", "Resumo"), t)).child(tiles))
            .into_any_element()
    }

    fn members(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let q = self.search.read(cx).text().trim().to_lowercase();
        let s = self.session.read(cx);
        let me = s.me.clone();
        let rank = |r: Role| match r {
            Role::Owner => 0,
            Role::Admin => 1,
            Role::Member => 2,
        };
        let mut users: Vec<User> =
            s.users.values().filter(|u| q.is_empty() || u.name().to_lowercase().contains(&q) || u.nickname.contains(&q)).cloned().collect();
        users.sort_by_key(|u| (rank(u.role), !s.online.contains(&u.id), u.name().to_lowercase()));
        let total = s.users.len();

        let mut list = div().flex().flex_col().gap(px(2.));
        let mut last_role = None;
        for u in &users {
            if last_role != Some(u.role) {
                last_role = Some(u.role);
                let count = users.iter().filter(|x| x.role == u.role).count();
                let heading = match u.role {
                    Role::Owner => tr!("Owner", "Dono"),
                    Role::Admin => tr!("Admins", "Admins"),
                    Role::Member => tr!("Members", "Membros"),
                };
                list = list.child(div().px(px(4.)).pt(px(12.)).pb(px(4.)).child(label(format!("{heading} · {count}"), t)));
            }
            list = list.child(self.member_row(u, &me, t, cx));
        }
        if users.is_empty() {
            list = list.child(div().py(px(16.)).child(caption(tr!("Nobody matches that.", "Ninguém encontrado."), t.text3)));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(div().flex_1().min_w(px(0.)).child(self.search.clone()))
                    .child(mono(trf!("{} people", "{} pessoas", total), t.text3)),
            )
            .child(list)
            .child(
                card(t)
                    .mt(px(8.))
                    .child(label(tr!("Who can do what", "Quem pode o quê"), t))
                    .child(caption(
                        tr!(
                            "Admins manage channels, emoji and sounds, delete any message, mute or move people in voice, and make members admins.",
                            "Admins cuidam de canais, emojis e sons, apagam qualquer mensagem, silenciam ou movem pessoas na voz e tornam membros admins."
                        ),
                        t.text2,
                    ))
                    .child(caption(
                        tr!(
                            "Only the owner renames the server, changes its password, removes admins, hands ownership over and deletes accounts.",
                            "Só o dono renomeia o servidor, muda a senha dele, remove admins, transfere a propriedade e apaga contas."
                        ),
                        t.text2,
                    ))
                    .child(caption(
                        tr!(
                            "There is no ban. Deleting an account is the strongest step, and they can register again unless the server has a password.",
                            "Não existe banimento. Apagar a conta é o passo mais forte, e a pessoa pode se cadastrar de novo se o servidor não tiver senha."
                        ),
                        t.text3,
                    )),
            )
            .into_any_element()
    }

    fn member_row(&mut self, u: &User, me: &User, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let img = self.session.update(cx, |s, cx| s.avatar(u.id, cx));
        let s = self.session.read(cx);
        let online = s.online.contains(&u.id);
        let voice =
            s.rosters.iter().find(|(_, r)| r.iter().any(|m| m.user_id == u.id)).and_then(|(id, _)| s.channel(*id)).map(|c| c.name.clone());
        let open = self.picked == Some(u.id);
        let id = u.id;
        let hover = t.layer_hover;
        let (presence, presence_color) = match (&voice, online) {
            (Some(name), _) => (trf!("In {}", "Em {}", name), t.accent),
            (None, true) => (tr!("Online", "Online").to_string(), t.success),
            (None, false) => (tr!("Offline", "Offline").to_string(), t.text3),
        };
        let row = div()
            .id(("admin-member", u.id as u64))
            .flex()
            .items_center()
            .gap(px(12.))
            .h(px(52.))
            .px(px(10.))
            .rounded(px(radius::CONTROL))
            .cursor_pointer()
            .when(open, |d| d.bg(t.layer_hover))
            .when(!open, |d| d.hover(move |s| s.bg(hover)))
            .child(div().relative().child(avatar(u.name(), img, 34., None)).when(online, |d| {
                d.child(
                    div()
                        .absolute()
                        .right(px(-2.))
                        .bottom(px(-2.))
                        .size(px(12.))
                        .rounded_full()
                        .border_2()
                        .border_color(t.pane)
                        .bg(if voice.is_some() { t.accent } else { t.success }),
                )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().truncate().text_size(px(14.)).font_weight(gpui::FontWeight::MEDIUM).child(u.name().to_string()))
                            .when(u.id == me.id, |d| d.child(mono(tr!("(you)", "(você)"), t.text3))),
                    )
                    .child(mono(format!("@{}", u.nickname), t.text3)),
            )
            .child(role_chip(u.role, t))
            .child(div().w(px(110.)).flex().justify_end().child(caption(presence, presence_color).truncate()))
            .child(icon(if open { "chevron-down" } else { "chevron-right" }, 14., t.text3))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.picked = if this.picked == Some(id) { None } else { Some(id) };
                cx.notify();
            }));
        let mut block = div().flex().flex_col().child(row);
        if open {
            block = block.child(self.member_actions(u, me, voice, t, cx));
        }
        block.into_any_element()
    }

    fn member_actions(&mut self, u: &User, me: &User, voice: Option<String>, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let panel = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .ml(px(56.))
            .mr(px(4.))
            .mt(px(2.))
            .mb(px(8.))
            .px(px(12.))
            .py(px(6.))
            .rounded(px(radius::CARD))
            .bg(t.layer)
            .border_1()
            .border_color(t.stroke);
        if u.id == me.id {
            let note = if me.role == Role::Owner {
                tr!(
                    "You are the owner. To step down, hand ownership to someone else from their row.",
                    "Você é o dono. Para deixar o cargo, transfira a propriedade para outra pessoa na linha dela."
                )
            } else {
                tr!("You are an admin. Only the owner can change your role.", "Você é admin. Só o dono pode mudar o seu cargo.")
            };
            return panel.child(div().py(px(6.)).child(caption(note, t.text2))).into_any_element();
        }
        if u.role == Role::Owner && me.role != Role::Owner {
            return panel
                .child(
                    div().py(px(6.)).child(lock_note(
                        tr!("Nobody but an owner can change the owner's role.", "Só um dono pode mudar o cargo do dono."),
                        t,
                    )),
                )
                .into_any_element();
        }

        let owner = me.role == Role::Owner;
        let lock = |reason: &'static str| (!owner).then_some(reason);
        let name = u.name().to_string();
        let mut acts: Vec<Act> = Vec::new();
        match u.role {
            Role::Member => acts.push(Act {
                id: "act-role",
                glyph: "shield",
                text: tr!("Make admin", "Tornar admin").into(),
                hint: tr!(
                    "Admins manage channels, emoji and sounds, and can delete any message.",
                    "Admins cuidam de canais, emojis e sons, e podem apagar qualquer mensagem."
                )
                .into(),
                danger: false,
                lock: None,
                run: set_role(&self.session, u.id, Role::Admin),
            }),
            Role::Admin => acts.push(Act {
                id: "act-role",
                glyph: "shield",
                text: tr!("Remove admin", "Remover admin").into(),
                hint: tr!("They go back to being a member.", "A pessoa volta a ser membro.").into(),
                danger: false,
                lock: lock(tr!("Only the owner can remove an admin.", "Só o dono pode remover um admin.")),
                run: set_role(&self.session, u.id, Role::Member),
            }),
            Role::Owner => acts.push(Act {
                id: "act-role",
                glyph: "shield",
                text: tr!("Make admin instead", "Rebaixar para admin").into(),
                hint: tr!("They stop being an owner.", "A pessoa deixa de ser dona.").into(),
                danger: false,
                lock: None,
                run: set_role(&self.session, u.id, Role::Admin),
            }),
        }
        if u.role != Role::Owner {
            acts.push(Act {
                id: "act-transfer",
                glyph: "crown",
                text: tr!("Transfer ownership", "Transferir a propriedade").into(),
                hint: trf!("{} becomes the owner and you become an admin.", "{} passa a ser o dono e você vira admin.", name).into(),
                danger: true,
                lock: lock(tr!("Only the owner can hand the server over.", "Só o dono pode passar o servidor adiante.")),
                run: transfer(&self.session, me.id, u.id, name.clone()),
            });
        }
        if let Some(channel_name) = voice {
            acts.push(Act {
                id: "act-voice",
                glyph: "phone-off",
                text: tr!("Disconnect from voice", "Desconectar da voz").into(),
                hint: trf!("Takes them out of {}. They can join again.", "Tira a pessoa de {}. Ela pode entrar de novo.", channel_name)
                    .into(),
                danger: false,
                lock: None,
                run: disconnect(&self.session, u.id),
            });
        }
        if u.role != Role::Owner {
            acts.push(Act {
                id: "act-delete",
                glyph: "delete",
                text: tr!("Delete account", "Apagar conta").into(),
                hint: tr!(
                    "Removes the account and every message it wrote. There is no undo.",
                    "Remove a conta e todas as mensagens dela. Não dá para desfazer."
                )
                .into(),
                danger: true,
                lock: lock(tr!("Only the owner can delete an account.", "Só o dono pode apagar uma conta.")),
                run: delete_account(&self.session, u.id, u.nickname.clone()),
            });
        }

        let mut panel = panel;
        for (i, a) in acts.into_iter().enumerate() {
            let control = match a.lock {
                Some(reason) => locked_button(a.id, a.text.clone(), t).tooltip(tip(reason, t)).into_any_element(),
                None => {
                    let run = a.run.clone();
                    icon_label_button(a.id, a.glyph, a.text.clone(), if a.danger { Kind::Danger } else { Kind::Standard }, t)
                        .on_click(cx.listener(move |_, _, window, cx| run(window, cx)))
                        .into_any_element()
                }
            };
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .py(px(8.))
                    .when(i > 0, |d| d.border_t_1().border_color(t.stroke))
                    .child(div().flex_1().min_w(px(0.)).child(match a.lock {
                        Some(reason) => lock_note(reason, t),
                        None => caption(a.hint, t.text2),
                    }))
                    .child(control),
            );
        }
        panel.into_any_element()
    }

    fn channels(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.session.read(cx);
        let layout: Vec<(Option<Group>, Vec<Channel>)> =
            s.layout().into_iter().map(|(g, cs)| (g.cloned(), cs.into_iter().cloned().collect())).collect();
        let unlocked = s.unlocked.clone();
        let (group_ids, order) = arrangement(s);
        let session = self.session.clone();

        let mut list = div().flex().flex_col().gap(px(2.));
        for (group, channels) in layout {
            match &group {
                Some(g) => {
                    let gi = group_ids.iter().position(|x| *x == g.id).unwrap_or(0);
                    let (gid, gname) = (g.id, g.name.clone());
                    let (s1, s2, s3, s4) = (session.clone(), session.clone(), session.clone(), session.clone());
                    let n2 = gname.clone();
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .pt(px(14.))
                            .pb(px(4.))
                            .px(px(4.))
                            .child(div().flex_1().min_w(px(0.)).child(label(g.name.clone(), t)))
                            .child(
                                step_button(("group-up", gid as u64), "chevron-up", tr!("Move up", "Subir"), gi > 0, t)
                                    .when(gi > 0, |d| d.on_click(move |_, _, cx| step_group(&s1, gid, false, cx))),
                            )
                            .child(
                                step_button(
                                    ("group-down", gid as u64),
                                    "chevron-down",
                                    tr!("Move down", "Descer"),
                                    gi + 1 < group_ids.len(),
                                    t,
                                )
                                .when(gi + 1 < group_ids.len(), |d| d.on_click(move |_, _, cx| step_group(&s2, gid, true, cx))),
                            )
                            .child(
                                small_icon_button(("group-edit", gid as u64), "edit", tr!("Rename the group", "Renomear o grupo"), t)
                                    .on_click(move |_, window, cx| rename_group(&s3, gid, gname.clone(), window, cx)),
                            )
                            .child(
                                small_icon_button(("group-delete", gid as u64), "delete", tr!("Delete the group", "Apagar o grupo"), t)
                                    .on_click(move |_, window, cx| delete_group(&s4, gid, n2.clone(), window, cx)),
                            ),
                    );
                }
                None if !channels.is_empty() => {
                    list = list.child(div().px(px(4.)).pb(px(4.)).child(label(tr!("Outside any group", "Fora de grupos"), t)));
                }
                None => {}
            }
            for c in channels {
                let i = order.iter().position(|(id, _)| *id == c.id).unwrap_or(0);
                list = list.child(channel_row(&session, &c, unlocked.contains(&c.id), i > 0, i + 1 < order.len(), t));
            }
            if group.is_some() && group_is_empty(&order, group.as_ref().map(|g| g.id)) {
                list = list.child(
                    div().pl(px(22.)).pb(px(4.)).child(caption(tr!("No channels in this group.", "Nenhum canal neste grupo."), t.text3)),
                );
            }
        }
        let (s1, s2) = (session.clone(), session);
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        icon_label_button("new-channel", "plus", tr!("New channel", "Novo canal"), Kind::Standard, t)
                            .on_click(move |_, window, cx| new_channel(&s1, window, cx)),
                    )
                    .child(
                        icon_label_button("new-group", "plus", tr!("New group", "Novo grupo"), Kind::Subtle, t)
                            .on_click(move |_, window, cx| new_group(&s2, window, cx)),
                    )
                    .child(div().flex_1())
                    .child(caption(
                        tr!("You can also drag channels in the sidebar.", "Também dá para arrastar os canais na barra lateral."),
                        t.text3,
                    )),
            )
            .child(list)
            .into_any_element()
    }

    fn emoji(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let emojis = self.session.read(cx).emojis.clone();
        let count = emojis.len();
        let mut grid = div().flex().flex_wrap().gap(px(8.));
        for e in emojis {
            let face = match self.session.update(cx, |s, cx| s.picture(&e.hash, cx)) {
                Picture::Ready(image) => img(image).size(px(32.)).object_fit(ObjectFit::Contain).into_any_element(),
                _ => div().size(px(32.)).rounded(px(6.)).bg(t.control).into_any_element(),
            };
            let (session, id, name) = (self.session.clone(), e.id, e.name.clone());
            grid = grid.child(
                div()
                    .w(px(176.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .p(px(8.))
                    .rounded(px(radius::CONTROL))
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .child(face)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().truncate().child(mono(format!(":{}:", e.name), t.text)))
                            .child(
                                div()
                                    .truncate()
                                    .child(caption(e.uploader.clone().map(|u| trf!("by {}", "por {}", u)).unwrap_or_default(), t.text3)),
                            ),
                    )
                    .child(small_icon_button(("emoji-delete", id as u64), "delete", tr!("Remove", "Remover"), t).on_click(
                        move |_, window, cx| {
                            let session = session.clone();
                            Ask::confirm_action(
                                trf!("Remove :{}:?", "Remover :{}:?", name),
                                tr!(
                                    "Nobody can use it any more. This can't be undone.",
                                    "Ninguém mais poderá usá-lo. Não dá para desfazer."
                                ),
                                tr!("Remove", "Remover"),
                                window,
                                cx,
                                move |_, cx| {
                                    session.update(cx, |s, cx| {
                                        s.call(cx, move |api| Box::pin(async move { api.delete_emoji(id).await }), |_, _, _| {})
                                    })
                                },
                            );
                        },
                    )),
            );
        }
        if count == 0 {
            grid = grid.child(caption(tr!("No custom emoji yet.", "Nenhum emoji personalizado ainda."), t.text3));
        }
        let session = self.session.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        icon_label_button("add-emoji", "plus", tr!("Add emoji", "Adicionar emoji"), Kind::Standard, t)
                            .on_click(move |_, window, cx| emoji_picker::add_emoji(session.clone(), window, cx)),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(caption(
                        tr!(
                            "Any member can add one; admins can remove any. Pictures are fitted to 128 px.",
                            "Qualquer membro pode adicionar; admins podem remover qualquer um. As imagens ficam com até 128 px."
                        ),
                        t.text3,
                    )))
                    .child(mono(format!("{count} / {MAX_EMOJIS}"), t.text2)),
            )
            .child(grid)
            .into_any_element()
    }

    fn soundboard(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let clips = self.session.read(cx).clips.clone();
        let order: Vec<i64> = clips.iter().map(|c| c.id).collect();
        let gain = prefs(cx).soundpad_volume as f32 / 100.;
        let mut list = div().flex().flex_col().gap(px(6.));
        for (i, c) in clips.iter().enumerate() {
            let s = self.session.clone();
            let (s1, s2, s3, s4, s5) = (s.clone(), s.clone(), s.clone(), s.clone(), s);
            let (hash, clip, clip2) = (c.hash.clone(), c.clone(), c.clone());
            let (earlier, later) = (moved(&order, i, -1), moved(&order, i, 1));
            let id = c.id as u64;
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(48.))
                    .px(px(10.))
                    .rounded(px(radius::CONTROL))
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .child(
                        div().w(px(28.)).flex().justify_center().text_size(px(20.)).child(c.emoji.clone().unwrap_or_else(|| "🔊".into())),
                    )
                    .child(
                        div().flex().flex_col().flex_1().min_w(px(0.)).child(div().truncate().child(body(c.name.clone(), t.text))).child(
                            caption(c.uploader.clone().map(|u| trf!("added by {}", "adicionado por {}", u)).unwrap_or_default(), t.text3),
                        ),
                    )
                    .child(
                        small_icon_button(("clip-play", id), "play", tr!("Listen (only you hear it)", "Ouvir (só você escuta)"), t)
                            .on_click(move |_, _, cx| stage::play_clip(&s1, hash.clone(), gain, cx)),
                    )
                    .child(
                        step_button(("clip-up", id), "chevron-up", tr!("Move earlier", "Mover para antes"), i > 0, t)
                            .when(i > 0, |d| d.on_click(move |_, _, cx| reorder_clips(&s2, earlier.clone(), cx))),
                    )
                    .child(
                        step_button(("clip-down", id), "chevron-down", tr!("Move later", "Mover para depois"), i + 1 < order.len(), t)
                            .when(i + 1 < order.len(), |d| d.on_click(move |_, _, cx| reorder_clips(&s3, later.clone(), cx))),
                    )
                    .child(
                        small_icon_button(("clip-edit", id), "edit", tr!("Rename", "Renomear"), t)
                            .on_click(move |_, window, cx| rename_clip(&s4, &clip, window, cx)),
                    )
                    .child(
                        small_icon_button(("clip-delete", id), "delete", tr!("Delete", "Apagar"), t)
                            .on_click(move |_, window, cx| delete_clip(&s5, &clip2, window, cx)),
                    ),
            );
        }
        if clips.is_empty() {
            list = list.child(caption(tr!("No sounds yet.", "Nenhum som ainda."), t.text3));
        }
        let session = self.session.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        icon_label_button("add-sound", "plus", tr!("Add a sound", "Adicionar som"), Kind::Standard, t)
                            .on_click(move |_, window, cx| stage::add_clip(session.clone(), window, cx)),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(caption(
                        tr!(
                            "Audio up to 2 MB. Everyone downloads every sound, so keep them short.",
                            "Áudio de até 2 MB. Todos baixam todos os sons, então prefira sons curtos."
                        ),
                        t.text3,
                    ))),
            )
            .child(list)
            .into_any_element()
    }
}

impl Render for ServerAdmin {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let (server_name, role) = {
            let s = self.session.read(cx);
            (s.server_name.clone(), s.me.role)
        };
        let nav =
            |id: &'static str, glyph: &'static str, text: &'static str, section: Section, this: &ServerAdmin, cx: &mut Context<Self>| {
                let on = this.section == section;
                let hover = t.layer_hover;
                div()
                    .id(id)
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(38.))
                    .px(px(12.))
                    .rounded(px(radius::CONTROL))
                    .cursor_pointer()
                    .when(on, |d| {
                        d.bg(t.layer_hover)
                            .child(div().absolute().left(px(0.)).top(px(11.)).w(px(3.)).h(px(16.)).rounded(px(2.)).bg(t.accent))
                    })
                    .when(!on, |d| d.hover(move |s| s.bg(hover)))
                    .child(icon(glyph, 16., if on { t.text } else { t.text2 }))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(if on { gpui::FontWeight::SEMIBOLD } else { gpui::FontWeight::NORMAL })
                            .text_color(if on { t.text } else { t.text2 })
                            .child(text),
                    )
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.section = section;
                        cx.notify();
                    }))
            };
        let heading = match self.section {
            Section::Overview => tr!("Overview", "Visão geral"),
            Section::Members => tr!("Members and roles", "Membros e cargos"),
            Section::Channels => tr!("Channels", "Canais"),
            Section::Emoji => tr!("Custom emoji", "Emojis personalizados"),
            Section::Soundboard => tr!("Soundboard", "Painel de sons"),
        };
        let body_el = match self.section {
            Section::Overview => self.overview(&t, cx),
            Section::Members => self.members(&t, cx),
            Section::Channels => self.channels(&t, cx),
            Section::Emoji => self.emoji(&t, cx),
            Section::Soundboard => self.soundboard(&t, cx),
        };
        let max_h = (window.viewport_size().height * 0.86).min(px(640.));
        dialog_card(&t, 820.)
            .track_focus(&self.focus)
            .h(max_h)
            .flex_row()
            .child(
                div()
                    .w(px(210.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(12.))
                    .bg(t.layer)
                    .border_r_1()
                    .border_color(t.stroke)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .px(px(10.))
                            .pt(px(6.))
                            .pb(px(12.))
                            .child(div().truncate().child(title(server_name, t.text)))
                            .child(caption(tr!("Server settings", "Configurações do servidor"), t.text3)),
                    )
                    .child(nav("admin-nav-overview", "server", tr!("Overview", "Visão geral"), Section::Overview, self, cx))
                    .child(nav("admin-nav-members", "users", tr!("Members and roles", "Membros e cargos"), Section::Members, self, cx))
                    .child(nav("admin-nav-channels", "hash", tr!("Channels", "Canais"), Section::Channels, self, cx))
                    .child(nav("admin-nav-emoji", "smile", tr!("Emoji", "Emojis"), Section::Emoji, self, cx))
                    .child(nav("admin-nav-sounds", "music", tr!("Soundboard", "Painel de sons"), Section::Soundboard, self, cx))
                    .child(div().flex_1())
                    .child(div().flex().flex_col().gap(px(6.)).px(px(10.)).pb(px(6.)).child(div().flex().child(role_chip(role, &t))).when(
                        role != Role::Owner,
                        |d| {
                            d.child(caption(
                                tr!(
                                    "Some things only the owner can do. They show a lock.",
                                    "Algumas coisas só o dono pode fazer. Elas aparecem com um cadeado."
                                ),
                                t.text3,
                            ))
                        },
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(24.))
                            .pt(px(20.))
                            .pb(px(6.))
                            .child(title(heading, t.text))
                            .child(
                                icon_button("admin-close", "close", &t)
                                    .tooltip(tip(tr!("Close (Esc)", "Fechar (Esc)"), &t))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                            ),
                    )
                    .child(
                        div()
                            .id("admin-body")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .px(px(24.))
                            .pt(px(10.))
                            .pb(px(24.))
                            .child(body_el),
                    ),
            )
    }
}

// Pieces.

type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// One thing that can be done to a member, or that could be by the owner.
struct Act {
    id: &'static str,
    glyph: &'static str,
    text: SharedString,
    hint: SharedString,
    danger: bool,
    /// Why this viewer may not, when they may not.
    lock: Option<&'static str>,
    run: Action,
}

fn card(t: &Theme) -> gpui::Div {
    div().flex().flex_col().gap(px(10.)).p(px(14.)).rounded(px(radius::CARD)).bg(t.layer).border_1().border_color(t.stroke)
}

fn readonly(value: String, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .h(px(36.))
        .px(px(12.))
        .rounded(px(radius::CONTROL))
        .bg(t.well)
        .border_1()
        .border_color(t.stroke)
        .child(body(value, t.text2))
}

/// A size the way people read one: 820 KB, 12.4 MB, 9.8 GB.
fn byte_size(n: u64) -> String {
    let n = n as f64;
    if n >= (1u64 << 30) as f64 {
        format!("{:.1} GB", n / (1u64 << 30) as f64)
    } else if n >= (1u64 << 20) as f64 {
        format!("{:.1} MB", n / (1u64 << 20) as f64)
    } else {
        format!("{} KB", (n / 1024.).ceil())
    }
}

fn lock_note(reason: &'static str, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(icon("lock", 12., t.text3))
        .child(div().text_size(px(text::CAPTION.0)).line_height(px(text::CAPTION.1)).text_color(t.text3).child(reason))
}

fn role_chip(role: Role, t: &Theme) -> gpui::Div {
    match role {
        Role::Owner => chip(tr!("Owner", "Dono"), t.caution, t.tint(t.caution)),
        Role::Admin => chip(tr!("Admin", "Admin"), t.accent, t.accent_soft),
        Role::Member => chip(tr!("Member", "Membro"), t.text2, t.layer_hover),
    }
}

/// A button for something only the owner may do: there, but greyed out with a lock.
fn locked_button(id: &'static str, text: SharedString, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .h(px(36.))
        .pl(px(12.))
        .pr(px(14.))
        .rounded(px(radius::CONTROL))
        .bg(t.control)
        .border_1()
        .border_color(t.stroke_strong)
        .opacity(0.55)
        .cursor(CursorStyle::OperationNotAllowed)
        .text_size(px(text::BODY.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(t.text2)
        .child(icon("lock", 15., t.text2))
        .child(text)
}

fn small_icon_button(id: impl Into<gpui::ElementId>, glyph: &'static str, hint: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
    icon_button(id, glyph, t).tooltip(tip(hint, t))
}

/// An up or down arrow; dimmed, and inert, at the end it cannot go past.
fn step_button(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    hint: &'static str,
    enabled: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    if enabled {
        return small_icon_button(id, glyph, hint, t);
    }
    div().id(id.into()).flex().flex_none().items_center().justify_center().size(px(32.)).opacity(0.3).child(icon(glyph, 17., t.text2))
}

fn channel_row(session: &Entity<Session>, c: &Channel, unlocked: bool, can_up: bool, can_down: bool, t: &Theme) -> AnyElement {
    let voice = c.kind == ChannelKind::Voice;
    let (id, name, kind) = (c.id, c.name.clone(), c.kind);
    let (s1, s2, s3, s4) = (session.clone(), session.clone(), session.clone(), session.clone());
    let n2 = name.clone();
    let hover = t.layer_hover;
    div()
        .id(("admin-channel", id as u64))
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(38.))
        .pl(px(20.))
        .pr(px(4.))
        .rounded(px(radius::INNER + 1.))
        .hover(move |s| s.bg(hover))
        .child(icon(if voice { "volume" } else { "hash" }, 15., t.text3))
        .child(div().flex_1().min_w(px(0.)).truncate().child(body(c.name.clone(), t.text)))
        .when(c.locked, |d| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(icon("lock", 12., if unlocked { t.success } else { t.text3 }))
                    .child(caption(tr!("password", "senha"), t.text3)),
            )
        })
        .child(mono(if voice { tr!("voice", "voz") } else { tr!("text", "texto") }, t.text3))
        .child(
            step_button(("channel-up", id as u64), "chevron-up", tr!("Move up", "Subir"), can_up, t)
                .when(can_up, |d| d.on_click(move |_, _, cx| step_channel(&s1, id, false, cx))),
        )
        .child(
            step_button(("channel-down", id as u64), "chevron-down", tr!("Move down", "Descer"), can_down, t)
                .when(can_down, |d| d.on_click(move |_, _, cx| step_channel(&s2, id, true, cx))),
        )
        .child(
            small_icon_button(("channel-edit", id as u64), "edit", tr!("Rename or set a password", "Renomear ou definir senha"), t)
                .on_click(move |_, window, cx| edit_channel(&s3, id, name.clone(), kind, window, cx)),
        )
        .child(
            small_icon_button(("channel-delete", id as u64), "delete", tr!("Delete the channel", "Apagar o canal"), t)
                .on_click(move |_, window, cx| delete_channel(&s4, id, n2.clone(), window, cx)),
        )
        .into_any_element()
}

// What the buttons do. All of it goes through `Api`, and a refusal comes back as a toast.

fn call<T: Send + 'static>(
    session: &Entity<Session>,
    cx: &mut App,
    f: impl FnOnce(crate::core::api::Api) -> futures::future::BoxFuture<'static, crate::core::api::Result<T>> + Send + 'static,
) {
    session.update(cx, |s, cx| s.call(cx, f, |_, _, _| {}));
}

fn set_role(session: &Entity<Session>, id: UserId, role: Role) -> Action {
    let session = session.clone();
    Rc::new(move |_, cx| {
        session.update(cx, |s, cx| {
            s.call(
                cx,
                move |api| Box::pin(async move { api.set_role(id, role).await }),
                |s, user, cx| {
                    s.users.insert(user.id, user);
                    cx.notify();
                },
            )
        })
    })
}

fn transfer(session: &Entity<Session>, me: UserId, to: UserId, name: String) -> Action {
    let session = session.clone();
    Rc::new(move |window, cx| {
        let (session, done_name) = (session.clone(), name.clone());
        Ask::confirm_action(
            trf!("Make {} the owner?", "Tornar {} dono do servidor?", name),
            trf!(
                "You become an admin, and only {} can give ownership back.",
                "Você vira admin, e só {} pode devolver a propriedade.",
                name
            ),
            tr!("Transfer ownership", "Transferir a propriedade"),
            window,
            cx,
            move |_, cx| {
                let done_name = done_name.clone();
                session.update(cx, |s, cx| {
                    s.call(
                        cx,
                        // In this order: the server refuses to leave itself without an owner.
                        move |api| {
                            Box::pin(async move {
                                api.set_role(to, Role::Owner).await?;
                                api.set_role(me, Role::Admin).await
                            })
                        },
                        move |_, _, cx| toast(trf!("{} is now the owner.", "{} agora é o dono.", done_name), cx),
                    )
                });
            },
        );
    })
}

fn disconnect(session: &Entity<Session>, user: UserId) -> Action {
    let session = session.clone();
    Rc::new(move |_, cx| super::request(&session, "admin:move", serde_json::json!({ "userId": user, "toChannelId": null }), cx))
}

fn delete_account(session: &Entity<Session>, id: UserId, nick: String) -> Action {
    let session = session.clone();
    Rc::new(move |window, cx| {
        let (session, nick) = (session.clone(), nick.clone());
        Ask::open(
            trf!("Delete {}'s account?", "Apagar a conta de {}?", nick),
            Some(trf!(
                "Their messages go too, and there is no undo. Type {} to confirm.",
                "As mensagens vão junto, e não dá para desfazer. Digite {} para confirmar.",
                nick
            )),
            tr!("Delete account", "Apagar conta"),
            true,
            vec![Field::Text {
                label: tr!("Username", "Nome de usuário"),
                value: String::new(),
                placeholder: "",
                secret: false,
                multiline: false,
                max: 32,
            }],
            window,
            cx,
            move |v, _, cx| {
                if v[0].trim().to_lowercase() != nick {
                    return Some(trf!("Type {} to confirm.", "Digite {} para confirmar.", nick));
                }
                call(&session, cx, move |api| Box::pin(async move { api.delete_account(id).await }));
                None
            },
        );
    })
}

/// Every group in order, and every channel in drawing order with its group, as `arrange` wants.
fn arrangement(s: &Session) -> (Vec<i64>, Vec<(ChannelId, Option<i64>)>) {
    let mut groups: Vec<&Group> = s.groups.iter().collect();
    groups.sort_by_key(|g| g.position);
    let order = s.layout().into_iter().flat_map(|(g, cs)| cs.into_iter().map(move |c| (c.id, g.map(|g| g.id)))).collect();
    (groups.into_iter().map(|g| g.id).collect(), order)
}

fn group_is_empty(order: &[(ChannelId, Option<i64>)], group: Option<i64>) -> bool {
    !order.iter().any(|(_, g)| *g == group)
}

/// One step up or down the list. At the edge of a group it crosses into the next one rather
/// than swapping with a channel there, so a channel can be walked into and out of groups.
fn step_channel(session: &Entity<Session>, id: ChannelId, down: bool, cx: &mut App) {
    let (groups, mut order) = arrangement(session.read(cx));
    let Some(i) = order.iter().position(|(c, _)| *c == id) else { return };
    let Some(j) = (if down { i.checked_add(1) } else { i.checked_sub(1) }).filter(|j| *j < order.len()) else { return };
    if order[j].1 == order[i].1 {
        order.swap(i, j);
    } else {
        order[i].1 = order[j].1;
    }
    call(session, cx, move |api| Box::pin(async move { api.arrange(&groups, &order).await }));
}

fn step_group(session: &Entity<Session>, id: i64, down: bool, cx: &mut App) {
    let (mut groups, order) = arrangement(session.read(cx));
    let Some(i) = groups.iter().position(|g| *g == id) else { return };
    let Some(j) = (if down { i.checked_add(1) } else { i.checked_sub(1) }).filter(|j| *j < groups.len()) else { return };
    groups.swap(i, j);
    call(session, cx, move |api| Box::pin(async move { api.arrange(&groups, &order).await }));
}

fn new_channel(session: &Entity<Session>, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::open(
        tr!("New channel", "Novo canal"),
        None,
        tr!("Create", "Criar"),
        false,
        vec![
            Field::Text {
                label: tr!("Name", "Nome"),
                value: String::new(),
                placeholder: "geral",
                secret: false,
                multiline: false,
                max: 32,
            },
            Field::Choice {
                label: tr!("Kind", "Tipo"),
                options: vec![("text".into(), tr!("Text", "Texto").into()), ("voice".into(), tr!("Voice", "Voz").into())],
                picked: 0,
            },
            Field::Text {
                label: tr!("Password (optional)", "Senha (opcional)"),
                value: String::new(),
                placeholder: tr!("Leave empty for an open channel", "Deixe vazio para um canal aberto"),
                secret: true,
                multiline: false,
                max: 64,
            },
        ],
        window,
        cx,
        move |v, _, cx| {
            let name = v[0].trim().to_string();
            if name.is_empty() {
                return Some(tr!("Give it a name.", "Dê um nome a ele.").into());
            }
            let kind = if v[1] == "voice" { ChannelKind::Voice } else { ChannelKind::Text };
            let password = (!v[2].is_empty()).then(|| v[2].clone());
            call(&session, cx, move |api| Box::pin(async move { api.create_channel(kind, &name, password.as_deref()).await }));
            None
        },
    );
}

fn new_group(session: &Entity<Session>, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::open(
        tr!("New group", "Novo grupo"),
        Some(tr!("A folder for channels in the sidebar.", "Uma pasta para canais na barra lateral.").into()),
        tr!("Create", "Criar"),
        false,
        vec![Field::Text { label: tr!("Name", "Nome"), value: String::new(), placeholder: "", secret: false, multiline: false, max: 32 }],
        window,
        cx,
        move |v, _, cx| {
            let name = v[0].trim().to_string();
            call(&session, cx, move |api| Box::pin(async move { api.create_group(&name).await }));
            None
        },
    );
}

fn edit_channel(session: &Entity<Session>, id: ChannelId, name: String, kind: ChannelKind, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::open(
        tr!("Edit channel", "Editar canal"),
        Some(if kind == ChannelKind::Voice { tr!("Voice channel", "Canal de voz") } else { tr!("Text channel", "Canal de texto") }.into()),
        tr!("Save", "Salvar"),
        false,
        vec![
            Field::Text { label: tr!("Name", "Nome"), value: name, placeholder: "", secret: false, multiline: false, max: 32 },
            Field::Text {
                label: tr!("New password", "Nova senha"),
                value: String::new(),
                placeholder: tr!("Empty keeps it as it is", "Vazio mantém como está"),
                secret: true,
                multiline: false,
                max: 64,
            },
            Field::Choice {
                label: tr!("Password", "Senha"),
                options: vec![
                    ("keep".into(), tr!("Keep or set", "Manter ou definir").into()),
                    ("remove".into(), tr!("Remove it", "Remover").into()),
                ],
                picked: 0,
            },
        ],
        window,
        cx,
        move |v, _, cx| {
            let name = v[0].trim().to_string();
            if name.is_empty() {
                return Some(tr!("Give it a name.", "Dê um nome a ele.").into());
            }
            let password = if v[2] == "remove" { Some(String::new()) } else { (!v[1].is_empty()).then(|| v[1].clone()) };
            call(&session, cx, move |api| Box::pin(async move { api.update_channel(id, &name, password.as_deref()).await }));
            None
        },
    );
}

fn delete_channel(session: &Entity<Session>, id: ChannelId, name: String, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::confirm_action(
        trf!("Delete #{}?", "Apagar #{}?", name),
        tr!("Its messages go with it. This can't be undone.", "As mensagens vão junto. Não dá para desfazer."),
        tr!("Delete", "Apagar"),
        window,
        cx,
        move |_, cx| call(&session, cx, move |api| Box::pin(async move { api.delete_channel(id).await })),
    );
}

fn rename_group(session: &Entity<Session>, id: i64, name: String, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::open(
        tr!("Rename group", "Renomear grupo"),
        None,
        tr!("Save", "Salvar"),
        false,
        vec![Field::Text { label: tr!("Name", "Nome"), value: name, placeholder: "", secret: false, multiline: false, max: 32 }],
        window,
        cx,
        move |v, _, cx| {
            let name = v[0].trim().to_string();
            call(&session, cx, move |api| Box::pin(async move { api.rename_group(id, &name).await }));
            None
        },
    );
}

fn delete_group(session: &Entity<Session>, id: i64, name: String, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::confirm_action(
        trf!("Delete the group {}?", "Apagar o grupo {}?", name),
        tr!("Its channels stay, outside any group.", "Os canais dele continuam, fora de qualquer grupo."),
        tr!("Delete", "Apagar"),
        window,
        cx,
        move |_, cx| call(&session, cx, move |api| Box::pin(async move { api.delete_group(id).await })),
    );
}

/// The clip order with the one at `i` moved by `delta`, clamped to the ends.
fn moved(order: &[i64], i: usize, delta: isize) -> Vec<i64> {
    let mut o = order.to_vec();
    let to = (i as isize + delta).clamp(0, o.len() as isize - 1) as usize;
    let v = o.remove(i);
    o.insert(to, v);
    o
}

fn reorder_clips(session: &Entity<Session>, order: Vec<i64>, cx: &mut App) {
    call(session, cx, move |api| Box::pin(async move { api.reorder_clips(&order).await }));
}

fn rename_clip(session: &Entity<Session>, clip: &Clip, window: &mut Window, cx: &mut App) {
    let (session, id) = (session.clone(), clip.id);
    Ask::open(
        tr!("Rename the sound", "Renomear o som"),
        None,
        tr!("Save", "Salvar"),
        false,
        vec![
            Field::Text { label: tr!("Name", "Nome"), value: clip.name.clone(), placeholder: "", secret: false, multiline: false, max: 32 },
            Field::Optional { label: tr!("Emoji", "Emoji"), value: clip.emoji.clone().unwrap_or_default(), placeholder: "🔊", max: 4 },
        ],
        window,
        cx,
        move |v, _, cx| {
            let (name, emoji) = (v[0].trim().to_string(), v[1].trim().to_string());
            call(&session, cx, move |api| {
                Box::pin(async move { api.rename_clip(id, &name, (!emoji.is_empty()).then_some(emoji.as_str())).await })
            });
            None
        },
    );
}

fn delete_clip(session: &Entity<Session>, clip: &Clip, window: &mut Window, cx: &mut App) {
    let (session, id) = (session.clone(), clip.id);
    Ask::confirm_action(
        trf!("Delete the sound {}?", "Apagar o som {}?", clip.name),
        tr!("It leaves everyone's soundboard. This can't be undone.", "Ele sai do painel de todos. Não dá para desfazer."),
        tr!("Delete", "Apagar"),
        window,
        cx,
        move |_, cx| call(&session, cx, move |api| Box::pin(async move { api.delete_clip(id).await })),
    );
}

#[cfg(test)]
mod tests {
    use super::moved;

    #[test]
    fn clips_move_one_step_and_stop_at_the_ends() {
        assert_eq!(moved(&[1, 2, 3], 1, -1), vec![2, 1, 3]);
        assert_eq!(moved(&[1, 2, 3], 1, 1), vec![1, 3, 2]);
        assert_eq!(moved(&[1, 2, 3], 0, -1), vec![1, 2, 3]);
        assert_eq!(moved(&[1, 2, 3], 2, 1), vec![1, 2, 3]);
    }
}
