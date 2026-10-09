//! The dialogs of private conversations, written for someone who has never heard the words
//! "end-to-end": keeping the recovery key, unlocking another computer, comparing the safety
//! number, starting a conversation, and answering a call.

use super::sidebar::{Menu, MenuEntry};
use crate::core::types::*;
use crate::session::Session;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{MONO, Theme, current, px, radius};
use crate::ui::overlay::{self, Ask, Dismiss, dialog_card};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div,
};

/// A dialog's bottom row of buttons.
fn footer(t: &Theme) -> gpui::Div {
    div().flex().items_center().justify_end().gap(px(8.)).px(px(22.)).py(px(14.)).bg(t.layer).border_t_1().border_color(t.stroke)
}

/// The recovery key, big and easy to copy, in its groups.
fn key_block(text: &str, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .justify_center()
        .gap(px(8.))
        .p(px(14.))
        .rounded(px(radius::CARD))
        .bg(t.well)
        .border_1()
        .border_color(t.stroke)
        .children(text.split('-').map(|g| {
            div().font_family(MONO).text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text).child(g.to_string())
        }))
}

fn step(n: &str, text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .gap(px(10.))
        .child(
            div()
                .flex_none()
                .size(px(22.))
                .rounded_full()
                .bg(t.accent_soft)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .font_weight(FontWeight::BOLD)
                .text_color(t.accent)
                .child(n.to_string()),
        )
        .child(div().flex_1().min_w(px(0.)).child(body(text, t.text2)))
}

// The recovery key.

/// Shows the recovery key and asks the person to put it somewhere safe. Nothing else can open
/// their private messages on another computer, so this is the one thing worth a dialog of its own.
pub fn backup(session: &Entity<Session>, window: &mut Window, cx: &mut App) {
    let Some(key) = session.read(cx).vault.recovery() else { return };
    let view = cx.new(|cx| Backup { session: session.clone(), key, kept: false, focus: cx.focus_handle() });
    overlay::open_dialog(view, window, cx);
}

struct Backup {
    session: Entity<Session>,
    key: String,
    kept: bool,
    focus: FocusHandle,
}

impl EventEmitter<Dismiss> for Backup {}

impl Focusable for Backup {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Backup {
    fn save(&mut self, cx: &mut Context<Self>) {
        let dir = dirs::document_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let target = cx.prompt_for_new_path(&dir, Some("harmony-chave-de-recuperacao.txt"));
        let text = format!(
            "{}\r\n\r\n{}\r\n\r\n{}\r\n",
            tr!("Harmony recovery key", "Chave de recuperação do Harmony"),
            self.key,
            tr!(
                "It opens your private messages on another computer. Keep it to yourself.",
                "Ela abre suas mensagens privadas em outro computador. Não mostre a ninguém."
            )
        );
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = target.await else { return };
            let saved = std::fs::write(&path, text);
            let _ = this.update(cx, |this, cx| match saved {
                Ok(()) => {
                    this.kept = true;
                    overlay::toast(tr!("Saved. Keep that file somewhere safe.", "Salvo. Guarde esse arquivo em um lugar seguro."), cx);
                    cx.notify();
                }
                Err(e) => overlay::toast(trf!("Could not save it: {}", "Não foi possível salvar: {}", e), cx),
            });
        })
        .detach();
    }
}

impl Render for Backup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let kept = self.kept;
        dialog_card(&t, 500.)
            .track_focus(&self.focus)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .p(px(22.))
                    .child(div().flex().items_center().gap(px(10.)).child(icon("key", 20., t.accent)).child(title(
                        tr!("Your recovery key", "Sua chave de recuperação"),
                        t.text,
                    )))
                    .child(body(
                        tr!(
                            "Your private messages are locked with a key that only exists on your computers. This code is the spare: with it you can read them on a new computer, or after reinstalling Windows.",
                            "Suas mensagens privadas são trancadas com uma chave que só existe nos seus computadores. Este código é a cópia de segurança: com ele você consegue lê-las em um computador novo ou depois de reinstalar o Windows."
                        ),
                        t.text2,
                    ))
                    .child(key_block(&self.key, &t))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button("backup-copy", tr!("Copy", "Copiar"), Kind::Standard, &t).on_click(cx.listener(|this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(this.key.clone()));
                                overlay::toast(tr!("Copied.", "Copiado."), cx);
                            })))
                            .child(
                                button("backup-save", tr!("Save as a file", "Salvar em um arquivo"), Kind::Standard, &t)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(12.))
                            .rounded(px(radius::CARD))
                            .bg(t.tint(t.caution))
                            .child(step("1", tr!("Keep it somewhere only you can get to: a password manager, or on paper.", "Guarde em um lugar que só você acessa: um gerenciador de senhas, ou no papel."), &t))
                            .child(step("2", tr!("Never send it to anyone. Nobody from the server will ever ask for it.", "Nunca envie para ninguém. Ninguém do servidor vai pedir por ele."), &t))
                            .child(step("3", tr!("Lose it and every computer that has your key, and your old messages can't be opened again.", "Se perder o código e todos os computadores com a sua chave, as mensagens antigas não abrem mais."), &t)),
                    )
                    .child(
                        div()
                            .id("backup-kept")
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .cursor_pointer()
                            .child(checkbox(kept, &t))
                            .child(body(tr!("I put my recovery key somewhere safe", "Guardei minha chave de recuperação em um lugar seguro"), t.text))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.kept = !this.kept;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                footer(&t)
                    .child(button("backup-later", tr!("Later", "Depois"), Kind::Standard, &t).on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))))
                    .child(
                        button("backup-done", tr!("Done", "Pronto"), Kind::Primary, &t)
                            .when(!kept, |d| d.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.kept {
                                    this.session.update(cx, |s, cx| s.mark_backed_up(cx));
                                    cx.emit(Dismiss);
                                }
                            })),
                    ),
            )
    }
}

/// Makes a new recovery key for the key this computer has (the old one stops working), then
/// shows it.
pub fn replace_recovery(session: &Entity<Session>, window: &mut Window, cx: &mut App) {
    let session = session.clone();
    Ask::confirm_action(
        tr!("Make a new recovery key?", "Gerar uma nova chave de recuperação?"),
        tr!(
            "The old one stops working. Your messages stay as they are.",
            "A antiga para de funcionar. Suas mensagens continuam como estão."
        ),
        tr!("Make a new one", "Gerar nova"),
        window,
        cx,
        move |window, cx| {
            let task = session.update(cx, |s, cx| s.new_recovery(cx));
            let (session, window_handle) = (session.clone(), window.window_handle());
            cx.spawn(async move |cx| {
                let got = task.await;
                let _ = window_handle.update(cx, |_, window, cx| match got {
                    Ok(_) => backup(&session, window, cx),
                    Err(e) => overlay::toast(e.message, cx),
                });
            })
            .detach();
        },
    );
}

// Unlocking on another computer.

/// Asks for the recovery key on a computer that does not have this account's key yet.
pub fn unlock(session: &Entity<Session>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| {
        let field = cx.new(|cx| TextField::new(cx, false, 64).placeholder("XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX"));
        let sub = cx.subscribe_in(&field, window, |this: &mut Unlock, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => this.confirm(window, cx),
            TextFieldEvent::Changed => {
                this.error = false;
                cx.notify();
            }
        });
        Unlock { session: session.clone(), field, error: false, _sub: sub }
    });
    overlay::open_dialog(view, window, cx);
}

struct Unlock {
    session: Entity<Session>,
    field: Entity<TextField>,
    error: bool,
    _sub: Subscription,
}

impl EventEmitter<Dismiss> for Unlock {}

impl Focusable for Unlock {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.field.focus_handle(cx)
    }
}

impl Unlock {
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let typed = self.field.read(cx).text();
        if self.session.update(cx, |s, cx| s.unlock(&typed, cx)) {
            overlay::toast(tr!("Unlocked. Your private messages are here now.", "Desbloqueado. Suas mensagens privadas estão aqui agora."), cx);
            cx.emit(Dismiss);
        } else {
            self.error = true;
            cx.notify();
        }
    }

    fn lost(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        cx.emit(Dismiss);
        Ask::confirm_action(
            tr!("Start over without the recovery key?", "Recomeçar sem a chave de recuperação?"),
            tr!(
                "You get a new key and can talk privately again right away. Messages from before stay locked for you (the other person still has them), and the people you talk to will see that your key changed.",
                "Você ganha uma chave nova e pode conversar no privado de novo na hora. As mensagens de antes continuam trancadas para você (a outra pessoa ainda as tem), e quem conversa com você vai ver que sua chave mudou."
            ),
            tr!("Start over", "Recomeçar"),
            window,
            cx,
            move |window, cx| {
                session.update(cx, |s, cx| s.reset_keys(cx));
                let session = session.clone();
                // The new key's recovery key, as soon as it exists.
                let handle = window.window_handle();
                cx.spawn(async move |cx| {
                    for _ in 0..50 {
                        cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                        let ready = cx.update(|cx| session.read(cx).vault.needs_backup());
                        if ready {
                            let _ = handle.update(cx, |_, window, cx| backup(&session, window, cx));
                            return;
                        }
                    }
                })
                .detach();
            },
        );
    }
}

impl Render for Unlock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        dialog_card(&t, 480.)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .p(px(22.))
                    .child(div().flex().items_center().gap(px(10.)).child(icon("lock", 20., t.accent)).child(title(
                        tr!("Unlock your private messages", "Desbloquear suas mensagens privadas"),
                        t.text,
                    )))
                    .child(body(
                        tr!(
                            "This computer doesn't have your key yet. Type the recovery key you kept when you first set up private messages.",
                            "Este computador ainda não tem a sua chave. Digite a chave de recuperação que você guardou quando ativou as mensagens privadas."
                        ),
                        t.text2,
                    ))
                    .child(
                        div()
                            .p(px(10.))
                            .rounded(px(radius::CONTROL))
                            .bg(t.control)
                            .border_1()
                            .border_color(if self.error { t.critical } else { t.stroke })
                            .font_family(MONO)
                            .child(self.field.clone()),
                    )
                    .when(self.error, |d| {
                        d.child(caption(
                            tr!(
                                "That isn't the right key. Check each group of four, or find the file you saved.",
                                "Essa não é a chave certa. Confira cada grupo de quatro, ou procure o arquivo que você salvou."
                            ),
                            t.critical,
                        ))
                    }),
            )
            .child(
                footer(&t)
                    .child(
                        div()
                            .id("unlock-lost")
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(t.text3)
                            .hover(|s| s.underline())
                            .child(tr!("I lost my recovery key", "Perdi minha chave de recuperação"))
                            .on_click(cx.listener(|this, _, window, cx| this.lost(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(button("unlock-cancel", tr!("Cancel", "Cancelar"), Kind::Standard, &t).on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))))
                    .child(
                        button("unlock-ok", tr!("Unlock", "Desbloquear"), Kind::Primary, &t)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

// The safety number.

/// The number both people should see the same, to be sure nobody sits between them.
pub fn safety_number(session: &Entity<Session>, peer: UserId, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| SafetyNumber { session: session.clone(), peer, focus: cx.focus_handle() });
    overlay::open_dialog(view, window, cx);
}

struct SafetyNumber {
    session: Entity<Session>,
    peer: UserId,
    focus: FocusHandle,
}

impl EventEmitter<Dismiss> for SafetyNumber {}

impl Focusable for SafetyNumber {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SafetyNumber {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let s = self.session.read(cx);
        let name = s.display_name(Some(self.peer), None);
        let number = s.vault.safety_number(self.peer);
        let verified = s.vault.verified(self.peer);
        let peer = self.peer;
        dialog_card(&t, 460.)
            .track_focus(&self.focus)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .p(px(22.))
                    .child(div().flex().items_center().gap(px(10.)).child(icon("shield-check", 20., t.success)).child(title(
                        trf!("Safety number with {}", "Código de segurança com {}", name),
                        t.text,
                    )))
                    .child(body(
                        trf!(
                            "Your messages and calls with {} are encrypted on your computers. To be sure they reach {} and nobody else, compare this number with theirs, in a call or in person. If it's the same on both screens, you're safe.",
                            "Suas mensagens e chamadas com {} são criptografadas nos seus computadores. Para ter certeza de que chegam a {} e a mais ninguém, comparem este número, numa chamada ou pessoalmente. Se for igual nas duas telas, está tudo certo.",
                            name,
                            name
                        ),
                        t.text2,
                    ))
                    .child(match &number {
                        Some(n) => div()
                            .grid()
                            .grid_cols(3)
                            .gap(px(10.))
                            .p(px(16.))
                            .rounded(px(radius::CARD))
                            .bg(t.well)
                            .border_1()
                            .border_color(if verified { t.success } else { t.stroke })
                            .children(n.split(' ').map(|g| {
                                div().flex().justify_center().font_family(MONO).text_size(px(18.)).text_color(t.text).child(g.to_string())
                            })),
                        None => div().child(caption(
                            tr!("There's no number yet: one of you hasn't set up private messages.", "Ainda não há número: um de vocês não ativou as mensagens privadas."),
                            t.text3,
                        )),
                    })
                    .when(verified, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(icon("check", 14., t.success))
                                .child(caption(tr!("You marked this as checked.", "Você marcou como conferido."), t.success)),
                        )
                    }),
            )
            .child(
                footer(&t)
                    .child(button("safety-close", tr!("Close", "Fechar"), Kind::Standard, &t).on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))))
                    .when(number.is_some(), |d| {
                        d.child(
                            button(
                                "safety-mark",
                                if verified { tr!("Unmark as checked", "Desmarcar") } else { tr!("They match", "São iguais") },
                                if verified { Kind::Standard } else { Kind::Primary },
                                &t,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.session.update(cx, |s, cx| {
                                    s.vault.set_verified(peer, !verified);
                                    cx.notify();
                                });
                                cx.notify();
                            })),
                        )
                    }),
            )
    }
}

// The conversation's menu.

pub fn conversation_menu(session: &Entity<Session>, peer: UserId, at: Point<Pixels>, _: &mut Window, cx: &mut App) {
    let blocked = session.read(cx).blocked.contains(&peer);
    let name = session.read(cx).display_name(Some(peer), None);
    let (s1, s2) = (session.clone(), session.clone());
    let items = vec![
        MenuEntry::item("shield-check", tr!("Compare safety number", "Comparar código de segurança"), false, move |window, cx| {
            safety_number(&s1, peer, window, cx)
        }),
        MenuEntry::rule(),
        MenuEntry::item(
            "ban",
            if blocked { trf!("Unblock {}", "Desbloquear {}", name) } else { trf!("Block {}", "Bloquear {}", name) },
            !blocked,
            move |window, cx| {
                if blocked {
                    s2.update(cx, |s, cx| s.set_blocked(peer, false, cx));
                    return;
                }
                let session = s2.clone();
                let name = s2.read(cx).display_name(Some(peer), None);
                Ask::confirm_action(
                    trf!("Block {}?", "Bloquear {}?", name),
                    tr!(
                        "Neither of you will be able to send the other messages or calls until you unblock them. They aren't told.",
                        "Nenhum dos dois vai conseguir mandar mensagens ou ligar para o outro até você desbloquear. A pessoa não é avisada."
                    ),
                    tr!("Block", "Bloquear"),
                    window,
                    cx,
                    move |_, cx| session.update(cx, |s, cx| s.set_blocked(peer, true, cx)),
                );
            },
        ),
    ];
    let menu = cx.new(|_| Menu { items });
    overlay::open_menu(menu, at, cx);
}

// Picking someone to talk to.

type OnPick = std::rc::Rc<dyn Fn(UserId, &mut Window, &mut App)>;

/// A list of everybody, to pick who to write to.
pub fn pick_person(session: &Entity<Session>, window: &mut Window, cx: &mut App, picked: impl Fn(UserId, &mut Window, &mut App) + 'static) {
    let view = cx.new(|cx| {
        let search = cx.new(|cx| TextField::new(cx, false, 40).placeholder(tr!("Search people", "Buscar pessoas")));
        let sub = cx.observe(&search, |_: &mut People, _, cx| cx.notify());
        People { session: session.clone(), search, picked: std::rc::Rc::new(picked), _sub: sub }
    });
    overlay::open_dialog(view, window, cx);
}

struct People {
    session: Entity<Session>,
    search: Entity<TextField>,
    picked: OnPick,
    _sub: Subscription,
}

impl EventEmitter<Dismiss> for People {}

impl Focusable for People {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl Render for People {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let q = self.search.read(cx).text().trim().to_lowercase();
        let s = self.session.read(cx);
        let me = s.me.id;
        let mut people: Vec<User> = s
            .users
            .values()
            .filter(|u| u.id != me && (q.is_empty() || u.name().to_lowercase().contains(&q) || u.nickname.contains(&q)))
            .cloned()
            .collect();
        people.sort_by_key(|u| (!s.online.contains(&u.id), u.name().to_lowercase()));
        let online: Vec<bool> = people.iter().map(|u| s.online.contains(&u.id)).collect();
        let mut list = div().id("people").flex().flex_col().gap(px(1.)).max_h(px(360.)).overflow_y_scroll().px(px(8.)).pb(px(8.));
        if people.is_empty() {
            list = list.child(div().p(px(14.)).child(caption(tr!("Nobody by that name.", "Ninguém com esse nome."), t.text3)));
        }
        for (i, u) in people.into_iter().enumerate() {
            let img = self.session.update(cx, |s, cx| s.avatar(u.id, cx));
            let id = u.id;
            let hover = t.layer_hover;
            list = list.child(
                div()
                    .id(("person", u.id as u64))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(42.))
                    .px(px(8.))
                    .rounded(px(radius::INNER + 1.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(div().relative().child(avatar(u.name(), img, 28., None)).when(online[i], |d| {
                        d.child(div().absolute().right(px(-2.)).bottom(px(-2.)).size(px(10.)).rounded_full().border_2().border_color(t.pane).bg(t.success))
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().truncate().text_size(px(13.5)).text_color(t.text).child(u.name().to_string()))
                            .child(div().truncate().text_size(px(11.5)).text_color(t.text3).child(format!("@{}", u.nickname))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let picked = this.picked.clone();
                        cx.emit(Dismiss);
                        picked(id, window, cx);
                    })),
            );
        }
        dialog_card(&t, 420.)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .p(px(18.))
                    .child(title(tr!("New private conversation", "Nova conversa privada"), t.text))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(36.))
                            .px(px(10.))
                            .rounded(px(radius::CONTROL))
                            .bg(t.control)
                            .border_1()
                            .border_color(t.stroke)
                            .child(icon("search", 14., t.text3))
                            .child(div().flex_1().child(self.search.clone())),
                    ),
            )
            .child(list)
    }
}

// An incoming call.

pub type OnAnswer = std::rc::Rc<dyn Fn(ConversationId, &mut Window, &mut App)>;
pub type OnDecline = std::rc::Rc<dyn Fn(ConversationId, &mut App)>;

/// "Ana is calling you", with Answer and Decline, while it rings.
pub struct Incoming {
    pub session: Entity<Session>,
    pub conversation: ConversationId,
    pub caller: UserId,
    pub on_answer: OnAnswer,
    pub on_decline: OnDecline,
    pub focus: FocusHandle,
}

impl EventEmitter<Dismiss> for Incoming {}

impl Incoming {
    /// Answered on another computer, cancelled, or over: there is nothing to answer any more.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(Dismiss);
    }
}

impl Focusable for Incoming {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Incoming {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let img = self.session.update(cx, |s, cx| s.avatar(self.caller, cx));
        let name = self.session.read(cx).display_name(Some(self.caller), None);
        let conversation = self.conversation;
        dialog_card(&t, 340.).track_focus(&self.focus).child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.))
                .p(px(26.))
                .child(avatar(&name, img, 72., Some(t.success)))
                .child(title(name.clone(), t.text))
                .child(div().flex().items_center().gap(px(6.)).child(icon("lock", 12., t.text3)).child(caption(
                    tr!("is calling you · end-to-end encrypted", "está te ligando · criptografia de ponta a ponta"),
                    t.text3,
                )))
                .child(
                    div()
                        .flex()
                        .gap(px(14.))
                        .pt(px(10.))
                        .child(
                            round_button("call-decline", "phone-off", t.critical, &t)
                                .tooltip(tip(tr!("Decline", "Recusar"), &t))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    (this.on_decline)(conversation, cx);
                                    cx.emit(Dismiss);
                                })),
                        )
                        .child(
                            round_button("call-answer", "phone", t.success, &t)
                                .tooltip(tip(tr!("Answer", "Atender"), &t))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    (this.on_answer)(conversation, window, cx);
                                    cx.emit(Dismiss);
                                })),
                        ),
                ),
        )
    }
}

fn round_button(id: &'static str, glyph: &'static str, color: gpui::Hsla, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(52.))
        .rounded_full()
        .bg(color)
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .active(|s| s.opacity(0.75))
        .child(icon(glyph, 22., t.on_accent))
}
