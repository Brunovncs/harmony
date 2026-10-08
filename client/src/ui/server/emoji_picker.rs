//! The emoji picker: the server's own emoji, the ones you used lately, then every standard emoji
//! by section, with search. Picks are characters, or `:name:` for the server's.

use crate::core::types::CustomEmoji;
use crate::emoji;
use crate::prefs::prefs;
use crate::session::{Picture, Session};
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{current, px, radius};
use crate::ui::overlay::Dismiss;
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, Focusable, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, StyledImage, Window, div, img, uniform_list,
};
use std::rc::Rc;

const PER_ROW: usize = 9;
const LIMIT: usize = 300;

#[derive(Clone)]
enum Cell {
    Standard(&'static str, String),
    Custom(CustomEmoji),
}

enum Row {
    Heading(SharedString),
    Cells(Vec<Cell>),
}

type OnPick = Rc<dyn Fn(String, &mut Window, &mut App)>;

pub struct EmojiPicker {
    session: Entity<Session>,
    search: Entity<TextField>,
    rows: Vec<Row>,
    hovered: Option<String>,
    on_pick: OnPick,
}

impl EventEmitter<Dismiss> for EmojiPicker {}

impl EmojiPicker {
    pub fn new(
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut App,
        on_pick: impl Fn(String, &mut Window, &mut App) + 'static,
    ) -> Entity<EmojiPicker> {
        let view = cx.new(|cx| {
            let search = cx.new(|cx| TextField::new(cx, false, 40).placeholder(tr!("Find an emoji", "Buscar um emoji")));
            cx.subscribe(&search, |this: &mut EmojiPicker, _, ev: &TextFieldEvent, cx| {
                if let TextFieldEvent::Changed = ev {
                    this.rebuild(cx);
                }
            })
            .detach();
            let mut p = EmojiPicker { session, search, rows: Vec::new(), hovered: None, on_pick: Rc::new(on_pick) };
            p.rebuild(cx);
            p
        });
        let f = view.read(cx).search.focus_handle(cx);
        f.focus(window, cx);
        view
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let q = emoji::normalize_query(&self.search.read(cx).text());
        let custom: Vec<CustomEmoji> = self.session.read(cx).emojis.clone();
        let mut rows = Vec::new();
        let push = |rows: &mut Vec<Row>, title: &str, cells: Vec<Cell>| {
            if cells.is_empty() {
                return;
            }
            rows.push(Row::Heading(title.to_string().into()));
            for chunk in cells.chunks(PER_ROW) {
                rows.push(Row::Cells(chunk.to_vec()));
            }
        };
        if q.is_empty() {
            push(&mut rows, tr!("This server", "Deste servidor"), custom.into_iter().map(Cell::Custom).collect());
            let recent: Vec<Cell> = prefs(cx)
                .recent_emoji
                .iter()
                .filter_map(|r| match r.strip_prefix(':').and_then(|x| x.strip_suffix(':')) {
                    Some(name) => self.session.read(cx).emoji(name).cloned().map(Cell::Custom),
                    None => emojis::get(r).map(|e| Cell::Standard(e.as_str(), emoji::name_of(e.as_str()).unwrap_or_default())),
                })
                .collect();
            push(&mut rows, tr!("Recent", "Recentes"), recent);
            for s in emoji::sections() {
                push(
                    &mut rows,
                    s.title(),
                    s.items.iter().map(|e| Cell::Standard(e.as_str(), emoji::name_of(e.as_str()).unwrap_or_default())).collect(),
                );
            }
        } else {
            let custom: Vec<Cell> = custom.into_iter().filter(|c| c.name.contains(&q)).map(Cell::Custom).collect();
            let n = custom.len();
            push(&mut rows, tr!("This server", "Deste servidor"), custom);
            let found = emoji::search(&q, LIMIT.saturating_sub(n));
            push(
                &mut rows,
                tr!("Results", "Resultados"),
                found.into_iter().map(|e| Cell::Standard(e.as_str(), emoji::name_of(e.as_str()).unwrap_or_default())).collect(),
            );
        }
        self.rows = rows;
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Dismiss);
        add_emoji(self.session.clone(), window, cx);
    }

    fn remove(&mut self, emoji: CustomEmoji, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let s = self.session.read(cx);
        if emoji.uploaded_by != Some(s.me.id) && !s.me.role.is_admin() {
            return;
        }
        let session = self.session.clone();
        let menu = cx.new(|_| super::sidebar::Menu {
            items: vec![super::sidebar::MenuEntry::item("delete", trf!("Remove :{}:", "Remover :{}:", emoji.name), true, move |_, cx| {
                let id = emoji.id;
                session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.delete_emoji(id).await }), |_, _, _| {}));
            })],
        });
        crate::ui::overlay::open_menu(menu, at, cx);
    }

    fn pick(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Dismiss);
        (self.on_pick)(value, window, cx);
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = current();
        match &self.rows[ix] {
            Row::Heading(title) => {
                div().h(px(34.)).flex().items_end().px(px(4.)).pb(px(6.)).child(label(title.clone(), &t)).into_any_element()
            }
            Row::Cells(cells) => {
                let mut row = div().h(px(34.)).flex().gap(px(2.));
                for (i, c) in cells.clone().into_iter().enumerate() {
                    let (value, name, face): (String, String, AnyElement) = match &c {
                        Cell::Standard(e, n) => {
                            (e.to_string(), n.clone(), div().text_size(px(22.)).child(e.to_string()).into_any_element())
                        }
                        Cell::Custom(ce) => {
                            let pic = self.session.update(cx, |s, cx| s.picture(&ce.hash, cx));
                            let face = match pic {
                                Picture::Ready(image) => img(image).size(px(24.)).object_fit(gpui::ObjectFit::Contain).into_any_element(),
                                _ => div().size(px(24.)).rounded(px(4.)).bg(t.control).into_any_element(),
                            };
                            (format!(":{}:", ce.name), ce.name.clone(), face)
                        }
                    };
                    let hover = t.layer_hover;
                    let n2 = name.clone();
                    row = row.child(
                        div()
                            .id(("emoji", ix * PER_ROW + i))
                            .size(px(34.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(radius::INNER))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .child(face)
                            .on_hover(cx.listener(move |this, on: &bool, _, cx| {
                                if *on {
                                    this.hovered = Some(n2.clone());
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| this.pick(value.clone(), window, cx)))
                            .when_some(
                                match &c {
                                    Cell::Custom(ce) => Some(ce.clone()),
                                    _ => None,
                                },
                                |d, ce| {
                                    d.on_mouse_down(
                                        gpui::MouseButton::Right,
                                        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| this.remove(ce.clone(), e.position, cx)),
                                    )
                                },
                            ),
                    );
                }
                row.into_any_element()
            }
        }
    }
}

/// A picture made into an emoji: fit to 128 px, as PNG, named after the file unless renamed.
pub fn add_emoji(session: Entity<Session>, window: &mut Window, cx: &mut App) {
    let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(tr!("Make an emoji", "Criar emoji").into()),
    });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = paths.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let png = (|| -> Result<Vec<u8>, String> {
            let img = image::open(&path).map_err(|_| {
                tr!("That file is not a picture Harmony can read.", "Esse arquivo não é uma imagem que o Harmony consiga ler.").to_string()
            })?;
            let img =
                if img.width() > 128 || img.height() > 128 { img.resize(128, 128, image::imageops::FilterType::Lanczos3) } else { img };
            let mut out = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
            if out.len() > 256 * 1024 {
                Err(tr!("That picture is too big even at 128 px.", "Essa imagem é grande demais mesmo com 128 px.").into())
            } else {
                Ok(out)
            }
        })();
        let stem: String = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
            .take(32)
            .collect();
        let _ = handle.update(cx, |_, window, cx| match png {
            Ok(png) => crate::ui::overlay::Ask::open(
                tr!("Name the emoji", "Dê um nome ao emoji"),
                Some(tr!("People type it as :name: in messages.", "As pessoas digitam :nome: nas mensagens.").into()),
                tr!("Add", "Adicionar"),
                false,
                vec![crate::ui::overlay::Field::Text {
                    label: tr!("Name", "Nome"),
                    value: stem.clone(),
                    placeholder: "party_parrot",
                    secret: false,
                    multiline: false,
                    max: 32,
                }],
                window,
                cx,
                move |v, _, cx| {
                    let (name, png) = (v[0].trim().trim_matches(':').to_lowercase().replace(['-', ' '], "_"), png.clone());
                    session.update(cx, |s, cx| {
                        s.call(
                            cx,
                            move |api| {
                                Box::pin(async move {
                                    let up = api.upload(png, "image/png").await?;
                                    api.add_emoji(&name, &up.hash).await
                                })
                            },
                            |_, _, cx| crate::ui::overlay::toast(tr!("Emoji added.", "Emoji adicionado."), cx),
                        )
                    });
                    None
                },
            ),
            Err(e) => crate::ui::overlay::toast(e, cx),
        });
    })
    .detach();
}

impl Render for EmojiPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let count = self.rows.len();
        div()
            .w(px(PER_ROW as f32 * 36. + 24.))
            .h(px(420.))
            .flex()
            .flex_col()
            .rounded(px(radius::CONTROL + 2.))
            .bg(t.popover)
            .border_1()
            .border_color(t.stroke_strong)
            .shadow_lg()
            .overflow_hidden()
            .child(
                div().flex().items_center().gap(px(6.)).p(px(10.)).child(div().flex_1().child(self.search.clone())).child(
                    icon_label_button("emoji-add", "plus", tr!("Add", "Adicionar"), Kind::Standard, &t)
                        .tooltip(tip(tr!("Add a picture as this server's emoji", "Adicionar uma imagem como emoji deste servidor"), &t))
                        .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                ),
            )
            .child(if count == 0 {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(caption(tr!("No emoji match that.", "Nenhum emoji encontrado."), t.text3))
                    .into_any_element()
            } else {
                uniform_list(
                    "emoji-rows",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()),
                )
                .flex_1()
                .px(px(10.))
                .into_any_element()
            })
            .child(div().h(px(34.)).px(px(12.)).flex().items_center().border_t_1().border_color(t.stroke).bg(t.layer).child(mono(
                self.hovered.as_ref().map(|h| format!(":{h}:")).unwrap_or_else(|| tr!("Pick an emoji", "Escolha um emoji").into()),
                t.text3,
            )))
            .when(false, |d| d)
    }
}
