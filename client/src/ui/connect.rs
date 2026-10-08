//! The first screen: which server, and who you are on it. Signs in or creates the account, then
//! hands a started session to the root view. Once the rail has servers, a smaller dialog adds
//! more of them, and more accounts.

use crate::core::api::{Api, ApiError, sends_in_the_clear};
use crate::core::cache::Cache;
use crate::core::settings::SavedServer;
use crate::core::types::{AuthReply, Health, IceServer, User};
use crate::core::{self};
use crate::prefs::{prefs, set_prefs};
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{Theme, current, px, radius};
use crate::ui::overlay::{Dismiss, dialog_card};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement, ParentElement,
    Render, RenderImage, StatefulInteractiveElement, Styled, Task, Window, div, linear_color_stop, linear_gradient,
};
use std::sync::Arc;
use std::time::Duration;

pub struct Connected {
    pub api: Api,
    pub cache: Cache,
    pub me: User,
    pub server_name: String,
    pub server_icon: Option<String>,
    pub ice_servers: Vec<IceServer>,
}

pub enum ConnectEvent {
    Connected(Box<Connected>),
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    SignIn,
    Register,
}

pub struct ConnectView {
    focus: FocusHandle,
    server: Entity<TextField>,
    door: Entity<TextField>,
    nickname: Entity<TextField>,
    password: Entity<TextField>,
    confirm: Entity<TextField>,
    owner_key: Entity<TextField>,
    remember: bool,
    mode: Mode,
    busy: bool,
    error: Option<String>,
    /// What the last probe of the server said, for the hints under the form.
    health: Option<Health>,
    /// A saved sign-in for this nickname, so the password can stay empty.
    saved_token: bool,
    /// Asks the server about itself while the address and its password are typed.
    probe: Task<()>,
}

impl EventEmitter<ConnectEvent> for ConnectView {}

impl Focusable for ConnectView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ConnectView {
    pub fn new(error: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> ConnectView {
        let p = prefs(cx).clone();
        let field = |placeholder: &str, value: &str, cx: &mut Context<Self>| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            let f = cx.new(|cx| {
                let mut f = TextField::new(cx, false, 200).placeholder(placeholder);
                f.set_text(&value, cx);
                f
            });
            cx.subscribe(&f, |this: &mut ConnectView, f, ev: &TextFieldEvent, cx| match ev {
                TextFieldEvent::Submit => this.connect(cx),
                TextFieldEvent::Changed => {
                    this.error = None;
                    if f == this.server || f == this.door {
                        this.probe(cx);
                    }
                    cx.notify();
                }
            })
            .detach();
            f
        };
        let server = field("pi.local:8080", &p.server_url, cx);
        let door = field(tr!("Only if the server has one", "Só se o servidor tiver uma"), &p.password, cx);
        let nickname = field(tr!("Your name on the server", "Seu nome no servidor"), &p.username, cx);
        let password = field(tr!("At least 6 characters", "Pelo menos 6 caracteres"), "", cx);
        let confirm = field(tr!("The same again", "A mesma de novo"), "", cx);
        for f in [&door, &password, &confirm] {
            f.update(cx, |f, _| f.set_masked(true));
        }
        let owner_key = field(tr!("From the server's install log", "Do log de instalação do servidor"), "", cx);
        let saved_token = !p.session_token.is_empty() && !p.username.is_empty();
        let mut this = ConnectView {
            focus: cx.focus_handle(),
            server,
            door,
            nickname,
            password,
            confirm,
            owner_key,
            remember: p.remember_account,
            mode: Mode::SignIn,
            busy: false,
            error,
            health: None,
            saved_token,
            probe: Task::ready(()),
        };
        let first = if p.server_url.is_empty() {
            this.server.clone()
        } else if p.username.is_empty() {
            this.nickname.clone()
        } else {
            this.password.clone()
        };
        first.focus_handle(cx).focus(window, cx);
        this.probe(cx);
        // Remembered on this computer: go straight in, as a chat app does.
        if this.error.is_none() && saved_token && !p.server_url.is_empty() {
            cx.defer_in(window, |this: &mut ConnectView, _, cx| this.connect(cx));
        }
        this
    }

    /// Whether the server has accounts and an owner decides which fields the form shows, so it is
    /// asked as soon as the address (and its password) are in, not only after a failed attempt.
    fn probe(&mut self, cx: &mut Context<Self>) {
        let server = self.text(&self.server, cx);
        let door = self.door.read(cx).text();
        if server.is_empty() {
            self.health = None;
            self.probe = Task::ready(());
            return;
        }
        self.probe = probe(server, door, cx, |this: &mut ConnectView, health, cx| {
            if no_accounts(&health) && this.mode == Mode::SignIn {
                this.mode = Mode::Register;
            }
            this.health = health;
            cx.notify();
        });
    }

    fn text(&self, f: &Entity<TextField>, cx: &Context<Self>) -> String {
        f.read(cx).text().trim().to_string()
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let server = self.text(&self.server, cx);
        let door = self.door.read(cx).text();
        let nickname = self.text(&self.nickname, cx).to_lowercase().split_whitespace().collect::<String>();
        let password = self.password.read(cx).text();
        let confirm = self.confirm.read(cx).text();
        let owner_key = self.text(&self.owner_key, cx);
        if server.is_empty() {
            return self.fail(tr!("Type the server's address first.", "Digite o endereço do servidor primeiro."), cx);
        }
        if nickname.is_empty() {
            return self.fail(tr!("Type the name you use on this server.", "Digite o nome que você usa neste servidor."), cx);
        }
        if self.mode == Mode::Register && password != confirm {
            return self.fail(tr!("The two passwords are not the same.", "As duas senhas não são iguais."), cx);
        }
        let saved = prefs(cx).clone();
        // A sign-in this computer kept for that server and name lets the password stay empty.
        let token = (password.is_empty() && self.mode == Mode::SignIn)
            .then(|| saved.saved_server_as(&server, &nickname).map(|s| s.session_token.clone()))
            .flatten()
            .filter(|t| !t.is_empty());
        if password.is_empty() && token.is_none() {
            return self.fail(tr!("Type your password.", "Digite sua senha."), cx);
        }
        self.busy = true;
        self.error = None;
        cx.notify();

        let mode = self.mode;
        let remember = self.remember;
        let api = Api::new();
        api.set_server(&server, &door);
        cx.spawn(async move |this, cx| {
            let result = core::run({
                let api = api.clone();
                let nickname = nickname.clone();
                async move { sign_in(&api, mode, &nickname, &password, &owner_key, token).await }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(done) => {
                        let connected = finish(done, api, &server, &door, &nickname, remember, cx);
                        cx.emit(ConnectEvent::Connected(connected));
                    }
                    Err(Failure { message, health, expired }) => {
                        if let Some(h) = health {
                            note_health(&server, &h, cx);
                            if h.has_accounts == Some(false) && this.mode == Mode::SignIn {
                                this.mode = Mode::Register;
                            }
                            this.health = Some(h);
                        }
                        if expired {
                            this.saved_token = false;
                            set_prefs(cx, |s| s.forget_token(&server, &nickname));
                        }
                        this.error = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fail(&mut self, message: &str, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    fn forget(&mut self, cx: &mut Context<Self>) {
        set_prefs(cx, |s| s.session_token.clear());
        self.saved_token = false;
        cx.notify();
    }
}

/// Asks a server about itself once typing pauses, for which fields a form shows.
fn probe<V: 'static>(
    server: String,
    door: String,
    cx: &mut Context<V>,
    done: impl FnOnce(&mut V, Option<Health>, &mut Context<V>) + 'static,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(Duration::from_millis(400)).await;
        let api = Api::new();
        api.set_server(&server, &door);
        let health = core::run(async move { api.health().await }).await.ok();
        let _ = this.update(cx, |this, cx| done(this, health, cx));
    })
}

/// A server with no accounts at all, where only creating one can work.
fn no_accounts(health: &Option<Health>) -> bool {
    health.as_ref().is_some_and(|h| h.has_accounts == Some(false))
}

/// Keeps the server and the sign-in, passes on what the server wanted said, and hands over what
/// the session starts from.
fn finish(done: Done, api: Api, server: &str, door: &str, nickname: &str, remember: bool, cx: &mut App) -> Box<Connected> {
    set_prefs(cx, |s| {
        s.remember_account = remember;
        s.remember_server(SavedServer {
            url: server.into(),
            password: door.into(),
            username: nickname.into(),
            session_token: if remember { done.token.clone() } else { String::new() },
            name: done.server_name.clone(),
            icon: done.server_icon.clone().unwrap_or_default(),
        });
    });
    for note in &done.notes {
        crate::ui::overlay::toast(*note, cx);
    }
    Box::new(Connected {
        api,
        cache: Cache::shared(prefs(cx).media_cache_mb),
        me: done.user,
        server_name: done.server_name,
        server_icon: done.server_icon,
        ice_servers: done.ice_servers,
    })
}

/// A failed attempt that got past the door still saw the server's name and picture as they are
/// now, for the rail.
fn note_health(server: &str, h: &Health, cx: &mut App) {
    if !h.authenticated {
        return;
    }
    let name = h.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Harmony".into());
    let icon = h.icon_hash.clone().unwrap_or_default();
    if !prefs(cx).knows_server(server, &name, &icon) {
        set_prefs(cx, |p| {
            p.note_server(server, &name, &icon);
        });
    }
}

struct Done {
    user: User,
    token: String,
    server_name: String,
    server_icon: Option<String>,
    ice_servers: Vec<IceServer>,
    notes: Vec<&'static str>,
}

struct Failure {
    message: String,
    health: Option<Health>,
    /// The server refused the kept sign-in.
    expired: bool,
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        Failure { message: e.message, health: None, expired: false }
    }
}

async fn sign_in(api: &Api, mode: Mode, nickname: &str, password: &str, owner_key: &str, token: Option<String>) -> Result<Done, Failure> {
    let health = api.health().await?;
    let fail = |message: String, health: &Health| Failure { message, health: Some(health.clone()), expired: false };
    if health.password_required && !health.authenticated {
        let message = if api.base().is_empty() {
            tr!("Type the server password.", "Digite a senha do servidor.")
        } else {
            tr!("That is not this server's password.", "Essa não é a senha do servidor.")
        };
        return Err(fail(message.into(), &health));
    }
    if health.has_accounts.is_none() {
        return Err(fail(
            tr!(
                "This server is older than accounts. Update it to use this version of Harmony.",
                "Este servidor é de uma versão sem contas. Atualize-o para usar esta versão do Harmony."
            )
            .into(),
            &health,
        ));
    }
    let key = (!owner_key.is_empty()).then_some(owner_key);
    let mut notes = Vec::new();
    if health.mediamtx.as_deref() == Some("down") {
        notes.push(tr!(
            "The server's media relay is not answering, so voice, cameras and screens won't work until it is back.",
            "O servidor de mídia não está respondendo. Voz, câmeras e telas só vão funcionar quando ele voltar."
        ));
    }
    let (user, token) = match token {
        Some(t) => {
            api.set_token(&t);
            match api.me().await {
                Ok(u) if u.nickname == nickname => (u, t),
                // Only a refusal ends the kept sign-in; a network hiccup leaves it for next time.
                Err(e) if e.status != 401 => return Err(fail(e.message, &health)),
                _ => {
                    api.set_token("");
                    let server = health.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| api.base());
                    return Err(Failure {
                        message: trf!(
                            "Your saved sign-in to {} expired. Type your password to sign in again.",
                            "Seu login salvo em {} expirou. Digite sua senha para entrar de novo.",
                            server
                        ),
                        health: Some(health),
                        expired: true,
                    });
                }
            }
        }
        None => {
            let reply: AuthReply = match mode {
                Mode::SignIn => api.login(nickname, password, key).await,
                Mode::Register => api.register(nickname, password, key).await,
            }
            .map_err(|e| fail(e.message, &health))?;
            api.set_token(&reply.token);
            if reply.owner_claimed {
                notes.push(tr!("You are this server's owner now.", "Agora você é o dono deste servidor."));
            } else if reply.owner_key_rejected {
                notes.push(tr!(
                    "That owner key was not accepted; you joined as a member.",
                    "Essa chave de dono não foi aceita; você entrou como membro."
                ));
            }
            (reply.user, reply.token)
        }
    };
    // The session claim is where voice gets its ICE servers; the stream list has them too.
    let ice_servers = match api.session(nickname, false).await {
        Ok(s) if !s.ice_servers.is_empty() => s.ice_servers,
        _ => api.streams().await.map(|(_, ice)| ice).unwrap_or_default(),
    };
    let server_name = health.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Harmony".into());
    Ok(Done { user, token, server_name, server_icon: health.icon_hash, ice_servers, notes })
}

/// Said wherever a password is about to cross the internet over plain HTTP.
pub fn clear_text_warning(t: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(10.))
        .px(px(12.))
        .py(px(10.))
        .rounded(px(radius::CONTROL))
        .bg(t.tint(t.caution))
        .border_1()
        .border_color(t.caution.opacity(0.4))
        .child(icon("warning", 16., t.caution))
        .child(div().flex_1().min_w(px(0.)).child(body(
            tr!(
                "This address is plain HTTP across the internet, so your password travels unencrypted and anyone along the way can read it. Use an https:// address if the server has one.",
                "Este endereço usa HTTP sem criptografia pela internet, então qualquer um no caminho pode ler a sua senha. Use um endereço https:// se o servidor tiver um."
            ),
            t.text,
        )))
}

impl Render for ConnectView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let register = self.mode == Mode::Register;
        let needs_owner = self.health.as_ref().is_some_and(|h| h.needs_owner);
        let door_needed = self.health.as_ref().map(|h| h.password_required);
        let saved = self.saved_token && self.mode == Mode::SignIn;
        let field = |label_text: &'static str, f: &Entity<TextField>, t: &Theme| {
            div().flex().flex_col().gap(px(6.)).child(label(label_text, t)).child(f.clone())
        };
        let form = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(14.))
                            .child(field(tr!("Server address", "Endereço do servidor"), &self.server, &t))
                            .child(field(
                                if door_needed == Some(false) {
                                    tr!("Server password (not needed)", "Senha do servidor (não precisa)")
                                } else {
                                    tr!("Server password", "Senha do servidor")
                                },
                                &self.door,
                                &t,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(14.))
                            .child(field(tr!("Username", "Nome de usuário"), &self.nickname, &t))
                            .child(field(tr!("Password", "Senha"), &self.password, &t))
                            .when(register, |d| d.child(field(tr!("Confirm password", "Confirme a senha"), &self.confirm, &t)))
                            .when(needs_owner, |d| d.child(field(tr!("Owner key", "Chave de dono"), &self.owner_key, &t))),
                    ),
            )
            .when(needs_owner, |d| {
                d.child(caption(
                    tr!(
                        "This server has no owner yet. Whoever installed it pastes the owner key it printed, on sign-up or sign-in; everyone else leaves it empty.",
                        "Este servidor ainda não tem dono. Quem instalou cola a chave de dono que ele gerou, ao criar a conta ou ao entrar; os outros deixam em branco."
                    ),
                    t.text3,
                ))
            })
            .when(sends_in_the_clear(&self.server.read(cx).text()), |d| d.child(clear_text_warning(&t)))
            .child(
                div()
                    .id("remember")
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .child(switch(self.remember, &t))
                    .child(body(tr!("Remember me on this computer", "Lembrar de mim neste computador"), t.text2))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.remember = !this.remember;
                        cx.notify();
                    })),
            )
            .when(saved, |d| {
                d.child(caption(
                    trf!(
                        "Signed in as {}. Leave the password empty to continue.",
                        "Login salvo como {}. Deixe a senha vazia para continuar.",
                        prefs(cx).username
                    ),
                    t.text3,
                ))
            })
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(10.))
                        .px(px(12.))
                        .py(px(10.))
                        .rounded(px(radius::CONTROL))
                        .bg(t.tint(t.critical))
                        .border_1()
                        .border_color(t.critical.opacity(0.4))
                        .child(icon("warning", 16., t.critical))
                        .child(body(e, t.text)),
                )
            })
            .child(
                button(
                    "connect",
                    if self.busy {
                        tr!("Connecting…", "Conectando…")
                    } else if register {
                        tr!("Create account", "Criar conta")
                    } else {
                        tr!("Connect", "Conectar")
                    },
                    Kind::Primary,
                    &t,
                )
                .w_full()
                .h(px(42.))
                .when(self.busy, |b| b.opacity(0.7))
                .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(caption(
                        if register {
                            tr!("Already registered?", "Já tem conta?")
                        } else if saved {
                            tr!("Not you?", "Não é você?")
                        } else {
                            tr!("No account yet?", "Ainda não tem conta?")
                        },
                        t.text3,
                    ))
                    .child(
                        div()
                            .id("mode")
                            .cursor_pointer()
                            .text_color(t.accent)
                            .hover(|s| s.opacity(0.8))
                            .child(caption(
                                if register {
                                    tr!("Sign in", "Entrar")
                                } else if saved {
                                    tr!("Sign out", "Sair da conta")
                                } else {
                                    tr!("Create one", "Criar uma")
                                },
                                t.accent,
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if saved {
                                    this.forget(cx);
                                } else {
                                    this.mode = if register { Mode::SignIn } else { Mode::Register };
                                    this.error = None;
                                    cx.notify();
                                }
                            })),
                    ),
            );

        div()
            .key_context("Connect")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.base)
            .child(
                // A trace of the accent behind the form, as Texel's stage does.
                div().absolute().inset_0().bg(linear_gradient(
                    160.,
                    linear_color_stop(t.accent.opacity(if t.dark { 0.10 } else { 0.07 }), 0.),
                    linear_color_stop(t.base.opacity(0.), 0.6),
                )),
            )
            .children(crate::ui::updates::button(cx).map(|b| div().absolute().top(px(14.)).right(px(14.)).child(b)))
            .child(
                div()
                    .id("connect-card")
                    .relative()
                    .w_full()
                    .max_w(px(720.))
                    .mx(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(24.))
                    .p(px(32.))
                    .rounded(px(radius::PANE + 4.))
                    .bg(t.pane)
                    .border_1()
                    .border_color(t.stroke)
                    .shadow_lg()
                    .child(div().flex().items_center().gap(px(14.)).child(crate::ui::brand_mark(40., &t)).child(
                        div().flex().flex_col().child(display("Harmony", t.text)).child(body(
                            tr!(
                                "Voice, screens and chat for your group, on a server you run.",
                                "Voz, telas e chat para o seu grupo, num servidor que é seu."
                            ),
                            t.text2,
                        )),
                    ))
                    .child(form),
            )
    }
}

/// What the small dialog is there to add.
#[derive(Clone, Copy, PartialEq)]
pub enum Adding {
    /// Another server, for the account in use.
    Server,
    /// Another name you already have on some server.
    Account,
    /// A name you do not have yet, made on a server.
    NewAccount,
}

/// The account in use, which the dialog signs in as unless told otherwise.
pub struct Me {
    pub username: String,
    pub name: String,
    pub picture: Option<Arc<RenderImage>>,
}

/// Adds a server or an account once the rail has some: the address, the server's password only
/// when it has one, and your password, as the account in use unless you change the name.
pub struct AddServer {
    focus: FocusHandle,
    kind: Adding,
    server: Entity<TextField>,
    door: Entity<TextField>,
    nickname: Entity<TextField>,
    password: Entity<TextField>,
    confirm: Entity<TextField>,
    owner_key: Entity<TextField>,
    /// Who it signs in as while the name field is hidden.
    me: Option<Me>,
    mode: Mode,
    busy: bool,
    error: Option<String>,
    health: Option<Health>,
    /// A probe answered for the address as typed, so a silent one reads as "nothing there".
    probed: bool,
    probe: Task<()>,
}

impl EventEmitter<ConnectEvent> for AddServer {}
impl EventEmitter<Dismiss> for AddServer {}

impl Focusable for AddServer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        let first = if self.server.read(cx).text().trim().is_empty() {
            &self.server
        } else if self.me.is_none() && self.nickname.read(cx).text().trim().is_empty() {
            &self.nickname
        } else {
            &self.password
        };
        first.focus_handle(cx)
    }
}

impl AddServer {
    /// `server` fills the address in, for an account on a server you already have.
    pub fn new(kind: Adding, me: Option<Me>, server: &str, window: &mut Window, cx: &mut Context<Self>) -> AddServer {
        let field = |placeholder: &str, value: &str, cx: &mut Context<Self>| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            let f = cx.new(|cx| {
                let mut f = TextField::new(cx, false, 200).placeholder(placeholder);
                f.set_text(&value, cx);
                f
            });
            cx.subscribe_in(&f, window, |this: &mut AddServer, f, ev: &TextFieldEvent, _, cx| match ev {
                TextFieldEvent::Submit => this.submit(cx),
                TextFieldEvent::Changed => {
                    this.error = None;
                    if *f == this.server || *f == this.door {
                        this.probe(cx);
                    }
                    cx.notify();
                }
            })
            .detach();
            f
        };
        let server = field("pi.local:8080", server, cx);
        let door = field(tr!("Only if the server has one", "Só se o servidor tiver uma"), "", cx);
        let nickname = field(tr!("Your name on the server", "Seu nome no servidor"), "", cx);
        let password = field(tr!("At least 6 characters", "Pelo menos 6 caracteres"), "", cx);
        let confirm = field(tr!("The same again", "A mesma de novo"), "", cx);
        let owner_key = field(tr!("From the server's install log", "Do log de instalação do servidor"), "", cx);
        for f in [&door, &password, &confirm] {
            f.update(cx, |f, _| f.set_masked(true));
        }
        let mut this = AddServer {
            focus: cx.focus_handle(),
            kind,
            server,
            door,
            nickname,
            password,
            confirm,
            owner_key,
            me: if kind == Adding::Server { me } else { None },
            mode: Mode::SignIn,
            busy: false,
            error: None,
            health: None,
            probed: false,
            probe: Task::ready(()),
        };
        this.set_mode(if kind == Adding::NewAccount { Mode::Register } else { Mode::SignIn }, cx);
        this.probe(cx);
        this
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        let hint = match mode {
            Mode::SignIn => tr!("Your password on this server", "Sua senha neste servidor"),
            Mode::Register => tr!("At least 6 characters", "Pelo menos 6 caracteres"),
        };
        self.password.update(cx, |f, cx| f.set_placeholder(hint, cx));
        cx.notify();
    }

    fn probe(&mut self, cx: &mut Context<Self>) {
        let server = self.server.read(cx).text().trim().to_string();
        let door = self.door.read(cx).text();
        self.probed = false;
        if server.is_empty() {
            self.health = None;
            self.probe = Task::ready(());
            return;
        }
        self.probe = probe(server, door, cx, |this: &mut AddServer, health, cx| {
            if no_accounts(&health) {
                this.set_mode(Mode::Register, cx);
            }
            this.health = health;
            this.probed = true;
            cx.notify();
        });
    }

    /// Shows the name field, filled with the name in use, to sign in as someone else.
    fn other_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(me) = self.me.take() else { return };
        self.nickname.update(cx, |f, cx| f.set_text(&me.username, cx));
        self.nickname.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let server = self.server.read(cx).text().trim().to_string();
        let door = self.door.read(cx).text();
        let typed = self.nickname.read(cx).text();
        let nickname = self.me.as_ref().map_or(typed.as_str(), |me| &me.username).to_lowercase().split_whitespace().collect::<String>();
        let password = self.password.read(cx).text();
        let confirm = self.confirm.read(cx).text();
        let owner_key = self.owner_key.read(cx).text().trim().to_string();
        let problem = if server.is_empty() {
            Some(tr!("Type the server's address first.", "Digite o endereço do servidor primeiro."))
        } else if nickname.is_empty() {
            Some(tr!("Type the name you use on this server.", "Digite o nome que você usa neste servidor."))
        } else if password.is_empty() {
            Some(tr!("Type your password.", "Digite sua senha."))
        } else if self.mode == Mode::Register && password != confirm {
            Some(tr!("The two passwords are not the same.", "As duas senhas não são iguais."))
        } else {
            None
        };
        if let Some(problem) = problem {
            self.error = Some(problem.into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();

        let mode = self.mode;
        let api = Api::new();
        api.set_server(&server, &door);
        cx.spawn(async move |this, cx| {
            let result = core::run({
                let (api, nickname) = (api.clone(), nickname.clone());
                async move { sign_in(&api, mode, &nickname, &password, &owner_key, None).await }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(done) => {
                        let remember = prefs(cx).remember_account;
                        let connected = finish(done, api, &server, &door, &nickname, remember, cx);
                        cx.emit(ConnectEvent::Connected(connected));
                        cx.emit(Dismiss);
                    }
                    Err(Failure { message, health, .. }) => {
                        if let Some(h) = health {
                            note_health(&server, &h, cx);
                            if h.has_accounts == Some(false) {
                                this.set_mode(Mode::Register, cx);
                            }
                            this.health = Some(h);
                        }
                        this.error = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn header(&self, t: &Theme) -> gpui::Div {
        let (heading, text) = match self.kind {
            Adding::Server => (
                tr!("Add a server", "Adicionar um servidor"),
                tr!(
                    "It goes in the bar on the left, with your other servers.",
                    "Ele entra na barra à esquerda, junto dos seus outros servidores."
                ),
            ),
            Adding::Account => (
                tr!("Add an account", "Adicionar conta"),
                tr!(
                    "Sign in with another name. Each server has its own accounts.",
                    "Entre com outro nome. Cada servidor tem as suas próprias contas."
                ),
            ),
            Adding::NewAccount => (
                tr!("Create an account", "Criar conta"),
                tr!(
                    "A new name on a server. Each server has its own accounts.",
                    "Um nome novo em um servidor. Cada servidor tem as suas próprias contas."
                ),
            ),
        };
        div().flex().flex_col().gap(px(4.)).child(title(heading, t.text)).child(caption(text, t.text2))
    }

    /// What the probe found at the address, under it.
    fn found(&self, t: &Theme) -> Option<gpui::Div> {
        let row = |dot: gpui::Hsla, text: String, color: gpui::Hsla| {
            div().flex().items_center().gap(px(8.)).child(status_dot(dot, false)).child(caption(text, color))
        };
        match &self.health {
            Some(h) => {
                let name = h.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Harmony".into());
                Some(row(t.success, name, t.text2))
            }
            None if self.probed => Some(row(
                t.text3,
                tr!("No Harmony server answered there yet.", "Nenhum servidor Harmony respondeu aí ainda.").into(),
                t.text3,
            )),
            None => None,
        }
    }

    /// Who it signs in as: the account in use, a click away from another name.
    fn signing_as(&self, me: &Me, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let register = self.mode == Mode::Register;
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(avatar(&me.name, me.picture.clone(), 30., None))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(caption(
                        if register { tr!("Create an account as", "Criar conta como") } else { tr!("Sign in as", "Entrar como") },
                        t.text3,
                    ))
                    .child(div().truncate().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(me.username.clone())),
            )
            .child(
                div()
                    .id("add-other-name")
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.8))
                    .child(caption(tr!("Use another name", "Usar outro nome"), t.accent))
                    .on_click(cx.listener(|this, _, window, cx| this.other_name(window, cx))),
            )
    }
}

impl Render for AddServer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let register = self.mode == Mode::Register;
        let needs_owner = self.health.as_ref().is_some_and(|h| h.needs_owner);
        let door_needed = self.health.as_ref().is_some_and(|h| h.password_required);
        let field = |label_text: &'static str, f: &Entity<TextField>, t: &Theme| {
            div().flex().flex_col().gap(px(6.)).child(label(label_text, t)).child(f.clone())
        };
        let who = match &self.me {
            Some(me) => self.signing_as(me, &t, cx),
            None => field(tr!("Username", "Nome de usuário"), &self.nickname, &t),
        };
        let content = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(22.))
            .child(self.header(&t))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(field(tr!("Server address", "Endereço do servidor"), &self.server, &t))
                    .children(self.found(&t)),
            )
            .when(door_needed, |d| d.child(field(tr!("Server password", "Senha do servidor"), &self.door, &t)))
            .child(div().h(px(1.)).bg(t.stroke))
            .child(who)
            .child(field(tr!("Password", "Senha"), &self.password, &t))
            .when(register, |d| d.child(field(tr!("Confirm password", "Confirme a senha"), &self.confirm, &t)))
            .when(needs_owner, |d| {
                d.child(field(tr!("Owner key", "Chave de dono"), &self.owner_key, &t)).child(caption(
                    tr!(
                        "This server has no owner yet. Whoever installed it pastes the owner key it printed; everyone else leaves it empty.",
                        "Este servidor ainda não tem dono. Quem instalou cola a chave de dono que ele gerou; os outros deixam em branco."
                    ),
                    t.text3,
                ))
            })
            .when(no_accounts(&self.health), |d| {
                d.child(caption(
                    tr!("This server has no accounts yet, so yours will be the first.", "Este servidor ainda não tem contas, então a sua vai ser a primeira."),
                    t.text3,
                ))
            })
            .when(sends_in_the_clear(&self.server.read(cx).text()), |d| d.child(clear_text_warning(&t)))
            .when_some(self.error.clone(), |d, e| {
                d.child(div().flex().items_start().gap(px(8.)).child(icon("warning", 15., t.critical)).child(caption(e, t.critical)))
            })
            .when(!no_accounts(&self.health), |d| {
                d.child(
                    div()
                        .id("add-mode")
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.8))
                        .child(caption(
                            if register {
                                tr!("I have an account on this server", "Já tenho conta neste servidor")
                            } else {
                                tr!("Create an account on this server", "Criar uma conta neste servidor")
                            },
                            t.accent,
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.error = None;
                            this.set_mode(if register { Mode::SignIn } else { Mode::Register }, cx);
                        })),
                )
            });
        let go = if self.busy {
            tr!("Connecting…", "Conectando…")
        } else if register {
            tr!("Create account", "Criar conta")
        } else {
            tr!("Sign in", "Entrar")
        };
        dialog_card(&t, 420.).key_context("AddServer").track_focus(&self.focus).child(content).child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .px(px(22.))
                .py(px(14.))
                .bg(t.layer)
                .border_t_1()
                .border_color(t.stroke)
                .child(
                    button("add-cancel", tr!("Cancel", "Cancelar"), Kind::Standard, &t)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                )
                .child(
                    button("add-go", go, Kind::Primary, &t)
                        .when(self.busy, |b| b.opacity(0.7))
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
        )
    }
}
