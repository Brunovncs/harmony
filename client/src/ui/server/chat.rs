//! A text channel: its messages (grouped by sender, with reactions, attachments and pins), search,
//! and the box you write in, with attachments, emoji and @mentions.

use super::emoji_picker::EmojiPicker;
use super::rich::{self, Lookup};
use crate::core::types::*;
use crate::core::{self};
use crate::prefs::{prefs, set_prefs};
use crate::session::{Fetch, Picture, Session, SessionEvent, decode_picture};
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{MONO, Theme, current, px, radius};
use crate::ui::overlay::{self, Ask, Dismiss, Field, dialog_card};
use crate::widgets::*;
use chrono::{Local, TimeZone};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, ClipboardEntry, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent, ParentElement, PathPromptOptions, Render,
    RenderImage, SharedString, StatefulInteractiveElement, Styled, StyledImage, Subscription, Task, Window, div, img, list,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;
const MAX_UPLOAD: usize = 25 * 1024 * 1024;

struct Pending {
    bytes: Arc<Vec<u8>>,
    content_type: String,
    name: String,
    preview: Option<Arc<RenderImage>>,
}

/// What a chat pane asks of the window around it.
pub enum ChatEvent {
    /// Ring the other person of this private conversation.
    Call(ConversationId),
}

pub struct ChatView {
    session: Entity<Session>,
    pub room: Room,
    list: ListState,
    shown: Vec<MessageId>,
    composer: Entity<TextField>,
    search: Entity<TextField>,
    results: Option<(String, Vec<Message>)>,
    searching: Task<()>,
    show_pins: bool,
    pending: Option<Pending>,
    uploading: bool,
    sending: bool,
    flash: Option<(MessageId, Instant)>,
    mention: Option<(String, usize)>,
    /// Parsed bodies by message, until it is edited; dropped when the server's emoji change.
    bodies: HashMap<MessageId, (Option<i64>, Rc<rich::Parsed>)>,
    bodies_emoji: usize,
    /// When an animated picture on screen next changes frame, and the wake set for it.
    frame_due: Option<Instant>,
    frame_timer: Task<()>,
    /// The language the placeholders were written in, so a switch rewrites them.
    lang: crate::i18n::Lang,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl Focusable for ChatView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }
}

impl EventEmitter<ChatEvent> for ChatView {}

impl ChatView {
    pub fn new(session: Entity<Session>, room: Room, window: &mut Window, cx: &mut Context<Self>) -> ChatView {
        let composer = cx.new(|cx| TextField::new(cx, true, 4000).lines(1, 10).enter_submits().bare().placeholder(composer_hint(room, session.read(cx))));
        let search = cx.new(|cx| TextField::new(cx, false, 200).bare().placeholder(tr!("Search this channel", "Buscar neste canal")));
        let list = ListState::new(0, ListAlignment::Bottom, gpui::px(600.));
        list.set_follow_mode(gpui::FollowMode::Tail);
        let mut subs = vec![
            cx.observe(&session, |this, _, cx| this.sync(cx)),
            cx.subscribe_in(&composer, window, |this, _, ev: &TextFieldEvent, window, cx| match ev {
                TextFieldEvent::Submit => this.send(window, cx),
                TextFieldEvent::Changed => this.update_mention(cx),
            }),
            cx.subscribe(&search, |this, _, ev: &TextFieldEvent, cx| {
                let _ = ev;
                this.run_search(cx);
            }),
        ];
        subs.push(cx.subscribe(&session, |this, _, ev: &SessionEvent, cx| {
            if let SessionEvent::Message(room, m) = ev
                && *room == this.room
                && m.user_id == Some(this.session.read(cx).me.id)
            {
                this.list.scroll_to_end();
            }
        }));
        let weak = cx.entity().downgrade();
        // The list is borrowed while this runs, so it goes by the event, never by the list itself,
        // and loads after the event is done.
        list.set_scroll_handler(move |e, _, cx| {
            if e.visible_range.start < 3
                && let Some(this) = weak.upgrade()
            {
                let (session, room) = (this.read(cx).session.clone(), this.read(cx).room);
                cx.defer(move |cx| session.update(cx, |s, cx| s.load_older(room, cx)));
            }
        });
        session.update(cx, |s, cx| s.open(room, cx));
        let mut v = ChatView {
            session,
            room,
            list,
            shown: Vec::new(),
            composer,
            search,
            results: None,
            searching: Task::ready(()),
            show_pins: false,
            pending: None,
            uploading: false,
            sending: false,
            flash: None,
            mention: None,
            bodies: HashMap::new(),
            bodies_emoji: 0,
            frame_due: None,
            frame_timer: Task::ready(()),
            lang: crate::i18n::lang(),
            focus: cx.focus_handle(),
            _subs: subs,
        };
        v.sync(cx);
        v
    }

    fn messages<'a>(&'a self, cx: &'a App) -> &'a [Message] {
        if let Some((_, r)) = &self.results {
            return r;
        }
        self.session.read(cx).chats.get(&self.room).map(|l| l.messages.as_slice()).unwrap_or(&[])
    }

    /// The private conversation on screen, and who it is with.
    fn dm(&self, cx: &App) -> Option<(ConversationId, UserId)> {
        let id = self.room.conversation()?;
        Some((id, self.session.read(cx).peer_of(id)?))
    }

    /// Whether this is somewhere you can write right now, and if not, why.
    fn blocked_reason(&self, cx: &App) -> Option<String> {
        let (_, peer) = self.dm(cx)?;
        let s = self.session.read(cx);
        let name = s.display_name(Some(peer), None);
        if s.blocked.contains(&peer) {
            Some(trf!("You blocked {}. Unblock them to write here again.", "Você bloqueou {}. Desbloqueie para voltar a escrever aqui.", name))
        } else if !s.vault.ready() {
            Some(tr!("Private messages are locked on this computer.", "As mensagens privadas estão trancadas neste computador.").into())
        } else if !s.vault.peer_has_key(peer) {
            Some(trf!(
                "{} can't receive private messages yet: they need to update Harmony and open it once.",
                "{} ainda não pode receber mensagens privadas: precisa atualizar o Harmony e abri-lo uma vez.",
                name
            ))
        } else {
            None
        }
    }

    /// Keeps the list in step with the messages, splicing so the scroll position holds.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let emoji = self.session.read(cx).emojis.len();
        if emoji != self.bodies_emoji {
            self.bodies.clear();
            self.bodies_emoji = emoji;
        }
        let ids: Vec<MessageId> = self.messages(cx).iter().map(|m| m.id).collect();
        if ids == self.shown {
            cx.notify();
            return;
        }
        let keep: HashSet<MessageId> = ids.iter().copied().collect();
        self.bodies.retain(|id, _| keep.contains(id));
        let old = &self.shown;
        if !old.is_empty() && ids.len() > old.len() && ids.ends_with(old) {
            self.list.splice(0..0, ids.len() - old.len());
        } else if !old.is_empty() && ids.len() > old.len() && ids.starts_with(old) {
            self.list.splice(old.len()..old.len(), ids.len() - old.len());
        } else {
            self.list.reset(ids.len());
        }
        self.shown = ids;
        cx.notify();
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer.focus_handle(cx).focus(window, cx);
    }

    // Sending.

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, ix)) = &self.mention {
            let ix = *ix;
            self.accept_mention(ix, window, cx);
            return;
        }
        let text = self.composer.read(cx).text().trim().to_string();
        if (text.is_empty() && self.pending.is_none()) || self.sending {
            return;
        }
        self.sending = true;
        let pending = self.pending.take();
        self.uploading = pending.is_some();
        self.composer.update(cx, |f, cx| f.clear(cx));
        cx.notify();
        if let Room::Dm(conversation) = self.room {
            let file = pending.map(|p| crate::session::Outgoing { bytes: p.bytes, name: p.name, mime: p.content_type });
            let task = self.session.update(cx, |s, cx| s.send_dm(conversation, text.clone(), file, cx));
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| this.sent(result.err().map(|e| (e.message, text)), cx));
            })
            .detach();
            return;
        }
        let Room::Channel(channel) = self.room else { return };
        let (api, cache) = {
            let s = self.session.read(cx);
            (s.api.clone(), s.cache.clone())
        };
        cx.spawn(async move |this, cx| {
            let result = core::run(async move {
                let attachment = match pending {
                    Some(p) => {
                        let up = api.upload(p.bytes.to_vec(), &p.content_type).await?;
                        cache.insert(&p.bytes, &p.content_type);
                        Some((up.hash, p.name))
                    }
                    None => None,
                };
                let body = if text.is_empty() { String::new() } else { text.clone() };
                api.send_message(channel, &body, attachment.as_ref().map(|(h, n)| (h.as_str(), n.as_str())))
                    .await
                    .map_err(|e| (e, text))
                    .map(|_| ())
                    .map_err(|(e, text)| crate::core::api::ApiError { body: serde_json::Value::String(text), ..e })
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                let failed = result.err().map(|e| {
                    let text = if let serde_json::Value::String(text) = &e.body { text.clone() } else { String::new() };
                    (e.message, text)
                });
                this.sent(failed, cx)
            });
        })
        .detach();
    }

    /// A send finished. On failure the text goes back in the box, so nothing typed is lost.
    fn sent(&mut self, failed: Option<(String, String)>, cx: &mut Context<Self>) {
        self.sending = false;
        self.uploading = false;
        if let Some((why, text)) = failed {
            if self.composer.read(cx).text().is_empty() {
                self.composer.update(cx, |f, cx| f.set_text(&text, cx));
            }
            overlay::toast(why, cx);
        }
        cx.notify();
    }

    fn attach(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("Attach", "Anexar").into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            let bytes = std::fs::read(&path);
            let _ = this.update(cx, |this, cx| match bytes {
                Ok(b) => this.set_pending(b, content_type_for(&name), name, cx),
                Err(e) => overlay::toast(trf!("Could not read that file: {}", "Não foi possível ler esse arquivo: {}", e), cx),
            });
        })
        .detach();
    }

    fn set_pending(&mut self, bytes: Vec<u8>, content_type: String, name: String, cx: &mut Context<Self>) {
        if bytes.len() > MAX_UPLOAD {
            overlay::toast(tr!("Files can be up to 25 MB.", "Os arquivos podem ter até 25 MB."), cx);
            return;
        }
        let bytes = Arc::new(bytes);
        let (b, ct, mine) = (bytes.clone(), content_type.clone(), bytes.clone());
        let preview = cx.background_executor().spawn(async move { decode_picture(&b, &ct, (128, 128)) });
        cx.spawn(async move |this, cx| {
            let Ok((img, _)) = preview.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(p) = this.pending.as_mut().filter(|p| Arc::ptr_eq(&p.bytes, &mine)) {
                    p.preview = Some(img);
                    cx.notify();
                }
            });
        })
        .detach();
        self.pending = Some(Pending { bytes, content_type, name, preview: None });
        cx.notify();
    }

    /// Ctrl+V with a picture on the clipboard attaches it.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.control
            && k.key == "v"
            && let Some(item) = cx.read_from_clipboard()
        {
            for entry in item.entries() {
                if let ClipboardEntry::Image(image) = entry {
                    let ext = match image.format {
                        gpui::ImageFormat::Png => "png",
                        gpui::ImageFormat::Jpeg => "jpg",
                        gpui::ImageFormat::Gif => "gif",
                        gpui::ImageFormat::Webp => "webp",
                        _ => "png",
                    };
                    let name = format!("pasted-{}.{ext}", Local::now().format("%Y%m%dT%H%M%S"));
                    self.set_pending(image.bytes.clone(), content_type_for(&name), name, cx);
                    cx.stop_propagation();
                    return;
                }
            }
        }
        if self.mention.is_some() {
            let n = self.mention_matches(cx).len();
            match k.key.as_str() {
                "up" if n > 0 => {
                    if let Some((_, ix)) = &mut self.mention {
                        *ix = (*ix + n - 1) % n;
                    }
                }
                "down" if n > 0 => {
                    if let Some((_, ix)) = &mut self.mention {
                        *ix = (*ix + 1) % n;
                    }
                }
                "tab" => {
                    let ix = self.mention.as_ref().map(|m| m.1).unwrap_or(0);
                    self.accept_mention(ix, window, cx);
                }
                "escape" => self.mention = None,
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    // @mentions.

    fn update_mention(&mut self, cx: &mut Context<Self>) {
        // Two people: there is nobody to point at.
        if self.room.conversation().is_some() {
            return;
        }
        let text = self.composer.read(cx).text();
        let word = text.rsplit(|c: char| c.is_whitespace()).next().unwrap_or("");
        self.mention = word.strip_prefix('@').filter(|w| !w.contains('@')).map(|w| (w.to_lowercase(), 0));
        if self.mention.is_some() && self.mention_matches(cx).is_empty() {
            self.mention = None;
        }
        cx.notify();
    }

    fn mention_matches(&self, cx: &App) -> Vec<(String, String, Option<UserId>)> {
        let Some((q, _)) = &self.mention else { return Vec::new() };
        let s = self.session.read(cx);
        let mut users: Vec<&User> =
            s.users.values().filter(|u| u.nickname.contains(q.as_str()) || u.name().to_lowercase().contains(q.as_str())).collect();
        users.sort_by_key(|u| (!u.nickname.starts_with(q.as_str()), u.nickname.clone()));
        let mut out: Vec<(String, String, Option<UserId>)> =
            users.into_iter().take(8).map(|u| (u.nickname.clone(), u.name().to_string(), Some(u.id))).collect();
        if "everyone".starts_with(q.as_str()) {
            out.push(("everyone".into(), tr!("Notifies the whole server", "Avisa o servidor inteiro").into(), None));
        }
        out
    }

    fn accept_mention(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.mention_matches(cx);
        let Some((nick, _, _)) = matches.get(ix) else { return };
        let text = self.composer.read(cx).text();
        let cut = text.rfind('@').unwrap_or(text.len());
        let new = format!("{}@{nick} ", &text[..cut]);
        self.composer.update(cx, |f, cx| f.set_text(&new, cx));
        self.mention = None;
        self.focus_composer(window, cx);
        cx.notify();
    }

    // Search.

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).text().trim().to_string();
        if q.is_empty() {
            self.results = None;
            self.sync(cx);
            return;
        }
        let api = self.session.read(cx).api.clone();
        let Room::Channel(channel) = self.room else { return };
        self.searching = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let q2 = q.clone();
            let got = core::run(async move { api.search(channel, &q2).await }).await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(mut r) = got {
                    r.results.reverse();
                    this.results = Some((q, r.results));
                    this.sync(cx);
                }
            });
        });
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search.update(cx, |f, cx| f.clear(cx));
        self.results = None;
        self.sync(cx);
    }

    /// Scrolls to a message and flashes it.
    fn jump_to(&mut self, id: MessageId, cx: &mut Context<Self>) {
        if self.results.is_some() {
            self.clear_search(cx);
        }
        if let Some(ix) = self.shown.iter().position(|m| *m == id) {
            self.list.scroll_to_reveal_item(ix);
            self.flash = Some((id, Instant::now()));
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(1600)).await;
                let _ = this.update(cx, |this, cx| {
                    this.flash = None;
                    cx.notify();
                });
            })
            .detach();
        } else {
            // Not loaded yet: fetch older pages, then try again.
            let ch = self.room;
            self.session.update(cx, |s, cx| s.load_older(ch, cx));
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(600)).await;
                let _ = this.update(cx, |this, cx| {
                    let more = this.session.read(cx).chats.get(&ch).is_some_and(|l| l.more_before);
                    if this.shown.contains(&id) || more {
                        this.jump_to(id, cx);
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    // Actions on a message.

    fn react(&mut self, m: &Message, emoji: String, cx: &mut Context<Self>) {
        let me = self.session.read(cx).me.id;
        let mine = m.reactions.iter().any(|r| r.emoji == emoji && r.user_ids.contains(&me));
        let id = m.id;
        remember_emoji(&emoji, cx);
        if self.room.conversation().is_some() {
            let task = self.session.update(cx, |s, cx| s.react_dm(m, emoji, cx));
            toast_failure(task, cx);
            return;
        }
        self.session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.react(id, &emoji, !mine).await }), |_, _, _| {}));
    }

    fn pick_reaction(&mut self, m: Message, at: gpui::Point<gpui::Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let picker = EmojiPicker::new(self.session.clone(), window, cx, move |emoji, _, cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.react(&m, emoji, cx));
            }
        });
        overlay::open_menu(picker, at, cx);
    }

    fn pick_emoji(&mut self, at: gpui::Point<gpui::Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let composer = self.composer.clone();
        let picker = EmojiPicker::new(self.session.clone(), window, cx, move |emoji, window, cx| {
            let text = composer.read(cx).text();
            let insert = if emoji.starts_with(':') { format!("{emoji} ") } else { emoji.clone() };
            composer.update(cx, |f, cx| f.set_text(&format!("{text}{insert}"), cx));
            remember_emoji(&emoji, cx);
            composer.focus_handle(cx).focus(window, cx);
        });
        overlay::open_menu(picker, at, cx);
    }

    fn edit(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let id = m.id;
        let private = self.room.conversation().is_some().then(|| m.clone());
        Ask::open(
            tr!("Edit message", "Editar mensagem"),
            None,
            tr!("Save", "Salvar"),
            false,
            vec![Field::Text {
                label: tr!("Message", "Mensagem"),
                value: m.body.clone(),
                placeholder: "",
                secret: false,
                multiline: true,
                max: 4000,
            }],
            window,
            cx,
            move |v, _, cx| {
                let body = v[0].trim().to_string();
                if let Some(m) = &private {
                    let task = session.update(cx, |s, cx| s.edit_dm(m, body, cx));
                    toast_failure(task, cx);
                    return None;
                }
                session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.edit(id, &body).await }), |_, _, _| {}));
                None
            },
        );
    }

    fn delete(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let id = m.id;
        let private = self.room.conversation().is_some();
        Ask::confirm_action(
            tr!("Delete this message?", "Apagar esta mensagem?"),
            if private {
                tr!("It goes for both of you. This can't be undone.", "Ela some para os dois. Não dá para desfazer.")
            } else {
                tr!("This can't be undone.", "Não dá para desfazer.")
            },
            tr!("Delete", "Apagar"),
            window,
            cx,
            move |_, cx| {
                session.update(cx, |s, cx| {
                    if private {
                        s.call(cx, move |api| Box::pin(async move { api.delete_dm(id).await }), |_, _, _| {})
                    } else {
                        s.call(cx, move |api| Box::pin(async move { api.delete_message(id).await }), |_, _, _| {})
                    }
                })
            },
        );
    }

    fn pin(&mut self, m: &Message, cx: &mut Context<Self>) {
        let (id, pinned) = (m.id, !m.pinned);
        self.session.update(cx, |s, cx| s.call(cx, move |api| Box::pin(async move { api.pin(id, pinned).await }), |_, _, _| {}));
    }

    fn save_attachment(&mut self, hash: String, name: String, how: Fetch, cx: &mut Context<Self>) {
        let dir = dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let target = cx.prompt_for_new_path(&dir, Some(&name));
        let bytes = self.session.read(cx).attachment(&hash, how);
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(path))) = target.await else { return };
            let result = async move {
                let bytes = bytes.await.map_err(|e| e.message)?;
                std::fs::write(&path, bytes.as_slice()).map_err(|e| e.to_string())
            }
            .await;
            cx.update(|cx| match result {
                Ok(()) => overlay::toast(tr!("Saved.", "Salvo."), cx),
                Err(e) => overlay::toast(trf!("Could not save it: {}", "Não foi possível salvar: {}", e), cx),
            });
        })
        .detach();
    }

    /// Swaps the file on one of your messages for another.
    fn replace_attachment(&mut self, id: MessageId, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("Replace", "Substituir").into()),
        });
        let session = self.session.clone();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            let Ok(bytes) = std::fs::read(&path) else { return };
            let ct = content_type_for(&name);
            cx.update(|cx| {
                session.update(cx, |s, cx| {
                    s.call(
                        cx,
                        move |api| {
                            Box::pin(async move {
                                let up = api.upload(bytes, &ct).await?;
                                api.replace_attachment(id, Some(&up.hash), Some(&name)).await
                            })
                        },
                        |_, _, _| {},
                    )
                })
            });
        })
        .detach();
    }

    /// Shows the thumbnail at once and swaps in the full picture when it has decoded.
    fn open_image(&mut self, image: Arc<RenderImage>, hash: String, name: String, how: Fetch, window: &mut Window, cx: &mut Context<Self>) {
        let chat = cx.entity().downgrade();
        let full = self.session.update(cx, |s, cx| s.full_picture(&hash, how.clone(), cx));
        let view = cx.new(|cx| {
            let load = cx.spawn(async move |this, cx| {
                if let Some(img) = full.await {
                    let _ = this.update(cx, |this: &mut Lightbox, cx| {
                        this.full = Some(img);
                        cx.notify();
                    });
                }
            });
            cx.on_release(|this: &mut Lightbox, cx| {
                if let Some(img) = this.full.take() {
                    cx.drop_image(img, None);
                }
            })
            .detach();
            Lightbox { image, full: None, hash, name, how, chat, focus: cx.focus_handle(), _load: load }
        });
        overlay::open_dialog(view, window, cx);
    }

    // Drawing.

    /// A message, and a wake for when an animated picture in it changes frame.
    fn render_message(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.session.update(cx, |s, _| s.take_frame_due());
        let el = self.render_message_inner(ix, window, cx);
        if let Some(due) = self.session.update(cx, |s, _| s.take_frame_due()) {
            self.wake_at(due, cx);
        }
        el
    }

    /// Draws again at `due`, unless a wake no later than that is already set.
    fn wake_at(&mut self, due: Instant, cx: &mut Context<Self>) {
        if self.frame_due.is_some_and(|d| d <= due) {
            return;
        }
        self.frame_due = Some(due);
        self.frame_timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(due.saturating_duration_since(Instant::now())).await;
            let _ = this.update(cx, |this, cx| {
                this.frame_due = None;
                cx.notify();
            });
        });
    }

    fn render_message_inner(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = current();
        let messages = self.messages(cx);
        let Some(m) = messages.get(ix).cloned() else { return div().into_any_element() };
        let prev = ix.checked_sub(1).and_then(|i| messages.get(i)).cloned();
        if let Some(call) = m.private.as_ref().and_then(|p| p.call.clone()) {
            return self.render_call_line(&m, &call, &t, cx);
        }
        let is_line = |p: &Message| p.private.as_ref().is_some_and(|p| p.call.is_some());
        let grouped = prev.as_ref().is_some_and(|p| {
            p.user_id == m.user_id
                && !p.pinned
                && !m.pinned
                && !is_line(p)
                && m.created_at - p.created_at < GROUP_WINDOW_MS
                && self.results.is_none()
        });
        let me = self.session.read(cx).me.clone();
        let mine = m.user_id == Some(me.id);
        let private = self.room.conversation().is_some();
        let unreadable = m.private.as_ref().is_some_and(|p| p.unreadable);
        // In a private conversation nobody has a say over your words but you: no admin delete.
        let can_delete = mine || (!private && me.role.is_admin());
        let for_me = (m.mentions.contains(&me.id) || m.mentions_everyone) && !mine;
        let flashing = self.flash.is_some_and(|(id, _)| id == m.id);
        let name = self.session.read(cx).display_name(m.user_id, m.nickname.as_deref());
        // Rank means nothing between two people.
        let role = m.user_id.and_then(|u| self.session.read(cx).users.get(&u)).map(|u| u.role).filter(|_| !private);
        let avatar_img = m.user_id.and_then(|u| self.session.update(cx, |s, cx| s.avatar(u, cx)));
        let body_el = if unreadable {
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon("lock", 13., t.text3))
                    .child(caption(
                        tr!(
                            "This message was sealed to a key this computer doesn't have.",
                            "Esta mensagem foi protegida com uma chave que este computador não tem."
                        ),
                        t.text3,
                    ))
                    .into_any_element(),
            )
        } else if m.body.trim().is_empty() {
            None
        } else {
            let parsed = self.parsed(&m, cx);
            let id = m.id;
            Some(self.session.update(cx, |s, cx| {
                let mut look = Lookup { session: s, me: me.id };
                rich::body(format!("body-{id}"), &parsed, 14., &t, &mut look, cx)
            }))
        };
        let attachment = self.render_attachment(&m, &t, window, cx);
        let reactions = self.render_reactions(&m, &t, cx);
        let time = Local.timestamp_millis_opt(m.created_at).single();
        let hhmm = time.map(|d| d.format("%H:%M").to_string()).unwrap_or_default();
        let stamp = time.map(|d| crate::i18n::message_time(&d, &Local::now())).unwrap_or_default();
        let full = time.map(|d| crate::i18n::date_time(&d)).unwrap_or_default();

        let group_name = SharedString::from(format!("msg-{}", m.id));
        let tools = {
            let (m1, m2, m3, m4) = (m.clone(), m.clone(), m.clone(), m.clone());
            div()
                .absolute()
                .top(px(-14.))
                .right(px(12.))
                .flex()
                .gap(px(2.))
                .p(px(3.))
                .rounded(px(radius::CONTROL))
                .bg(t.popover)
                .border_1()
                .border_color(t.stroke_strong)
                .shadow_md()
                .invisible()
                .group_hover(group_name.clone(), |s| s.visible())
                .child(
                    tool_button(("react", m.id as u64), "smile-plus", false, t.text2, &t)
                        .tooltip(tip(tr!("Add a reaction", "Adicionar uma reação"), &t))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.pick_reaction(m1.clone(), e.position, window, cx)
                            }),
                        ),
                )
                .when(!private, |d| {
                    d.child(
                        tool_button(("pin", m.id as u64), "pin", m.pinned, if m.pinned { t.accent } else { t.text2 }, &t)
                            .tooltip(tip(if m.pinned { tr!("Unpin", "Desafixar") } else { tr!("Pin to the channel", "Fixar no canal") }, &t))
                            .on_click(cx.listener(move |this, _, _, cx| this.pin(&m2, cx))),
                    )
                })
                .when(mine && !unreadable, |d| {
                    d.child(
                        tool_button(("edit", m.id as u64), "edit", false, t.text2, &t)
                            .tooltip(tip(tr!("Edit", "Editar"), &t))
                            .on_click(cx.listener(move |this, _, window, cx| this.edit(&m3, window, cx))),
                    )
                })
                .when(can_delete, |d| {
                    d.child(
                        tool_button(("delete", m.id as u64), "delete", false, t.critical, &t)
                            .tooltip(tip(tr!("Delete", "Apagar"), &t))
                            .on_click(cx.listener(move |this, _, window, cx| this.delete(&m4, window, cx))),
                    )
                })
        };

        let hover = t.layer;
        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .gap(px(4.))
            .when(!grouped, |d| {
                d.child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(8.))
                        .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text).child(name.clone()))
                        .when(role == Some(Role::Owner), |d| d.child(icon("crown", 12., t.caution)))
                        .when(role == Some(Role::Admin), |d| d.child(icon("shield", 12., t.accent)))
                        .child(
                            div()
                                .id(("time", m.id as u64))
                                .font_family(MONO)
                                .text_size(px(11.))
                                .text_color(t.text3)
                                .child(stamp.clone())
                                .tooltip(tip(full.clone(), &t)),
                        )
                        .when(m.pinned, |d| d.child(chip(tr!("Pinned", "Fixada"), t.accent)))
                        .when(m.edited_at.is_some(), |d| {
                            d.child(div().text_size(px(11.)).text_color(t.text3).child(tr!("(edited)", "(editada)")))
                        }),
                )
            })
            .children(body_el)
            .children(attachment)
            .children(reactions);

        div()
            .id(("message", m.id as u64))
            .group(group_name.clone())
            .relative()
            .w_full()
            .flex()
            .gap(px(12.))
            .px(px(16.))
            .py(px(if grouped { 2. } else { 6. }))
            .when(!grouped, |d| d.mt(px(8.)))
            .hover(move |s| s.bg(hover))
            .when(for_me, |d| d.bg(t.tint(t.caution)).border_l_2().border_color(t.caution))
            .when(flashing, |d| d.bg(t.accent_soft))
            .child(if grouped {
                div().w(px(36.)).flex_none().flex().justify_end().pt(px(2.)).child(
                    div()
                        .font_family(MONO)
                        .text_size(px(10.))
                        .text_color(t.text3)
                        .invisible()
                        .group_hover(group_name, |s| s.visible())
                        .child(hhmm),
                )
            } else {
                div().flex_none().pt(px(2.)).child(avatar(&name, avatar_img, 36., None))
            })
            .child(content)
            .child(tools)
            .into_any_element()
    }

    fn parsed(&mut self, m: &Message, cx: &App) -> Rc<rich::Parsed> {
        match self.bodies.get(&m.id) {
            Some((edited, parsed)) if *edited == m.edited_at => parsed.clone(),
            _ => {
                let parsed = Rc::new(rich::Parsed::new(&m.body, self.session.read(cx)));
                self.bodies.insert(m.id, (m.edited_at, parsed.clone()));
                parsed
            }
        }
    }

    /// "Missed call from Ana", "Call · 12 min": a line across the conversation, not a message.
    fn render_call_line(&mut self, m: &Message, call: &CallMeta, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.session.read(cx);
        let from_me = m.user_id == Some(s.me.id);
        let caller = s.display_name(m.user_id, None);
        let missed = call.outcome != "ended";
        let time = Local.timestamp_millis_opt(m.created_at).single();
        let stamp = time.map(|d| crate::i18n::message_time(&d, &Local::now())).unwrap_or_default();
        let color = if missed && !from_me { t.critical } else { t.text3 };
        div()
            .id(("call-line", m.id as u64))
            .w_full()
            .flex()
            .justify_center()
            .px(px(16.))
            .mt(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(7.))
                    .rounded_full()
                    .bg(t.layer)
                    .border_1()
                    .border_color(t.stroke)
                    .child(icon(if missed { "phone-off" } else { "phone" }, 14., color))
                    .child(div().text_size(px(13.)).text_color(t.text2).child(crate::dm::call_line(call, &caller, from_me)))
                    .child(mono(stamp, t.text3)),
            )
            .into_any_element()
    }

    fn render_attachment(&mut self, m: &Message, t: &Theme, _: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hash = m.attachment_hash.clone()?;
        let name = m.attachment_name.clone().unwrap_or_else(|| tr!("attachment", "anexo").into());
        let how = Fetch::of(m);
        if m.media_type == Some(MediaType::Image) {
            let pic = self.session.update(cx, |s, cx| s.picture_from(&hash, how.clone(), cx));
            return Some(match pic {
                Picture::Ready(image) => {
                    let (img2, h2, n2, f2) = (image.clone(), hash.clone(), name.clone(), how.clone());
                    div()
                        .id(("attachment", m.id as u64))
                        .max_w(px(420.))
                        .max_h(px(300.))
                        .rounded(px(radius::CARD))
                        .overflow_hidden()
                        .border_1()
                        .border_color(t.stroke)
                        .cursor_pointer()
                        .child(img(image).max_w(px(420.)).max_h(px(300.)).object_fit(gpui::ObjectFit::Contain))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.open_image(img2.clone(), h2.clone(), n2.clone(), f2.clone(), window, cx)),
                        )
                        .into_any_element()
                }
                Picture::Loading => div().w(px(240.)).h(px(160.)).rounded(px(radius::CARD)).bg(t.layer).into_any_element(),
                Picture::Failed => file_card(m.id, &name, &hash, how, t, cx),
            });
        }
        // Swapping the file is a channel's: a sealed message would have to be sealed again.
        let mine = m.user_id == Some(self.session.read(cx).me.id) && self.room.channel().is_some();
        let id = m.id;
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(file_card(m.id, &name, &hash, how, t, cx))
                .when(mine, |d| {
                    d.child(
                        icon_button(("replace", m.id as u64), "paperclip", t)
                            .tooltip(tip(tr!("Replace the file", "Substituir o arquivo"), t))
                            .on_click(cx.listener(move |this, _, _, cx| this.replace_attachment(id, cx))),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_reactions(&mut self, m: &Message, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if m.reactions.is_empty() {
            return None;
        }
        let me = self.session.read(cx).me.id;
        let mut row = div().flex().flex_wrap().gap(px(4.)).pt(px(2.));
        for (i, r) in m.reactions.iter().enumerate() {
            let mine = r.user_ids.contains(&me);
            let who: Vec<String> = r.user_ids.iter().map(|u| self.session.read(cx).display_name(Some(*u), None)).collect();
            let shown: AnyElement = match r.emoji.strip_prefix(':').and_then(|e| e.strip_suffix(':')) {
                Some(name) => {
                    let custom = self.session.read(cx).emoji(name).cloned();
                    match custom.map(|c| self.session.update(cx, |s, cx| s.picture(&c.hash, cx))) {
                        Some(Picture::Ready(image)) => img(image).size(px(16.)).into_any_element(),
                        _ => div().child(r.emoji.clone()).into_any_element(),
                    }
                }
                None => div().child(r.emoji.clone()).into_any_element(),
            };
            let (m2, emoji) = (m.clone(), r.emoji.clone());
            let hover = t.layer_hover;
            row = row.child(
                div()
                    .id(("reaction", (m.id as u64) << 8 | i as u64))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(8.))
                    .rounded_full()
                    .border_1()
                    .cursor_pointer()
                    .when(mine, |d| d.bg(t.accent_soft).border_color(t.accent.opacity(0.5)))
                    .when(!mine, |d| d.bg(t.layer).border_color(t.stroke).hover(move |s| s.bg(hover)))
                    .text_size(px(14.))
                    .child(shown)
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(11.))
                            .text_color(if mine { t.accent } else { t.text2 })
                            .child(r.count.to_string()),
                    )
                    .tooltip(tip(reacted(&who, &r.emoji), t))
                    .on_click(cx.listener(move |this, _, _, cx| this.react(&m2, emoji.clone(), cx))),
            );
        }
        Some(row.into_any_element())
    }

    /// A private conversation's header: who it is with and whether they are around, that it is
    /// sealed end to end, and the call button.
    fn render_dm_header(&mut self, conversation: ConversationId, peer: UserId, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let img = self.session.update(cx, |s, cx| s.avatar(peer, cx));
        let s = self.session.read(cx);
        let name = s.display_name(Some(peer), None);
        let online = s.online.contains(&peer);
        let verified = s.vault.verified(peer);
        let in_call = s.calls.contains_key(&conversation);
        let can_call = s.vault.ready() && s.vault.peer_has_key(peer) && !s.blocked.contains(&peer);
        let session = self.session.clone();
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(16.))
            .border_b_1()
            .border_color(t.stroke)
            .child(div().relative().child(avatar(&name, img, 28., None)).child(
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
                    .min_w(px(0.))
                    .child(title(name.clone(), t.text))
                    .child(caption(if online { tr!("Online", "Online") } else { tr!("Offline", "Offline") }, t.text3)),
            )
            .child(
                div()
                    .id("e2ee")
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(24.))
                    .px(px(8.))
                    .rounded_full()
                    .bg(t.tint(t.success))
                    .cursor_pointer()
                    .child(icon(if verified { "shield-check" } else { "lock" }, 12., t.success))
                    .child(div().text_size(px(11.5)).text_color(t.success).child(if verified {
                        tr!("Verified", "Verificada")
                    } else {
                        tr!("End-to-end encrypted", "Criptografia de ponta a ponta")
                    }))
                    .tooltip(tip(
                        tr!(
                            "Only the two of you can read this conversation or hear your calls, not even whoever runs the server. Click to compare your safety number.",
                            "Só vocês dois conseguem ler esta conversa e ouvir suas chamadas, nem quem administra o servidor. Clique para comparar o código de segurança."
                        ),
                        t,
                    ))
                    .on_click(move |_, window, cx| super::private::safety_number(&session, peer, window, cx)),
            )
            .child(div().flex_1())
            .child(
                tool_button("dm-call", "phone", in_call, if in_call { t.success } else if can_call { t.text2 } else { t.text3 }, t)
                    .when(!can_call, |d| d.opacity(0.5))
                    .tooltip(tip(if in_call { tr!("In a call", "Em chamada") } else { tr!("Start a call", "Iniciar uma chamada") }, t))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if can_call {
                            cx.emit(ChatEvent::Call(conversation));
                        }
                    })),
            )
            .child(
                icon_button("dm-more", "more", t).tooltip(tip(tr!("More", "Mais"), t)).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        super::private::conversation_menu(&this.session, peer, e.position, window, cx);
                    }),
                ),
            )
    }

    /// A notice above a private conversation: a key that changed, or why you can't write.
    fn render_dm_notice(&mut self, peer: UserId, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.session.read(cx);
        let name = s.display_name(Some(peer), None);
        if s.vault.ready() && s.vault.key_changed_for(peer) {
            let (session, s2) = (self.session.clone(), self.session.clone());
            return Some(
                notice(
                    "shield-alert",
                    trf!(
                        "{}'s security key changed. It happens when someone sets Harmony up again without their recovery key; if you weren't expecting it, compare your safety number with them.",
                        "A chave de segurança de {} mudou. Isso acontece quando alguém configura o Harmony de novo sem a chave de recuperação; se você não esperava por isso, comparem o código de segurança.",
                        name
                    ),
                    t.caution,
                    t,
                )
                .child(
                    button("key-compare", tr!("Compare", "Comparar"), Kind::Standard, t)
                        .on_click(move |_, window, cx| super::private::safety_number(&s2, peer, window, cx)),
                )
                .child(button("key-ok", tr!("Got it", "Entendi"), Kind::Primary, t).on_click(move |_, _, cx| {
                    session.update(cx, |s, cx| {
                        s.vault.accept_key(peer);
                        cx.notify();
                    })
                }))
                .into_any_element(),
            );
        }
        let why = self.blocked_reason(cx)?;
        let s = self.session.read(cx);
        let blocked = s.blocked.contains(&peer);
        let locked = !s.vault.ready();
        let session = self.session.clone();
        Some(
            notice(if blocked { "ban" } else { "lock" }, why, t.text2, t)
                .when(blocked, |d| {
                    d.child(button("unblock", tr!("Unblock", "Desbloquear"), Kind::Standard, t).on_click(move |_, _, cx| {
                        session.update(cx, |s, cx| s.set_blocked(peer, false, cx))
                    }))
                })
                .when(locked, |d| {
                    let session = self.session.clone();
                    d.child(
                        button("unlock-here", tr!("Unlock", "Desbloquear"), Kind::Primary, t)
                            .on_click(move |_, window, cx| super::private::unlock(&session, window, cx)),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_header(&mut self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        if let Some((conversation, peer)) = self.dm(cx) {
            return self.render_dm_header(conversation, peer, t, cx);
        }
        let channel = self.room.channel().unwrap_or(-1);
        let s = self.session.read(cx);
        let ch = s.channel(channel).cloned();
        let pins = s.chats.get(&self.room).map(|l| l.pinned.len()).unwrap_or(0);
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(16.))
            .border_b_1()
            .border_color(t.stroke)
            .child(icon("hash", 18., t.text3))
            .child(title(ch.map(|c| c.name).unwrap_or_default(), t.text))
            .child(div().flex_1())
            .child(
                tool_button("pins", "pin", self.show_pins, if self.show_pins { t.accent } else { t.text2 }, t)
                    .tooltip(tip(trf!("Pinned messages ({})", "Mensagens fixadas ({})", pins), t))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_pins = !this.show_pins;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(px(240.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .h(px(34.))
                    .rounded(px(radius::CONTROL))
                    .bg(t.control)
                    .border_1()
                    .border_color(t.stroke)
                    .child(icon("search", 14., t.text3))
                    .child(div().flex_1().min_w(px(0.)).child(self.search.clone().into_any_element()).text_size(px(13.)))
                    .when(self.results.is_some(), |d| {
                        d.child(
                            icon_button("clear-search", "close", t)
                                .size(px(24.))
                                .tooltip(tip(tr!("Clear the search", "Limpar a busca"), t))
                                .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
                        )
                    }),
            )
    }

    fn render_pins(&mut self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.show_pins {
            return None;
        }
        let pinned = self.session.read(cx).chats.get(&self.room).map(|l| l.pinned.clone()).unwrap_or_default();
        let mut col = div().id("pinned").flex().flex_col().gap(px(2.)).max_h(px(220.)).overflow_y_scroll().p(px(6.));
        if pinned.is_empty() {
            col = col.child(div().p(px(10.)).child(caption(
                tr!(
                    "Nothing pinned yet. Hover a message and press the pin to keep it here.",
                    "Nada fixado ainda. Passe o mouse em uma mensagem e clique no alfinete para guardá-la aqui."
                ),
                t.text3,
            )));
        }
        for m in pinned {
            let who = self.session.read(cx).display_name(m.user_id, m.nickname.as_deref());
            let text = if m.body.trim().is_empty() {
                tr!("(attachment)", "(anexo)").to_string()
            } else {
                rich::plain(&m.body, self.session.read(cx))
            };
            let id = m.id;
            let hover = t.layer_hover;
            col = col.child(
                div()
                    .id(("pinned-row", m.id as u64))
                    .flex()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(radius::INNER))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(div().flex_none().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.text).child(who))
                    .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(13.)).text_color(t.text2).child(text))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to(id, cx))),
            );
        }
        Some(div().border_b_1().border_color(t.stroke).bg(t.layer).child(col).into_any_element())
    }

    fn render_composer(&mut self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let can_send = !self.composer.read(cx).is_blank() || self.pending.is_some();
        let mention_list = self.mention_matches(cx);
        let picked = self.mention.as_ref().map(|m| m.1).unwrap_or(0);
        let focused_hint = if self.uploading { Some(tr!("Uploading…", "Enviando…")) } else { None };
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(16.))
            .pb(px(14.))
            .pt(px(4.))
            .when(self.mention.is_some() && !mention_list.is_empty(), |d| {
                let mut menu = div()
                    .absolute()
                    .bottom(px(70.))
                    .left(px(16.))
                    .w(px(320.))
                    .p(px(5.))
                    .flex()
                    .flex_col()
                    .rounded(px(radius::CONTROL + 2.))
                    .bg(t.popover)
                    .border_1()
                    .border_color(t.stroke_strong)
                    .shadow_lg();
                for (i, (nick, shown, user)) in mention_list.into_iter().enumerate() {
                    let avatar_img = user.and_then(|u| self.session.update(cx, |s, cx| s.avatar(u, cx)));
                    menu = menu.child(
                        div()
                            .id(("mention", i))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .h(px(34.))
                            .px(px(8.))
                            .rounded(px(radius::INNER))
                            .cursor_pointer()
                            .when(i == picked, |d| d.bg(t.layer_hover))
                            .child(if user.is_some() { avatar(&nick, avatar_img, 22., None) } else { icon("users", 18., t.text2) })
                            .child(div().flex_1().truncate().text_size(px(13.5)).text_color(t.text).child(if user.is_some() {
                                shown.clone()
                            } else {
                                "@everyone".into()
                            }))
                            .child(div().text_size(px(12.)).text_color(t.text3).child(if user.is_some() {
                                format!("@{nick}")
                            } else {
                                shown
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| this.accept_mention(i, window, cx))),
                    );
                }
                d.child(menu)
            })
            .when_some(self.pending.as_ref(), |d, p| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .p(px(8.))
                        .rounded(px(radius::CONTROL))
                        .bg(t.layer)
                        .border_1()
                        .border_color(t.stroke)
                        .child(match &p.preview {
                            Some(img_) => div()
                                .size(px(44.))
                                .rounded(px(6.))
                                .overflow_hidden()
                                .child(img(img_.clone()).size_full().object_fit(gpui::ObjectFit::Cover)),
                            None => div().size(px(44.)).rounded(px(6.)).bg(t.control).flex().items_center().justify_center().child(icon(
                                "paperclip",
                                18.,
                                t.text2,
                            )),
                        })
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .child(div().truncate().text_size(px(13.5)).child(p.name.clone()))
                                .child(mono(human_size(p.bytes.len()), t.text3)),
                        )
                        .child(icon_button("remove-pending", "close", t).tooltip(tip(tr!("Remove", "Remover"), t)).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.pending = None;
                                cx.notify();
                            },
                        ))),
                )
            })
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(4.))
                    .p(px(5.))
                    .rounded(px(radius::CONTROL + 2.))
                    .bg(t.control)
                    .border_1()
                    .border_color(t.stroke)
                    .child(
                        icon_button("attach", "plus", t)
                            .tooltip(tip(tr!("Attach a file", "Anexar um arquivo"), t))
                            .on_click(cx.listener(|this, _, _, cx| this.attach(cx))),
                    )
                    .child(div().flex_1().min_w(px(0.)).py(px(6.)).child(self.composer.clone()))
                    .child(icon_button("emoji", "smile", t).tooltip(tip(tr!("Emoji", "Emoji"), t)).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.pick_emoji(e.position, window, cx)
                        }),
                    ))
                    .child(
                        div()
                            .id("send")
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(32.))
                            .rounded(px(8.))
                            .cursor_pointer()
                            .when(can_send, |d| d.bg(t.accent))
                            .child(icon("send", 16., if can_send { t.on_accent } else { t.text3 }))
                            .tooltip(tip(tr!("Send (Enter) · New line (Shift+Enter)", "Enviar (Enter) · Nova linha (Shift+Enter)"), t))
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    ),
            )
            .when_some(focused_hint, |d, h| d.child(caption(h, t.text3)))
    }
}

impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let log = self.session.read(cx).chats.get(&self.room);
        let (loaded, error) = (log.is_some_and(|l| l.loaded), log.and_then(|l| l.error.clone()));
        let empty = self.messages(cx).is_empty();
        let dm = self.dm(cx);
        let s = self.session.read(cx);
        let readable = match self.room {
            Room::Channel(id) => s.channel(id).is_some_and(|c| s.can_read(c)),
            Room::Dm(_) => dm.is_some(),
        };
        let writable = readable && self.blocked_reason(cx).is_none();
        let name = match dm {
            Some((_, peer)) => s.display_name(Some(peer), None),
            None => self.room.channel().and_then(|id| s.channel(id)).map(|c| c.name.clone()).unwrap_or_default(),
        };
        if self.lang != crate::i18n::lang() {
            self.lang = crate::i18n::lang();
            let hint = composer_hint(self.room, self.session.read(cx));
            self.composer.update(cx, |f, cx| f.set_placeholder(hint, cx));
            self.search.update(cx, |f, cx| f.set_placeholder(tr!("Search this channel", "Buscar neste canal"), cx));
        }
        let header = self.render_header(&t, cx);
        let pins = self.render_pins(&t, cx);
        let notice = dm.and_then(|(_, peer)| self.render_dm_notice(peer, &t, cx));
        let composer = self.render_composer(&t, cx);
        let drop_title = if dm.is_some() {
            trf!("Drop a file to send it to {}", "Solte um arquivo para enviá-lo para {}", name)
        } else {
            trf!("Drop a file to send it in #{}", "Solte um arquivo para enviá-lo em #{}", name)
        };
        let body: AnyElement = if dm.is_some() && loaded && empty {
            empty_state(
                "lock",
                &trf!("Your private conversation with {}", "Sua conversa privada com {}", name),
                tr!(
                    "Only the two of you can read what you write here, or hear your calls: it is sealed on your computers, and not even whoever runs the server can open it.",
                    "Só vocês dois conseguem ler o que escreverem aqui ou ouvir suas chamadas: tudo é protegido nos seus computadores, e nem quem administra o servidor consegue abrir."
                ),
                &t,
            )
            .into_any_element()
        } else if !readable {
            empty_state(
                "lock",
                tr!("This channel is locked", "Este canal tem senha"),
                tr!(
                    "Join the voice channel with the same password, or ask an admin.",
                    "Entre no canal de voz com a mesma senha ou peça a um admin."
                ),
                &t,
            )
            .into_any_element()
        } else if let Some(e) = error {
            empty_state("warning", tr!("Messages did not load", "As mensagens não carregaram"), &e, &t).into_any_element()
        } else if let Some((q, r)) = &self.results
            && r.is_empty()
        {
            empty_state(
                "search",
                tr!("No results", "Nenhum resultado"),
                &trf!("Nothing in #{} matches “{}”.", "Nenhuma mensagem em #{} com “{}”.", name, q),
                &t,
            )
            .into_any_element()
        } else if loaded && empty {
            empty_state(
                "hash",
                &trf!("Welcome to #{}", "Boas-vindas ao #{}", name),
                tr!("This is the start of the channel. Say something.", "Este é o começo do canal. Diga alguma coisa."),
                &t,
            )
            .into_any_element()
        } else {
            list(self.list.clone(), cx.processor(|this, ix, window, cx| this.render_message(ix, window, cx))).size_full().into_any_element()
        };
        div()
            .id("chat")
            .key_context("Chat")
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key))
            .group("chat-drop")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .when(writable, |d| {
                d.on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                    if let Some(path) = paths.paths().first() {
                        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
                        match std::fs::read(path) {
                            Ok(b) => this.set_pending(b, content_type_for(&name), name, cx),
                            Err(e) => overlay::toast(trf!("Could not read that file: {}", "Não foi possível ler esse arquivo: {}", e), cx),
                        }
                    }
                }))
            })
            .child(header)
            .children(pins)
            .when_some(self.results.as_ref(), |d, (q, r)| {
                d.child(div().px(px(16.)).py(px(6.)).border_b_1().border_color(t.stroke).child(caption(results_for(r.len(), q), t.text3)))
            })
            .children(notice)
            .child(div().flex_1().min_h(px(0.)).child(body))
            .when(writable, |d| d.child(composer))
            // Texel's drop target: dashed, inset, with one line saying what happens.
            .child(
                div()
                    .absolute()
                    .inset(px(10.))
                    .invisible()
                    .when(writable, |d| d.group_drag_over::<gpui::ExternalPaths>("chat-drop", |s| s.visible()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(radius::CARD))
                    .border_2()
                    .border_dashed()
                    .border_color(t.accent)
                    .bg(t.pane.opacity(0.88))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(8.))
                            .child(icon("paperclip", 26., t.accent))
                            .child(title(drop_title, t.text)),
                    ),
            )
    }
}

pub fn empty_state(glyph: &'static str, heading: &str, text: &str, t: &Theme) -> gpui::Div {
    div().size_full().flex().items_center().justify_center().p(px(24.)).child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.))
            .max_w(px(420.))
            .child(div().size(px(56.)).rounded_full().bg(t.control).flex().items_center().justify_center().child(icon(glyph, 24., t.text2)))
            .child(title(heading.to_string(), t.text))
            .child(div().text_center().child(body(text.to_string(), t.text2))),
    )
}

fn file_card(id: MessageId, name: &str, hash: &str, how: Fetch, t: &Theme, cx: &mut Context<ChatView>) -> AnyElement {
    let (h, n) = (hash.to_string(), name.to_string());
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .max_w(px(420.))
        .p(px(10.))
        .rounded(px(radius::CARD))
        .bg(t.layer)
        .border_1()
        .border_color(t.stroke)
        .child(div().size(px(36.)).rounded(px(8.)).bg(t.control).flex().items_center().justify_center().child(icon(
            "paperclip",
            16.,
            t.text2,
        )))
        .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(13.5)).child(name.to_string()))
        .child(
            icon_button(("save", id as u64), "download", t)
                .tooltip(tip(tr!("Save", "Salvar"), t))
                .on_click(cx.listener(move |this, _, _, cx| this.save_attachment(h.clone(), n.clone(), how.clone(), cx))),
        )
        .into_any_element()
}

fn composer_hint(room: Room, s: &Session) -> String {
    match room {
        Room::Channel(id) => trf!("Message #{}", "Mensagem em #{}", s.channel(id).map(|c| c.name.as_str()).unwrap_or("")),
        Room::Dm(id) => {
            let name = s.display_name(s.peer_of(id), None);
            trf!("Message {}", "Mensagem para {}", name)
        }
    }
}

/// A strip above a conversation with something to say, and buttons for what to do about it.
fn notice(glyph: &'static str, text: String, color: gpui::Hsla, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(16.))
        .py(px(10.))
        .border_b_1()
        .border_color(t.stroke)
        .bg(t.tint(color))
        .child(icon(glyph, 16., color))
        .child(div().flex_1().min_w(px(0.)).child(caption(text, t.text)))
}

/// Toasts what went wrong when a background send fails.
fn toast_failure(task: Task<Result<(), crate::core::api::ApiError>>, cx: &mut App) {
    cx.spawn(async move |cx| {
        if let Err(e) = task.await
            && !e.message.is_empty()
        {
            cx.update(|cx| overlay::toast(e.message, cx));
        }
    })
    .detach();
}

fn results_for(n: usize, q: &str) -> String {
    match n {
        1 => trf!("1 result for “{}”", "1 resultado para “{}”", q),
        _ => trf!("{} results for “{}”", "{} resultados para “{}”", n, q),
    }
}

fn reacted(who: &[String], emoji: &str) -> String {
    match who.len() {
        1 => trf!("{} reacted with {}", "{} reagiu com {}", who[0], emoji),
        _ => trf!("{} reacted with {}", "{} reagiram com {}", who.join(", "), emoji),
    }
}

fn human_size(n: usize) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} B"),
    }
}

/// The content type the server expects for a file name.
pub fn content_type_for(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "text/plain",
    }
    .into()
}

fn remember_emoji(emoji: &str, cx: &mut App) {
    let e = emoji.to_string();
    set_prefs(cx, |s| {
        s.recent_emoji.retain(|x| *x != e);
        s.recent_emoji.insert(0, e);
        s.recent_emoji.truncate(24);
    });
    let _ = prefs(cx);
}

/// A picture, big, with Save.
struct Lightbox {
    /// The thumbnail, shown until `full` has decoded.
    image: Arc<RenderImage>,
    full: Option<Arc<RenderImage>>,
    hash: String,
    name: String,
    how: Fetch,
    chat: gpui::WeakEntity<ChatView>,
    focus: FocusHandle,
    _load: Task<()>,
}

impl EventEmitter<Dismiss> for Lightbox {}

impl Focusable for Lightbox {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Lightbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = current();
        let size = window.viewport_size();
        dialog_card(&t, f32::from(size.width) * 0.8 / crate::theme::scale())
            .track_focus(&self.focus)
            .child(
                div().flex().justify_center().bg(t.stage).max_h(size.height * 0.75).child(
                    img(self.full.clone().unwrap_or_else(|| self.image.clone()))
                        .max_h(size.height * 0.75)
                        .max_w_full()
                        .object_fit(gpui::ObjectFit::Contain),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(10.))
                    .child(div().flex_1().truncate().child(body(self.name.clone(), t.text2)))
                    .child(button("lightbox-save", tr!("Save", "Salvar"), Kind::Standard, &t).on_click(cx.listener(|this, _, _, cx| {
                        let (h, n, how) = (this.hash.clone(), this.name.clone(), this.how.clone());
                        if let Some(chat) = this.chat.upgrade() {
                            chat.update(cx, |c, cx| c.save_attachment(h, n, how, cx));
                        }
                    })))
                    .child(
                        button("lightbox-close", tr!("Close", "Fechar"), Kind::Primary, &t)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismiss))),
                    ),
            )
    }
}
