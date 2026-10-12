//! Showing the selected message: header, body (in the webview or as text), links and attachments.

use super::*;

impl Controller {
    #[cfg(feature = "servo")]
    pub(super) fn init_webview(self: &Rc<Self>, window: &AppWindow) {
        use crate::webview::{MailView, delegate::Hooks};

        let weak = Rc::downgrade(self);
        let initialized = std::cell::Cell::new(false);
        let result = window.window().set_rendering_notifier(move |state, graphics_api| {
            if !matches!(state, slint::RenderingState::RenderingSetup) || initialized.get() {
                return;
            }
            let slint::GraphicsAPI::WGPU30 { device, queue, .. } = graphics_api else {
                log::error!("renderer is not wgpu 30; HTML view disabled");
                return;
            };
            let Some(controller) = weak.upgrade() else { return };
            let link_target = weak.clone();
            let status_target = weak.clone();
            let hooks = Hooks {
                on_link: Box::new(move |url| {
                    if let Some(c) = link_target.upgrade() {
                        c.open_link(url.as_str());
                    }
                }),
                on_status: Box::new(move |text| {
                    if let Some(c) = status_target.upgrade() {
                        let engine_status = c.state.borrow().engine_status.clone();
                        c.window().set_status(text.unwrap_or(engine_status).into());
                    }
                }),
            };
            let view = MailView::new(&controller.window(), device.clone(), queue.clone(), hooks);
            *controller.viewer.borrow_mut() = Some(view);
            initialized.set(true);
            // A message may have been selected before the view existed.
            controller.render_selected();
        });
        if let Err(e) = result {
            log::error!("cannot install rendering notifier: {e:?}");
        }
    }
    // ---- reading ----

    /// Shows a message file read-only, without connecting to any account.
    pub(super) fn open_file(&self, path: &std::path::Path) -> Result<()> {
        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let summary = mime::summary_from_headers("", 0, &raw, None);
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some((0, String::new(), 0));
            state.standalone = Some((summary, raw));
        }
        let window = self.window();
        window.set_list_title(path.file_name().unwrap_or_default().to_string_lossy().as_ref().into());
        window.set_account_label("Viewing file — no account".into());
        self.render_selected();
        Ok(())
    }

    pub(super) fn raw_of(&self, account: usize, m: &MessageSummary) -> Result<Option<Vec<u8>>> {
        let state = self.state.borrow();
        if let Some((_, raw)) = &state.standalone {
            return Ok(Some(raw.clone()));
        }
        match state.accounts.get(account) {
            Some(a) => a.store.raw(&m.folder, m.uid),
            None => Ok(None),
        }
    }

    /// The selected message and its account.
    pub(super) fn selected_summary(&self) -> Option<(usize, MessageSummary)> {
        let state = self.state.borrow();
        if let Some((summary, _)) = &state.standalone {
            return Some((0, summary.clone()));
        }
        let (account, folder, uid) = state.selected.as_ref()?;
        let row = state.selected_row()?;
        let m = state.conversations[row].iter().find(|m| &m.folder == folder && m.uid == *uid)?;
        Some((*account, m.clone()))
    }

    pub(super) fn select_message(&self, index: usize) {
        // Strips are skipped, onward in the direction the selection moves (j/k).
        let index = {
            let state = self.state.borrow();
            let back = (index as i32) < self.window().get_current_message();
            let mut i = index;
            while state.is_strip(i) {
                match if back { i.checked_sub(1) } else { Some(i + 1).filter(|&n| n < state.messages.len()) } {
                    Some(n) => i = n,
                    None => return,
                }
            }
            i
        };
        let Some((account, summary)) = self.state.borrow().messages.get(index).cloned() else { return };
        let key = (account, summary.folder.clone(), summary.uid);
        if self.state.borrow().selected.as_ref() == Some(&key) {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some(key);
            state.body = None;
        }
        self.window().set_current_message(index as i32);
        // Opening a conversation reads all of it.
        let unread: Vec<MessageSummary> =
            self.state.borrow().conversations[index].iter().filter(|m| !m.seen).cloned().collect();
        if !unread.is_empty() {
            self.apply(account, &unread, PendingOp::SetSeen(true));
        }
        self.render_selected();
    }

    /// Shows another message of the open conversation.
    pub(super) fn select_in_conversation(&self, index: usize) {
        let found = {
            let state = self.state.borrow();
            state.selected_row().and_then(|row| Some((state.messages[row].0, state.conversations[row].get(index)?.clone())))
        };
        let Some((account, m)) = found else { return };
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some((account, m.folder.clone(), m.uid));
            state.body = None;
        }
        if !m.seen {
            self.apply(account, std::slice::from_ref(&m), PendingOp::SetSeen(true));
        }
        self.render_selected();
    }

    pub(super) fn close_message(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.selected = None;
            state.body = None;
        }
        self.window().set_current_message(-1);
        self.window().set_source_open(false);
        self.clear_reader();
    }

    pub(super) fn clear_reader(&self) {
        let window = self.window();
        window.set_has_message(false);
        window.set_invite_kind("".into());
        window.set_msg_loading(false);
        self.attachment_model.set_vec(Vec::new());
        self.conversation_model.set_vec(Vec::new());
        #[cfg(feature = "servo")]
        if let Some(view) = self.viewer.borrow().as_ref() {
            view.clear();
        }
        #[cfg(target_os = "android")]
        crate::platform::html::clear();
    }

    pub(super) fn update_reader_header(&self) {
        let Some((account, m)) = self.selected_summary() else { return };
        let window = self.window();
        window.set_has_message(true);
        window.set_msg_subject(if m.subject.is_empty() { "(no subject)".into() } else { m.subject.clone().into() });
        window.set_msg_from(format::person(&m.from_name, &m.from_addr).into());
        window.set_msg_to(m.to.clone().into());
        window.set_msg_cc(m.cc.clone().into());
        window.set_msg_date(format::full_date(m.date).into());
        window.set_msg_seen(m.seen);
        window.set_msg_flagged(m.flagged);
        let state = self.state.borrow();
        window.set_msg_is_draft(state.folder(account, &m.folder).is_some_and(|f| f.role == Role::Drafts));
        let conversation = state.selected_row().map(|row| state.conversations[row].as_slice()).unwrap_or_default();
        // A single message needs no conversation strip.
        let items: Vec<ConversationItem> = if conversation.len() < 2 {
            Vec::new()
        } else {
            conversation
                .iter()
                .map(|c| ConversationItem {
                    sender: if c.from_name.is_empty() { c.from_addr.clone() } else { c.from_name.clone() }.into(),
                    date: format::list_date(c.date).into(),
                    snippet: c.snippet.clone().into(),
                    seen: c.seen,
                })
                .collect()
        };
        let position = conversation.iter().position(|c| c.folder == m.folder && c.uid == m.uid);
        window.set_current_in_conversation(position.map(|i| i as i32).unwrap_or(-1));
        self.conversation_model.set_vec(items);
    }

    /// Shows the selected message with the rest of its conversation, downloading missing bodies.
    pub(super) fn render_selected(&self) {
        let Some((account, summary)) = self.selected_summary() else { return };
        self.update_reader_header();
        let window = self.window();
        // The new document starts at the open message; the timeline follows again once the user scrolls.
        window.set_reader_scrolled(false);
        window.set_scrolled_in_conversation(-1);
        let conversation = {
            let state = self.state.borrow();
            match state.selected_row() {
                Some(row) if state.standalone.is_none() => state.conversations[row].clone(),
                _ => vec![summary.clone()],
            }
        };
        let current = conversation.iter().position(|m| m.folder == summary.folder && m.uid == summary.uid).unwrap_or(0);
        let mut raws = Vec::new();
        for m in &conversation {
            match self.raw_of(account, m) {
                Ok(raw) => raws.push(raw),
                Err(e) => return self.show_error(&e),
            }
        }
        // An open source view follows the selection, so j/k step through sources.
        if window.get_source_open() {
            window.set_msg_source(raws[current].as_deref().map_or("Loading…".into(), source_text).into());
        }
        // Each arrives with BodyReady, which renders again.
        for (m, _) in conversation.iter().zip(&raws).filter(|(_, raw)| raw.is_none()) {
            if self.state.borrow_mut().fetching.insert((account, m.folder.clone(), m.uid)) {
                self.send(account, Command::FetchBody { folder: m.folder.clone(), uid: m.uid });
            }
        }
        if raws[current].is_none() {
            window.set_msg_loading(true);
            window.set_msg_unsubscribe("".into());
            window.set_invite_kind("".into());
            window.set_msg_text("".into());
            window.set_msg_parts(ModelRc::default());
            window.set_msg_remote_blocked(false);
            self.attachment_model.set_vec(Vec::new());
            #[cfg(feature = "servo")]
            if let Some(view) = self.viewer.borrow().as_ref() {
                view.clear();
            }
            #[cfg(target_os = "android")]
            crate::platform::html::clear();
            return;
        }
        window.set_msg_loading(false);
        let bodies: Vec<Option<Body>> = raws.iter().map(|raw| raw.as_deref().map(mime::body_from_raw)).collect();
        let body = bodies[current].clone().expect("checked above");
        let allow_remote = window.get_always_load_images() || {
            let state = self.state.borrow();
            conversation.iter().all(|m| state.remote_allowed.contains(&(account, m.folder.clone(), m.uid)))
        };
        let has_remote = bodies.iter().flatten().any(|b| b.has_remote_content);
        let html_view = cfg!(any(feature = "servo", target_os = "android"));
        window.set_msg_remote_blocked(has_remote && !allow_remote && html_view);
        window.set_msg_text(body.text.clone().into());
        window.set_msg_unsubscribe(match &body.unsubscribe {
            Some(mime::Unsubscribe::OneClick(_)) => "one-click",
            Some(mime::Unsubscribe::Mail(_)) => "mail",
            Some(mime::Unsubscribe::Web(_)) => "web",
            None => "",
        }.into());
        let headings: Vec<String> = conversation
            .iter()
            .map(|m| format!("{} — {}", format::person(&m.from_name, &m.from_addr), format::full_date(m.date)))
            .collect();
        // The text reader (builds without Servo) lists the whole conversation and scrolls to the open message.
        let parts: Vec<ReaderPart> = headings
            .iter()
            .zip(&bodies)
            .map(|(heading, b)| ReaderPart {
                heading: if conversation.len() > 1 { heading.into() } else { "".into() },
                text: b.as_ref().map_or("Loading…", |b| b.text.as_str()).into(),
            })
            .collect();
        window.set_msg_parts(ModelRc::new(VecModel::from(parts)));
        self.attachment_model.set_vec(
            body.attachments
                .iter()
                .map(|a| AttachmentItem {
                    index: a.index as i32,
                    name: a.name.clone().into(),
                    size: format::size(a.size).into(),
                })
                .collect::<Vec<_>>(),
        );
        #[cfg(any(feature = "servo", target_os = "android"))]
        {
            // Only the shown HTML gets the badges; the stored body stays clean for quoting.
            let marked: Vec<Option<Body>>;
            let (bodies, body) = if window.get_show_link_targets() {
                marked = bodies
                    .iter()
                    .map(|b| b.as_ref().map(|b| Body { html: b.html.as_deref().map(mime::mark_link_targets), ..b.clone() }))
                    .collect();
                (&marked, marked[current].as_ref().expect("checked above"))
            } else {
                (&bodies, &body)
            };
            let csp = mime::content_security_policy(allow_remote);
            // shortcut: one map for all messages; Content-IDs are meant to be globally unique.
            let inline_parts: HashMap<String, (String, Vec<u8>)> =
                bodies.iter().flatten().flat_map(|b| b.inline_parts.clone()).collect();
            #[cfg(feature = "servo")]
            if let Some(view) = self.viewer.borrow().as_ref() {
                let (document, anchor) = if conversation.len() == 1 {
                    (mime::render_document(body), String::new())
                } else {
                    let parts: Vec<(String, Option<&Body>)> =
                        headings.iter().cloned().zip(bodies.iter().map(Option::as_ref)).collect();
                    (mime::render_conversation(&parts, current), format!("m{current}"))
                };
                view.show(crate::webview::delegate::MessageContent { document, csp: csp.clone(), inline_parts: inline_parts.clone(), allow_remote, anchor });
            }
            // One WebView per message, stacked natively: Java then knows where each starts without scripts.
            #[cfg(target_os = "android")]
            {
                let documents: Vec<String> = bodies.iter().map(|b| b.as_ref().map(mime::render_document).unwrap_or_default()).collect();
                crate::platform::html::show(&crate::platform::html::Message {
                    documents: &documents,
                    headings: if conversation.len() > 1 { &headings } else { &[] },
                    current,
                    csp: &csp,
                    allow_remote,
                    inline_parts: &inline_parts,
                });
            }
        }
        self.show_invite(account, &body);
        self.state.borrow_mut().body = Some(body);
    }

    /// Allows remote images for the open conversation.
    pub(super) fn load_remote_images(&self) {
        let Some(key) = self.state.borrow().selected.clone() else { return };
        let keys: Vec<Key> = {
            let state = self.state.borrow();
            match state.selected_row() {
                Some(row) => state.conversations[row].iter().map(|m| (key.0, m.folder.clone(), m.uid)).collect(),
                None => vec![key],
            }
        };
        self.state.borrow_mut().remote_allowed.extend(keys);
        self.render_selected();
    }

    #[cfg_attr(not(any(feature = "servo", target_os = "android")), allow(dead_code))]
    pub(super) fn open_link(&self, url: &str) {
        if let Some(draft) = mailto_draft(url) {
            self.open_composer(self.compose_account(), draft, None);
        } else if (url.starts_with("https://") || url.starts_with("http://"))
            && let Err(e) = crate::platform::open_url(url)
        {
            self.show_error(&e);
        }
    }

    /// Leaves the open message's mailing list the way its headers offer: one click, a mail, or the list's page.
    pub(super) fn unsubscribe(&self) {
        let Some((account, m)) = self.selected_summary() else { return };
        let raw = match self.raw_of(account, &m) {
            Ok(Some(raw)) => raw,
            Ok(None) => return,
            Err(e) => return self.show_error(&e),
        };
        match mime::body_from_raw(&raw).unsubscribe {
            Some(mime::Unsubscribe::OneClick(url)) => {
                self.set_status("Unsubscribing…");
                std::thread::spawn(move || {
                    let result = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(anyhow::Error::from).and_then(|rt| {
                        rt.block_on(mailcore::http::request(
                            "POST",
                            &url,
                            &[("Content-Type", "application/x-www-form-urlencoded")],
                            b"List-Unsubscribe=One-Click",
                        ))
                    });
                    with_controller(move |c| match result {
                        // Some lists answer the POST with a redirect to a goodbye page: handled too.
                        Ok(r) if r.ok() || (300..400).contains(&r.status) => c.set_status("Unsubscribed"),
                        Ok(r) => c.set_status(format!("⚠ Unsubscribing failed: the list's server answered {}", r.status)),
                        Err(e) => c.show_error(&e.context("unsubscribing")),
                    });
                });
            }
            Some(mime::Unsubscribe::Mail(url)) => {
                let Some(mut draft) = mailto_draft(&url) else { return };
                let Some(from) = self.state.borrow().accounts.get(account).map(|a| a.config.email.clone()) else { return };
                if draft.subject.is_empty() {
                    draft.subject = "unsubscribe".into();
                }
                draft.message_id = mime::new_message_id(&from);
                let id = self.next_send_id.replace(self.next_send_id.get() + 1);
                self.send(account, Command::Send { id, draft, replied_to: None });
                self.set_status("Unsubscribe request sent");
            }
            Some(mime::Unsubscribe::Web(url)) => self.open_link(&url),
            None => {}
        }
    }

    /// Shows the open message's raw source in place of its body, or the body again.
    pub(super) fn toggle_source(&self) {
        let window = self.window();
        if window.get_source_open() || !window.get_has_message() {
            window.set_source_open(false);
            return;
        }
        window.set_source_open(true);
        self.render_selected();
    }

    /// Saves the open message as it came from the server, as an `.eml` file.
    pub(super) fn save_eml(&self) {
        let Some((account, summary)) = self.selected_summary() else { return };
        let raw = match self.raw_of(account, &summary) {
            Ok(Some(raw)) => raw,
            Ok(None) => return self.set_status("The message is still downloading"),
            Err(e) => return self.show_error(&e),
        };
        let name = mime::message_file_name(&summary.subject, "eml");
        self.save_in_background(name, "message/rfc822", async { Ok(raw) }, "saving message");
    }

    /// The open message as it goes on paper; `None` while its body is still downloading.
    fn printable(&self) -> Option<mailcore::pdf::Printable> {
        let (_, m) = self.selected_summary()?;
        let state = self.state.borrow();
        let body = state.body.as_ref()?;
        Some(mailcore::pdf::Printable {
            subject: m.subject.clone(),
            from: format::person(&m.from_name, &m.from_addr),
            to: m.to.clone(),
            cc: m.cc.clone(),
            date: format::full_date(m.date),
            text: body.text.clone(),
            attachments: body.attachments.iter().map(|a| format!("{}  {}", a.name, format::size(a.size))).collect(),
        })
    }

    /// Saves the open message as a PDF.
    pub(super) fn save_pdf(&self) {
        let Some(printable) = self.printable() else { return self.set_status("The message is still downloading") };
        let name = mime::message_file_name(&printable.subject, "pdf");
        self.save_in_background(name, "application/pdf", pdf_in_background(printable), "saving PDF");
    }

    /// Prints the open message. Android prints the HTML view as shown; elsewhere the PDF of
    /// [`Self::save_pdf`] goes to the system print dialog.
    pub(super) fn print(&self) {
        let Some(printable) = self.printable() else { return self.set_status("The message is still downloading") };
        let title = if printable.subject.trim().is_empty() { "Message".to_string() } else { printable.subject.clone() };
        #[cfg(target_os = "android")]
        {
            drop(printable);
            crate::platform::html::print(&title);
        }
        #[cfg(not(target_os = "android"))]
        {
            let weak = self.window.clone();
            self.set_status("Preparing to print…");
            let _ = slint::spawn_local(async move {
                let result = match pdf_in_background(printable).await {
                    Ok(pdf) => crate::platform::print_pdf(&title, pdf).await,
                    Err(e) => Err(e),
                };
                let status = match result {
                    Ok(status) => status.unwrap_or_default(),
                    Err(e) => format!("⚠ printing: {e:#}"),
                };
                if let Some(window) = weak.upgrade() {
                    window.set_status(status.into());
                }
            });
        }
    }

    pub(super) fn save_attachment(&self, index: usize) {
        let Some((account, summary)) = self.selected_summary() else { return };
        let Ok(Some(raw)) = self.raw_of(account, &summary) else { return };
        let Some((name, mime_type)) = self
            .state
            .borrow()
            .body
            .as_ref()
            .and_then(|b| b.attachments.iter().find(|a| a.index == index))
            .map(|a| (a.name.clone(), a.mime_type.clone()))
        else {
            return;
        };
        match mime::attachment_data(&raw, index) {
            Some(data) => self.save_in_background(name, &mime_type, async { Ok(data) }, "saving attachment"),
            None => self.set_status("⚠ attachment not found"),
        }
    }

    /// Asks where to save `data` once it is ready and reports the outcome in the status bar.
    fn save_in_background(
        &self,
        name: String,
        mime_type: &str,
        data: impl std::future::Future<Output = Result<Vec<u8>>> + 'static,
        what: &'static str,
    ) {
        let weak = self.window.clone();
        let mime_type = mime_type.to_string();
        let _ = slint::spawn_local(async move {
            let saved = match data.await {
                Ok(data) => crate::platform::save_file(&name, &mime_type, data).await,
                Err(e) => Err(e),
            };
            let result = match saved {
                Ok(None) => return,
                Ok(Some(saved)) => Ok(format!("Saved {saved}")),
                Err(e) => Err(anyhow!("{what}: {e:#}")),
            };
            if let Some(window) = weak.upgrade() {
                window.set_status(match result {
                    Ok(text) => text.into(),
                    Err(e) => format!("⚠ {e:#}").into(),
                });
            }
        });
    }
}

/// Larger sources are cut: a text view of megabytes of base64 helps nobody and is slow to lay out.
const SOURCE_LIMIT: usize = 512 * 1024;

fn source_text(raw: &[u8]) -> String {
    mime::source_text(raw, SOURCE_LIMIT)
}

/// The PDF of a message, made on another thread: finding the system font scans the font directories.
async fn pdf_in_background(printable: mailcore::pdf::Printable) -> Result<Vec<u8>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(mailcore::pdf::system_font().and_then(|font| mailcore::pdf::message_pdf(&printable, font)));
    });
    rx.await.map_err(|_| anyhow!("the PDF could not be made"))?
}

/// The message a `mailto:` URL describes; None for other URLs.
fn mailto_draft(url: &str) -> Option<Draft> {
    let mailto = url.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map(|_| &url[7..])?;
    let (to, query) = mailto.split_once('?').unwrap_or((mailto, ""));
    let mut draft = Draft { to: mime::percent_decode(to), ..Default::default() };
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let v = mime::percent_decode(&v.replace('+', " "));
            match k.to_ascii_lowercase().as_str() {
                "subject" => draft.subject = v,
                "cc" => draft.cc = v,
                "body" => draft.body = v,
                _ => {}
            }
        }
    }
    Some(draft)
}
