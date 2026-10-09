//! Private messages in the settings: whether this computer can open them, the recovery key, and
//! who you blocked.

use super::Settings;
use crate::dm::KeyState;
use crate::theme::{Theme, px, radius};
use crate::ui::server::private;
use crate::widgets::*;
use gpui::{AnyElement, Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div};

impl Settings {
    pub(super) fn privacy_section(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.session.read(cx);
        let state = s.vault.state.clone();
        let has_recovery = s.vault.recovery().is_some();
        let mut blocked: Vec<(i64, String)> = s.blocked.iter().map(|u| (*u, s.display_name(Some(*u), None))).collect();
        blocked.sort_by_key(|(_, n)| n.to_lowercase());
        let (status, color, glyph) = match &state {
            KeyState::Ready => (tr!("On, on this computer", "Ativas neste computador"), t.success, "shield-check"),
            KeyState::Locked => (tr!("Locked on this computer", "Trancadas neste computador"), t.caution, "lock"),
            KeyState::Loading => (tr!("Getting ready…", "Preparando…"), t.text3, "clock"),
            KeyState::Unsupported => (tr!("This server is too old for them", "Este servidor é antigo demais para elas"), t.text3, "info"),
            KeyState::Failed(_) => (tr!("Not available right now", "Indisponíveis agora"), t.critical, "warning"),
        };
        let session = self.session.clone();
        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(14.))
                    .rounded(px(radius::CARD))
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .child(div().flex().items_center().gap(px(8.)).child(icon(glyph, 16., color)).child(body(status, t.text)))
                    .child(caption(
                        tr!(
                            "Private messages and calls are encrypted on your computers before they leave. Only you and the person you talk to can open them: not the server, and not whoever runs it.",
                            "Mensagens e chamadas privadas são criptografadas nos seus computadores antes de sair. Só você e a pessoa com quem conversa conseguem abrir: nem o servidor, nem quem o administra."
                        ),
                        t.text2,
                    )),
            );

        // The recovery key.
        let mut key = div().flex().flex_col().gap(px(8.)).child(label(tr!("Recovery key", "Chave de recuperação"), t)).child(caption(
            tr!(
                "It opens your private messages on another computer, or after reinstalling. Keep it somewhere only you can get to.",
                "Ela abre suas mensagens privadas em outro computador ou depois de reinstalar. Guarde em um lugar que só você acessa."
            ),
            t.text3,
        ));
        let mut buttons = div().flex().gap(px(8.));
        match state {
            KeyState::Ready => {
                if has_recovery {
                    let s1 = session.clone();
                    buttons = buttons.child(
                        button("privacy-show", tr!("Show my recovery key", "Mostrar minha chave"), Kind::Standard, t)
                            .on_click(move |_, window, cx| private::backup(&s1, window, cx)),
                    );
                }
                let s2 = session.clone();
                buttons = buttons.child(
                    button("privacy-new", tr!("Make a new one", "Gerar uma nova"), Kind::Standard, t)
                        .on_click(move |_, window, cx| private::replace_recovery(&s2, window, cx)),
                );
            }
            KeyState::Locked => {
                let s1 = session.clone();
                buttons = buttons.child(
                    button("privacy-unlock", tr!("Unlock with my recovery key", "Desbloquear com minha chave"), Kind::Primary, t)
                        .on_click(move |_, window, cx| private::unlock(&s1, window, cx)),
                );
            }
            _ => {}
        }
        key = key.child(buttons);
        col = col.child(key);

        // Who you blocked.
        let mut people = div().flex().flex_col().gap(px(6.)).child(label(tr!("Blocked people", "Pessoas bloqueadas"), t));
        if blocked.is_empty() {
            people = people.child(caption(
                tr!(
                    "Nobody. Block someone from the ⋯ menu of your conversation with them.",
                    "Ninguém. Bloqueie alguém pelo menu ⋯ da conversa com a pessoa."
                ),
                t.text3,
            ));
        }
        for (id, name) in blocked {
            let session = session.clone();
            people = people.child(
                div()
                    .id(("blocked", id as u64))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(38.))
                    .px(px(10.))
                    .rounded(px(radius::INNER + 1.))
                    .bg(t.layer)
                    .child(icon("ban", 14., t.text3))
                    .child(div().flex_1().child(body(name, t.text)))
                    .child(
                        button(("unblock", id as u64), tr!("Unblock", "Desbloquear"), Kind::Subtle, t)
                            .on_click(move |_, _, cx| session.update(cx, |s, cx| s.set_blocked(id, false, cx))),
                    ),
            );
        }
        col.child(people).into_any_element()
    }
}
