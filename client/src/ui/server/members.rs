//! Everyone on the server, online first, with their role; the owner's and admins' menus for
//! promoting and removing people.

use super::ServerView;
use super::sidebar::{Menu, MenuEntry};
use crate::core::types::*;
use crate::theme::{Theme, px, radius};
use crate::ui::overlay::{self, Ask, Field};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, Window, div,
};
use std::collections::HashSet;

impl ServerView {
    pub fn render_members(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let s = self.session.read(cx);
        let mut users: Vec<User> = s.users.values().cloned().collect();
        users.sort_by_cached_key(|a| a.name().to_lowercase());
        let in_voice: HashSet<UserId> = s.rosters.values().flatten().map(|m| m.user_id).collect();
        let (online, offline): (Vec<User>, Vec<User>) = users.into_iter().partition(|u| s.online.contains(&u.id));
        let total = online.len() + offline.len();
        let mut list = div().id("members").flex().flex_col().gap(px(1.)).px(px(8.)).pb(px(10.)).flex_1().min_h(px(0.)).overflow_y_scroll();
        list = list.child(div().px(px(6.)).pt(px(4.)).pb(px(4.)).child(label(trf!("Online · {}", "Online · {}", online.len()), t)));
        for u in &online {
            list = list.child(self.member_row(u, true, in_voice.contains(&u.id), t, window, cx));
        }
        if !offline.is_empty() {
            list = list.child(div().px(px(6.)).pt(px(14.)).pb(px(4.)).child(label(trf!("Offline · {}", "Offline · {}", offline.len()), t)));
            for u in &offline {
                list = list.child(self.member_row(u, false, false, t, window, cx));
            }
        }
        let online_count = online.len();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(44.))
                    .px(px(14.))
                    .child(label(tr!("Members", "Membros"), t))
                    .child(mono(format!("{online_count}/{total}"), t.text3)),
            )
            .child(list)
    }

    fn member_row(&mut self, u: &User, online: bool, in_voice: bool, t: &Theme, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let me = self.session.read(cx).me.clone();
        let img = self.session.update(cx, |s, cx| s.avatar(u.id, cx));
        let can_manage = me.role.is_admin() && u.id != me.id && u.role != Role::Owner;
        let user = u.clone();
        let hover = t.layer_hover;
        div()
            .id(("member", u.id as u64))
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(36.))
            .px(px(6.))
            .rounded(px(radius::INNER + 1.))
            .hover(move |s| s.bg(hover))
            .when(!online, |d| d.opacity(0.45))
            .child(div().relative().child(avatar(u.name(), img, 26., None)).when(online, |d| {
                d.child(
                    div()
                        .absolute()
                        .right(px(-2.))
                        .bottom(px(-2.))
                        .size(px(10.))
                        .rounded_full()
                        .border_2()
                        .border_color(t.pane)
                        .bg(if in_voice { t.accent } else { t.success }),
                )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(div().truncate().text_size(px(13.5)).text_color(t.text).child(u.name().to_string()))
                    .when(u.custom_name, |d| {
                        d.child(div().truncate().text_size(px(11.)).text_color(t.text3).child(format!("@{}", u.nickname)))
                    }),
            )
            .when(u.role == Role::Owner, |d| d.child(icon("crown", 13., t.caution)))
            .when(u.role == Role::Admin, |d| d.child(icon("shield", 13., t.accent)))
            .when(can_manage, |d| {
                d.cursor_pointer().on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| this.member_menu(&user, e.position, window, cx)),
                )
            })
            .into_any_element()
    }

    fn member_menu(&mut self, u: &User, at: gpui::Point<gpui::Pixels>, _: &mut Window, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me.clone();
        let session = self.session.clone();
        let (id, nick) = (u.id, u.nickname.clone());
        let mut items = Vec::new();
        if u.role == Role::Member {
            let session = session.clone();
            items.push(MenuEntry::item("shield", tr!("Make admin", "Tornar admin"), false, move |_, cx| {
                session
                    .update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.set_role(id, Role::Admin).await }), |_, _, _| {}))
            }));
        } else if me.role == Role::Owner {
            let session = session.clone();
            items.push(MenuEntry::item("shield-off", tr!("Remove admin", "Remover admin"), false, move |_, cx| {
                session
                    .update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.set_role(id, Role::Member).await }), |_, _, _| {}))
            }));
        }
        if me.role == Role::Owner {
            items.push(MenuEntry::rule());
            items.push(MenuEntry::item("delete", tr!("Delete account", "Apagar conta"), true, move |window, cx| {
                let session = session.clone();
                let nick = nick.clone();
                Ask::open(
                    trf!("Delete {}'s account?", "Apagar a conta de {}?", nick),
                    Some(
                        tr!(
                            "Their messages go too. They can register again unless the server has a password. Type their name to confirm.",
                            "As mensagens vão junto. A pessoa pode se cadastrar de novo, a menos que o servidor tenha senha. Digite o nome dela para confirmar."
                        )
                        .into(),
                    ),
                    tr!("Delete account", "Apagar conta"),
                    true,
                    vec![Field::Text { label: tr!("Name", "Nome"), value: String::new(), placeholder: "", secret: false, multiline: false, max: 32 }],
                    window,
                    cx,
                    move |v, _, cx| {
                        if v[0].trim().to_lowercase() != nick {
                            return Some(trf!("Type {} to confirm.", "Digite {} para confirmar.", nick));
                        }
                        session
                            .update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.delete_account(id).await }), |_, _, _| {}));
                        None
                    },
                );
            }));
        }
        if items.is_empty() {
            return;
        }
        let menu = cx.new(|_| Menu { items });
        overlay::open_menu(menu, at, cx);
    }
}
