//! The channel list: groups that fold, text and voice channels, who is in each voice channel
//! (lit while they speak), and the admin's menus for all of it.

use super::{Center, ServerView};
use crate::core::types::*;
use crate::prefs::{prefs, set_prefs};
use crate::dm::KeyState;
use crate::session::Session;
use crate::theme::{Theme, px, radius};
use crate::ui::overlay::{self, Ask, Dismiss, Field, menu_card, menu_item};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
};

impl ServerView {
    pub fn render_sidebar(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let admin = self.session.read(cx).me.role.is_admin();
        let layout: Vec<(Option<Group>, Vec<Channel>)> =
            self.session.read(cx).layout().into_iter().map(|(g, cs)| (g.cloned(), cs.into_iter().cloned().collect())).collect();
        let mut list = div().id("channels").flex().flex_col().gap(px(1.)).px(px(8.)).pb(px(8.)).flex_1().min_h(px(0.)).overflow_y_scroll();
        // Your conversations first, then the server's channels, scrolling as one list.
        if let Some(dms) = self.render_dms(t, window, cx) {
            list = list.child(dms);
        }
        let empty = self.session.read(cx).channels.is_empty();
        list = list
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .pt(px(10.))
                    .pb(px(4.))
                    .px(px(4.))
                    .child(icon("hash", 12., t.text3))
                    .child(label(tr!("Channels", "Canais"), t))
                    .child(div().flex_1())
                    .when(admin, |d| {
                        d.child(
                            icon_button("new-channel", "plus", t)
                                .size(px(22.))
                                .tooltip(tip(tr!("New channel or group", "Novo canal ou grupo"), t))
                                .on_click(cx.listener(|this, _, window, cx| this.new_channel(window, cx))),
                        )
                    }),
            )
            .when(empty, |d| {
                d.child(div().px(px(8.)).py(px(4.)).child(caption(
                    if admin {
                        tr!("No channels yet. Make one with the + above.", "Ainda não há canais. Crie um com o + acima.")
                    } else {
                        tr!("No channels yet. An admin can make some.", "Ainda não há canais. Um admin pode criar alguns.")
                    },
                    t.text3,
                )))
            });
        for (group, channels) in layout {
            if let Some(g) = &group {
                let folded = self.folded.contains(&g.id);
                // Folding hides the channels and their counts with them, so the heading carries
                // the total of those it hides.
                let hidden: Vec<ChannelId> = if folded {
                    channels.iter().filter(|c| !(c.kind == ChannelKind::Voice && self.has_people(c.id, cx))).map(|c| c.id).collect()
                } else {
                    Vec::new()
                };
                let s = self.session.read(cx);
                let hidden_mentions: u32 = hidden.iter().filter_map(|id| s.mentioned.get(id)).sum();
                let server = s.api.base();
                let hidden_unread = hidden.iter().any(|id| s.unread.contains(id) && !prefs(cx).channel_muted(&server, *id));
                let gid = g.id;
                let name = g.name.clone();
                list = list.child(
                    div()
                        .id(("group", g.id as u64))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .pt(px(14.))
                        .pb(px(4.))
                        .px(px(4.))
                        .cursor_pointer()
                        .child(icon(if folded { "chevron-right" } else { "chevron-down" }, 12., t.text3))
                        .child(label(g.name.clone(), t))
                        .when(hidden_mentions > 0, |d| {
                            d.child(div().flex_1()).child(badge_text(mention_count(hidden_mentions), t.on_accent, t.accent))
                        })
                        .when(hidden_mentions == 0 && hidden_unread, |d| d.child(div().flex_1()).child(unread_dot(t).mr(px(2.))))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.folded.remove(&gid) {
                                this.folded.insert(gid);
                            }
                            cx.notify();
                        }))
                        .when(admin, |d| {
                            let accent = t.accent;
                            d.drag_over::<DraggedChannel>(move |s, _, _, _| s.text_color(accent).bg(accent.opacity(0.12)))
                                .on_drop(
                                    cx.listener(move |this, d: &DraggedChannel, _, cx| this.move_channel(d.id, Drop::IntoGroup(gid), cx)),
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                        this.group_menu(gid, name.clone(), e.position, window, cx);
                                    }),
                                )
                        }),
                );
                if folded {
                    // A folded group still shows the voice channels people are in.
                    let busy: Vec<Channel> =
                        channels.iter().filter(|c| c.kind == ChannelKind::Voice && self.has_people(c.id, cx)).cloned().collect();
                    for c in &busy {
                        list = list.child(self.channel_row(c, t, window, cx));
                    }
                    continue;
                }
            }
            for c in &channels {
                list = list.child(self.channel_row(c, t, window, cx));
            }
        }
        if admin {
            // Dropping below everything takes a channel out of its group, to the end.
            let accent = t.accent;
            list = list.child(
                div()
                    .id("drop-end")
                    .h(px(28.))
                    .rounded(px(radius::INNER))
                    .drag_over::<DraggedChannel>(move |s, _, _, _| s.border_1().border_dashed().border_color(accent))
                    .on_drop(cx.listener(|this, d: &DraggedChannel, _, cx| this.move_channel(d.id, Drop::End, cx))),
            );
        }
        div().flex().flex_col().size_full().pt(px(6.)).child(list)
    }

    /// Private conversations, above the channels: the people you talk to, newest first, with
    /// what is unread and who is calling. Folds like a group. Says plainly when this computer
    /// can't open them yet, and nags gently until the recovery key is kept somewhere.
    fn render_dms(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.session.read(cx);
        let state = s.vault.state.clone();
        if matches!(state, KeyState::Unsupported) {
            return None;
        }
        let folded = self.folded.contains(&DMS_FOLD);
        let unread = s.dm_unread();
        let needs_backup = s.vault.needs_backup();
        let dms: Vec<(ConversationId, UserId, String, u32, bool)> = s
            .dm_list()
            .into_iter()
            .map(|d| (d.info.id, d.peer, d.preview.clone(), d.info.unread, s.calls.contains_key(&d.info.id)))
            .collect();
        let mut col = div().flex().flex_col().gap(px(1.)).pb(px(6.)).child(
            div()
                .id("dms-head")
                .flex()
                .items_center()
                .gap(px(4.))
                .pt(px(6.))
                .pb(px(4.))
                .px(px(4.))
                .cursor_pointer()
                .child(icon(if folded { "chevron-right" } else { "chevron-down" }, 12., t.text3))
                .child(label(tr!("Direct messages", "Mensagens diretas"), t))
                .child(div().flex_1())
                .when(folded && unread > 0, |d| d.child(badge_text(unread_count(unread), t.on_accent, t.accent)))
                .child(
                    icon_button("new-dm", "plus", t)
                        .size(px(22.))
                        .tooltip(tip(tr!("New private conversation", "Nova conversa privada"), t))
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            let view = cx.entity().downgrade();
                            super::private::pick_person(&this.session, window, cx, move |user, window, cx| {
                                if let Some(view) = view.upgrade() {
                                    view.update(cx, |v, cx| v.message_user(user, window, cx));
                                }
                            });
                        })),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.folded.remove(&DMS_FOLD) {
                        this.folded.insert(DMS_FOLD);
                    }
                    cx.notify();
                })),
        );
        if folded {
            return Some(col.into_any_element());
        }
        let session = self.session.clone();
        match state {
            KeyState::Locked => {
                col = col.child(key_card(
                    "lock",
                    tr!(
                        "Your private messages are locked on this computer. Unlock them with your recovery key.",
                        "Suas mensagens privadas estão trancadas neste computador. Desbloqueie com a sua chave de recuperação."
                    ),
                    tr!("Unlock", "Desbloquear"),
                    t,
                    move |window, cx| super::private::unlock(&session, window, cx),
                ));
            }
            KeyState::Failed(_) => {
                col = col.child(div().px(px(8.)).py(px(4.)).child(caption(
                    tr!("Private messages aren't available right now.", "As mensagens privadas não estão disponíveis agora."),
                    t.text3,
                )));
            }
            _ if needs_backup => {
                col = col.child(key_card(
                    "key",
                    tr!(
                        "Keep your recovery key: it's how you read your private messages on another computer.",
                        "Guarde sua chave de recuperação: é com ela que você lê suas mensagens privadas em outro computador."
                    ),
                    tr!("Show key", "Ver chave"),
                    t,
                    move |window, cx| super::private::backup(&session, window, cx),
                ));
            }
            _ => {}
        }
        if dms.is_empty() && self.session.read(cx).vault.ready() {
            col = col.child(div().px(px(8.)).py(px(4.)).child(caption(
                tr!(
                    "Talk to someone in private: press + or right-click a member.",
                    "Converse com alguém no privado: clique no + ou com o botão direito em um membro."
                ),
                t.text3,
            )));
        }
        for (id, peer, preview, unread, in_call) in dms {
            col = col.child(self.dm_row(id, peer, preview, unread, in_call, t, window, cx));
        }
        Some(col.into_any_element())
    }

    #[allow(clippy::too_many_arguments)]
    fn dm_row(
        &mut self,
        id: ConversationId,
        peer: UserId,
        preview: String,
        unread: u32,
        in_call: bool,
        t: &Theme,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let img = self.session.update(cx, |s, cx| s.avatar(peer, cx));
        let s = self.session.read(cx);
        let name = s.display_name(Some(peer), None);
        let online = s.online.contains(&peer);
        let open = self.center == Center::Chat(Room::Dm(id));
        let fg = if open || unread > 0 { t.text } else { t.text2 };
        let hover = t.layer_hover;
        div()
            .id(("dm", id as u64))
            .relative()
            .flex()
            .items_center()
            .gap(px(9.))
            .h(px(40.))
            .px(px(6.))
            .rounded(px(radius::INNER + 1.))
            .cursor_pointer()
            .when(open, |d| d.bg(t.layer_hover))
            .when(!open, |d| d.hover(move |s| s.bg(hover)))
            .when(open, |d| d.child(div().absolute().left(px(-8.)).top(px(12.)).w(px(3.)).h(px(16.)).rounded(px(2.)).bg(t.accent)))
            .child(div().relative().flex_none().child(avatar(&name, img, 26., None)).child(
                div()
                    .absolute()
                    .right(px(-2.))
                    .bottom(px(-2.))
                    .size(px(10.))
                    .rounded_full()
                    .border_2()
                    .border_color(t.pane)
                    .bg(if online { t.success } else { t.text3 }),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.5))
                            .text_color(fg)
                            .when(open || unread > 0, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                            .child(name),
                    )
                    .when(!preview.is_empty(), |d| {
                        d.child(div().truncate().text_size(px(11.5)).text_color(if unread > 0 { t.text2 } else { t.text3 }).child(preview))
                    }),
            )
            .when(in_call, |d| d.child(icon("phone", 13., t.success)))
            .when(unread > 0 && !open, |d| d.child(badge_text(unread_count(unread), t.on_accent, t.accent)))
            .on_click(cx.listener(move |this, _, window, cx| this.open_room(Room::Dm(id), window, cx)))
            .into_any_element()
    }

    fn has_people(&self, channel: ChannelId, cx: &App) -> bool {
        self.session.read(cx).rosters.get(&channel).is_some_and(|r| !r.is_empty())
    }

    fn channel_row(&mut self, c: &Channel, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = self.session.read(cx);
        let admin = s.me.role.is_admin();
        let voice = c.kind == ChannelKind::Voice;
        let in_call = self.call_channel(cx) == Some(c.id);
        let open = match self.center {
            Center::Chat(room) => room == Room::Channel(c.id),
            Center::Stage => in_call,
        };
        let mentions = s.mentioned.get(&c.id).copied().unwrap_or(0);
        let mentioned = mentions > 0;
        let muted = !voice && prefs(cx).channel_muted(&s.api.base(), c.id);
        let unread = !open && !muted && s.unread.contains(&c.id);
        let locked = c.locked && !s.unlocked.contains(&c.id);
        let roster: Vec<Member> = if voice { s.rosters.get(&c.id).cloned().unwrap_or_default() } else { Vec::new() };
        let (id, name) = (c.id, c.name.clone());
        let kind = c.kind;
        let fg = if open || unread {
            t.text
        } else if muted {
            t.text3
        } else {
            t.text2
        };
        let hover = t.layer_hover;
        let row = div()
            .id(("channel", c.id as u64))
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(8.))
            .rounded(px(radius::INNER + 1.))
            .cursor_pointer()
            .when(open, |d| d.bg(t.layer_hover))
            .when(!open, |d| d.hover(move |s| s.bg(hover)))
            // The accent pill OpenController's nav rail uses for the place you are.
            .when(open, |d| d.child(div().absolute().left(px(-8.)).top(px(8.)).w(px(3.)).h(px(16.)).rounded(px(2.)).bg(t.accent)))
            // Unread is the same pill, small: opening the channel grows it into the one above.
            .when(unread, |d| d.child(unread_dot(t).absolute().left(px(-9.)).top(px(13.5))))
            .child(icon(if voice { "volume" } else { "hash" }, 15., if in_call { t.success } else { t.text3 }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(14.))
                    .text_color(fg)
                    .when(open || mentioned || unread, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                    .child(c.name.clone()),
            )
            .when(c.locked, |d| d.child(icon("lock", 12., if locked { t.text3 } else { t.success })))
            .when(c.mic_locked, |d| d.child(icon("mic-off", 12., t.text3)))
            .when(muted, |d| d.child(icon("bell-off", 12., t.text3)))
            .when(mentioned, |d| d.child(badge_text(mention_count(mentions), t.on_accent, t.accent)))
            .on_click(cx.listener(move |this, _, window, cx| match kind {
                ChannelKind::Text => this.open_text(id, window, cx),
                ChannelKind::Voice => this.ask_join_voice(id, window, cx),
            }))
            .on_mouse_down(MouseButton::Right, {
                let name = name.clone();
                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                    this.channel_menu(id, name.clone(), kind, e.position, window, cx);
                })
            })
            .when(admin, |d| {
                let accent = t.accent;
                let dragged = DraggedChannel { id, name: name.clone(), kind };
                d.on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
                    // A line above the row shows where the channel will land.
                    .drag_over::<DraggedChannel>(move |s, _, _, _| s.border_t_2().border_color(accent))
                    .on_drop(cx.listener(move |this, d: &DraggedChannel, _, cx| this.move_channel(d.id, Drop::Before(id), cx)))
            });
        let mut block = div().id(("channel-block", c.id as u64)).flex().flex_col().rounded(px(radius::INNER + 1.)).child(row);
        // Admins move people by dropping them on another voice channel, its row or its people.
        if admin && voice {
            let tint = t.accent.opacity(0.12);
            block = block
                .drag_over::<DraggedPerson>(move |s, d, _, _| if d.from == id { s } else { s.bg(tint) })
                .on_drop(cx.listener(move |this, d: &DraggedPerson, _, cx| this.move_person(d, id, cx)));
        }
        if !roster.is_empty() {
            let mut people = div().flex().flex_col().pl(px(30.)).pb(px(4.)).gap(px(1.));
            for m in roster {
                people = people.child(self.voice_member_row(c.id, &m, t, window, cx));
            }
            block = block.child(people);
        }
        block.into_any_element()
    }

    fn voice_member_row(&mut self, channel: ChannelId, m: &Member, t: &Theme, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let speaking = self.is_speaking(Place::Channel(channel), m.user_id, cx);
        let me = self.session.read(cx).me.id == m.user_id;
        let admin = self.session.read(cx).me.role.is_admin();
        let name = self.session.read(cx).display_name(Some(m.user_id), Some(&m.nickname));
        let avatar_img = self.session.update(cx, |s, cx| s.avatar(m.user_id, cx));
        let hover = t.layer_hover;
        let user = m.user_id;
        let mid = m.mid;
        let (name_for_drag, avatar_for_drag) = (name.clone(), avatar_img.clone());
        div()
            .id(("voice-member", (channel as u64) << 20 | m.user_id as u64))
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(28.))
            .px(px(6.))
            .rounded(px(radius::INNER))
            .hover(move |s| s.bg(hover))
            .child(avatar(&name, avatar_img, 20., speaking.then_some(t.success)))
            .child(
                div().flex_1().min_w(px(0.)).truncate().text_size(px(13.)).text_color(if speaking { t.text } else { t.text2 }).child(name),
            )
            .when(m.publishes("s"), |d| d.child(live_chip(t, false)))
            .when(m.publishes("c"), |d| d.child(icon("camera", 13., t.text3)))
            .when(m.muted || m.silenced(), |d| d.child(icon("mic-off", 13., if m.force_muted { t.critical } else { t.text3 })))
            .when(m.deafened, |d| d.child(icon("headphones-off", 13., t.text3)))
            .when(!me, |d| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        this.peer_menu(Place::Channel(channel), user, mid, e.position, window, cx);
                    }),
                )
            })
            .when(admin && !me, |d| {
                let dragged = DraggedPerson { user, from: channel, name: name_for_drag, avatar: avatar_for_drag };
                d.cursor_grab().on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
            })
            .into_any_element()
    }

    // Admin actions.

    fn new_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        Ask::open(
            tr!("New channel", "Novo canal"),
            None,
            tr!("Create", "Criar"),
            false,
            vec![
                Field::Text {
                    label: tr!("Name", "Nome"),
                    value: String::new(),
                    placeholder: tr!("general", "geral"),
                    secret: false,
                    multiline: false,
                    max: 32,
                },
                Field::Choice {
                    label: tr!("Kind", "Tipo"),
                    options: vec![
                        ("text".into(), tr!("Text", "Texto").into()),
                        ("voice".into(), tr!("Voice", "Voz").into()),
                        ("group".into(), tr!("Group", "Grupo").into()),
                    ],
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
                let (name, kind, password) = (v[0].trim().to_string(), v[1].clone(), v[2].clone());
                if name.is_empty() {
                    return Some(tr!("Give it a name.", "Dê um nome a ele.").into());
                }
                session.update(cx, |s, cx| {
                    if kind == "group" {
                        s.call(cx, move |api| Box::pin(async move { api.create_group(&name).await }), |_, _, _| {});
                    } else {
                        let kind = if kind == "voice" { ChannelKind::Voice } else { ChannelKind::Text };
                        let pw = (!password.is_empty()).then_some(password);
                        s.call(cx, move |api| Box::pin(async move { api.create_channel(kind, &name, pw.as_deref()).await }), |_, _, _| {});
                    }
                });
                None
            },
        );
    }

    fn channel_menu(
        &mut self,
        id: ChannelId,
        name: String,
        kind: ChannelKind,
        at: gpui::Point<gpui::Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.clone();
        let admin = self.session.read(cx).me.role.is_admin();
        let owner = self.session.read(cx).me.role == Role::Owner;
        let server = self.session.read(cx).api.base();
        let muted = prefs(cx).channel_muted(&server, id);
        let mic_locked = self.session.read(cx).channel(id).is_some_and(|c| c.mic_locked);
        let mut items = vec![
            MenuEntry::item("edit", tr!("Rename or set a password", "Renomear ou definir senha"), false, {
                let (session, name) = (session.clone(), name.clone());
                move |window, cx| {
                    let session = session.clone();
                    Ask::open(
                        tr!("Edit channel", "Editar canal"),
                        Some(
                            if kind == ChannelKind::Voice {
                                tr!("Voice channel", "Canal de voz")
                            } else {
                                tr!("Text channel", "Canal de texto")
                            }
                            .into(),
                        ),
                        tr!("Save", "Salvar"),
                        false,
                        vec![
                            Field::Text {
                                label: tr!("Name", "Nome"),
                                value: name.clone(),
                                placeholder: "",
                                secret: false,
                                multiline: false,
                                max: 32,
                            },
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
                            let password = if v[2] == "remove" { Some(String::new()) } else { (!v[1].is_empty()).then(|| v[1].clone()) };
                            session.update(cx, |s, cx| {
                                s.call(
                                    cx,
                                    move |api| Box::pin(async move { api.update_channel(id, &name, password.as_deref()).await }),
                                    |_, _, _| {},
                                )
                            });
                            None
                        },
                    );
                }
            }),
            MenuEntry::item("delete", tr!("Delete this channel", "Apagar este canal"), true, move |window, cx| {
                let session = session.clone();
                Ask::confirm_action(
                    trf!("Delete #{}?", "Apagar #{}?", name),
                    tr!("Its messages go with it. This can't be undone.", "As mensagens vão junto. Não dá para desfazer."),
                    tr!("Delete", "Apagar"),
                    window,
                    cx,
                    move |_, cx| {
                        session
                            .update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.delete_channel(id).await }), |_, _, _| {}))
                    },
                );
            }),
        ];
        // The owner's alone: it leaves the owner the only one speaking.
        if kind == ChannelKind::Voice && owner {
            let session = self.session.clone();
            items.insert(
                1,
                MenuEntry::item(
                    if mic_locked { "mic" } else { "mic-off" },
                    if mic_locked {
                        tr!("Unlock microphones", "Desbloquear microfones")
                    } else {
                        tr!("Lock microphones (only you speak)", "Bloquear microfones (só você fala)")
                    },
                    false,
                    move |_, cx| {
                        let locked = !mic_locked;
                        session.update(cx, |s, cx| {
                            s.call(cx, move |api| Box::pin(async move { api.set_channel_mic_locked(id, locked).await }), |_, _, _| {})
                        })
                    },
                ),
            );
        }
        // Silencing is anyone's, and only for themselves.
        let mut entries = Vec::new();
        if kind == ChannelKind::Text {
            entries.push(MenuEntry::item(
                if muted { "bell" } else { "bell-off" },
                if muted {
                    tr!("Unmute channel", "Reativar canal")
                } else {
                    tr!("Mute channel (just for you)", "Silenciar canal (só para você)")
                },
                false,
                move |_, cx| set_prefs(cx, |p| p.set_channel_muted(&server, id, !muted)),
            ));
        }
        if admin {
            if !entries.is_empty() {
                entries.push(MenuEntry::rule());
            }
            entries.extend(items);
        }
        if entries.is_empty() {
            return;
        }
        let menu = cx.new(|_| Menu { items: entries });
        overlay::open_menu(menu, at, cx);
    }

    fn group_menu(&mut self, id: i64, name: String, at: gpui::Point<gpui::Pixels>, _: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let menu = cx.new(|_| Menu {
            items: vec![
                MenuEntry::item("edit", tr!("Rename this group", "Renomear este grupo"), false, {
                    let (session, name) = (session.clone(), name.clone());
                    move |window, cx| {
                        let session = session.clone();
                        Ask::open(
                            tr!("Rename group", "Renomear grupo"),
                            None,
                            tr!("Save", "Salvar"),
                            false,
                            vec![Field::Text {
                                label: tr!("Name", "Nome"),
                                value: name.clone(),
                                placeholder: "",
                                secret: false,
                                multiline: false,
                                max: 32,
                            }],
                            window,
                            cx,
                            move |v, _, cx| {
                                let name = v[0].trim().to_string();
                                session.update(cx, |s, cx| {
                                    s.call(cx, move |api| Box::pin(async move { api.rename_group(id, &name).await }), |_, _, _| {})
                                });
                                None
                            },
                        );
                    }
                }),
                MenuEntry::item("delete", tr!("Delete this group", "Apagar este grupo"), true, move |window, cx| {
                    let session = session.clone();
                    Ask::confirm_action(
                        trf!("Delete the group {}?", "Apagar o grupo {}?", name),
                        tr!("Its channels stay, outside any group.", "Os canais dele continuam, fora de qualquer grupo."),
                        tr!("Delete", "Apagar"),
                        window,
                        cx,
                        move |_, cx| {
                            session.update(cx, |s, cx| {
                                s.call(cx, move |api| Box::pin(async move { api.delete_group(id).await }), |_, _, _| {})
                            })
                        },
                    );
                }),
            ],
        });
        overlay::open_menu(menu, at, cx);
    }
}

/// Where a dragged channel goes.
pub enum Drop {
    Before(ChannelId),
    IntoGroup(i64),
    End,
}

/// A channel being dragged, and what follows the pointer meanwhile.
#[derive(Clone)]
pub struct DraggedChannel {
    id: ChannelId,
    name: String,
    kind: ChannelKind,
}

impl Render for DraggedChannel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::current();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(32.))
            .px(px(10.))
            .rounded(px(radius::INNER + 1.))
            .bg(t.popover)
            .border_1()
            .border_color(t.accent)
            .shadow_lg()
            .child(icon(if self.kind == ChannelKind::Voice { "volume" } else { "hash" }, 15., t.text2))
            .child(body(self.name.clone(), t.text))
    }
}

/// Someone in voice being dragged to another voice channel.
#[derive(Clone)]
pub struct DraggedPerson {
    user: UserId,
    from: ChannelId,
    name: String,
    avatar: Option<std::sync::Arc<gpui::RenderImage>>,
}

impl Render for DraggedPerson {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::current();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(8.))
            .rounded(px(radius::INNER + 1.))
            .bg(t.popover)
            .border_1()
            .border_color(t.accent)
            .shadow_lg()
            .child(avatar(&self.name, self.avatar.clone(), 20., None))
            .child(body(self.name.clone(), t.text))
    }
}

impl ServerView {
    /// The same `admin:move` the right-click menu's "Move to" sends.
    fn move_person(&mut self, d: &DraggedPerson, to: ChannelId, cx: &mut Context<Self>) {
        if d.from != to {
            super::request(&self.session, "admin:move", serde_json::json!({ "userId": d.user, "toChannelId": to }), cx);
        }
    }

    /// Moves a channel and sends the whole new order, as the server wants it.
    fn move_channel(&mut self, id: ChannelId, to: Drop, cx: &mut Context<Self>) {
        let s = self.session.read(cx);
        let groups: Vec<i64> = {
            let mut g: Vec<&Group> = s.groups.iter().collect();
            g.sort_by_key(|g| g.position);
            g.into_iter().map(|g| g.id).collect()
        };
        let mut order: Vec<(ChannelId, Option<i64>)> =
            s.layout().into_iter().flat_map(|(g, cs)| cs.into_iter().map(move |c| (c.id, g.map(|g| g.id)))).collect();
        let Some(from) = order.iter().position(|(c, _)| *c == id) else { return };
        let moving = order.remove(from);
        match to {
            Drop::Before(target) => {
                if target == id {
                    return;
                }
                let Some(at) = order.iter().position(|(c, _)| *c == target) else { return };
                let group = order[at].1;
                order.insert(at, (moving.0, group));
            }
            Drop::IntoGroup(g) => {
                let at = order.iter().rposition(|(_, cg)| *cg == Some(g)).map(|i| i + 1).unwrap_or(order.len());
                order.insert(at, (moving.0, Some(g)));
            }
            Drop::End => order.push((moving.0, None)),
        }
        self.session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.arrange(&groups, &order).await }), |_, _, _| {}));
    }
}

/// The fold key of the private conversations' section; channel groups use their (positive) ids.
const DMS_FOLD: i64 = -1;

/// "3", or "99+".
fn unread_count(n: u32) -> String {
    if n > 99 { "99+".into() } else { n.to_string() }
}

/// A small card in the sidebar about your private-message key, with the one thing to do about it.
fn key_card(
    glyph: &'static str,
    text: &'static str,
    action: &'static str,
    t: &Theme,
    on: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .mx(px(2.))
        .my(px(4.))
        .p(px(10.))
        .rounded(px(radius::CARD))
        .bg(t.tint(t.accent))
        .border_1()
        .border_color(t.accent.opacity(0.35))
        .child(div().flex().gap(px(8.)).child(icon(glyph, 15., t.accent)).child(div().flex_1().min_w(px(0.)).child(caption(text, t.text))))
        .child(button(SharedString::from(format!("key-card-{glyph}")), action, Kind::Primary, t).on_click(move |_, window, cx| on(window, cx)))
}

/// New messages you have not seen: the accent pill of the channel you are in, small.
fn unread_dot(t: &Theme) -> gpui::Div {
    div().flex_none().size(px(5.)).rounded_full().bg(t.accent)
}

/// "@ 3", for the mentions waiting in a channel or a folded group.
fn mention_count(n: u32) -> String {
    if n > 99 { "@ 99+".into() } else { format!("@ {n}") }
}

pub fn badge_text(s: impl Into<gpui::SharedString>, fg: gpui::Hsla, bg: gpui::Hsla) -> gpui::Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(16.))
        .min_w(px(16.))
        .px(px(4.))
        .rounded_full()
        .bg(bg)
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(fg)
        .child(s.into())
}

/// The red LIVE chip on someone sharing their screen. Over video it sits on a dark backdrop
/// whatever the palette, so it takes a red light enough to read there.
pub fn live_chip(t: &Theme, over_video: bool) -> gpui::Div {
    chip(tr!("Live", "Ao vivo"), if over_video { gpui::rgb(0xff6b78).into() } else { t.critical })
}

// A simple menu of actions.

type Action = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;
type Custom = Box<dyn Fn(&Theme, &mut Window, &mut App) -> AnyElement>;

pub struct MenuEntry {
    pub glyph: Option<&'static str>,
    pub text: SharedString,
    pub danger: bool,
    pub action: Option<Action>,
    /// Drawn instead of an item: anything, e.g. a volume slider.
    pub custom: Option<Custom>,
}

impl MenuEntry {
    pub fn item(
        glyph: &'static str,
        text: impl Into<SharedString>,
        danger: bool,
        action: impl Fn(&mut Window, &mut App) + 'static,
    ) -> MenuEntry {
        MenuEntry { glyph: Some(glyph), text: text.into(), danger, action: Some(std::rc::Rc::new(action)), custom: None }
    }

    pub fn rule() -> MenuEntry {
        MenuEntry { glyph: None, text: SharedString::default(), danger: false, action: None, custom: None }
    }

    pub fn custom(f: impl Fn(&Theme, &mut Window, &mut App) -> AnyElement + 'static) -> MenuEntry {
        MenuEntry { glyph: None, text: SharedString::default(), danger: false, action: None, custom: Some(Box::new(f)) }
    }
}

pub struct Menu {
    pub items: Vec<MenuEntry>,
}

impl EventEmitter<Dismiss> for Menu {}

impl Render for Menu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::current();
        let mut card = menu_card(&t);
        for (i, e) in self.items.iter().enumerate() {
            if let Some(custom) = &e.custom {
                card = card.child(custom(&t, window, cx));
            } else if let Some(action) = e.action.clone() {
                card = card.child(menu_item(("menu-item", i), e.glyph, e.text.clone(), e.danger, &t).on_click(cx.listener(
                    move |_, _, window, cx| {
                        cx.emit(Dismiss);
                        action(window, cx);
                    },
                )));
            } else {
                card = card.child(overlay::menu_rule(&t));
            }
        }
        card
    }
}

pub fn _unused(_: &Entity<Session>) {}

#[cfg(test)]
mod tests {
    use super::mention_count;

    #[test]
    fn mention_counts_stop_at_99() {
        assert_eq!(mention_count(1), "@ 1");
        assert_eq!(mention_count(99), "@ 99");
        assert_eq!(mention_count(100), "@ 99+");
    }
}
