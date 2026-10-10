//! Changing messages (flags, archive, delete) and starting replies in the composer.

use super::*;

impl Controller {
    // ---- changing messages ----

    /// Applies a change locally right away and queues it for the server.
    pub(super) fn apply(&self, account: usize, messages: &[MessageSummary], op: PendingOp) {
        let result = match self.state.borrow_mut().accounts.get_mut(account) {
            Some(a) => messages.iter().try_for_each(|m| a.store.apply_local(&m.folder, m.uid, &op)),
            None => return,
        };
        if let Err(e) = result {
            return self.show_error(&e);
        }
        self.reload_messages();
        self.reload_folders();
        self.send(account, Command::Flush);
    }

    pub(super) fn toggle_selected(&self, flag: bool) {
        let Some((account, m)) = self.selected_summary() else { return };
        let op = if flag { PendingOp::SetFlagged(!m.flagged) } else { PendingOp::SetSeen(!m.seen) };
        self.apply(account, &[m], op);
    }

    /// Archives or deletes the open conversation: its messages in the folder of its list row.
    pub(super) fn remove_selected(&self, archive: bool) {
        let Some((account, m)) = self.selected_summary() else { return };
        let index = self.window().get_current_message().max(0) as usize;
        let (folder, targets) = {
            let state = self.state.borrow();
            match (state.messages.get(index), state.conversations.get(index)) {
                (Some((_, row)), Some(conversation)) => {
                    (row.folder.clone(), conversation.iter().filter(|c| c.folder == row.folder).cloned().collect())
                }
                _ => (m.folder.clone(), vec![m]),
            }
        };
        let op = {
            let state = self.state.borrow();
            let Some(store) = state.accounts.get(account).map(|a| &a.store) else { return };
            if archive { engine::archive_op(store, &folder) } else { engine::delete_op(store, &folder) }
        };
        let op = match op {
            Ok(op) => op,
            Err(e) => return self.show_error(&e),
        };
        // Select the message that takes this one's place, like most mail clients.
        self.apply(account, &targets, op);
        let count = self.state.borrow().messages.len();
        if count > 0 {
            self.select_message(index.min(count - 1));
        }
    }

    // ---- composing ----

    pub(super) fn reply(&self, kind: ReplyKind) {
        let Some((account, m)) = self.selected_summary() else { return };
        let me = self.state.borrow().accounts.get(account).map(|a| a.config.email.clone()).unwrap_or_default();
        let text = self.state.borrow().body.as_ref().map(|b| b.text.clone()).unwrap_or_default();
        let replied_to = (!matches!(kind, ReplyKind::Forward)).then(|| (m.folder.clone(), m.uid));
        let forwarding = matches!(kind, ReplyKind::Forward);
        let mut draft = mime::reply_draft(kind, &m, &text, &me);
        if forwarding {
            // Forward attachments too: write them to a temp dir and attach from there.
            if let Err(e) = self.attach_original_files(account, &m, &mut draft) {
                self.show_error(&e);
            }
        }
        self.open_composer(account, draft, replied_to);
    }

    pub(super) fn attach_original_files(&self, account: usize, m: &MessageSummary, draft: &mut Draft) -> Result<()> {
        let Some(attachments) = self.state.borrow().body.as_ref().map(|b| b.attachments.clone()) else { return Ok(()) };
        if attachments.is_empty() {
            return Ok(());
        }
        let raw = self.raw_of(account, m)?.ok_or_else(|| anyhow!("message not downloaded"))?;
        let dir = std::env::temp_dir().join(format!("mailclient-fwd-{}-{}", std::process::id(), m.uid));
        std::fs::create_dir_all(&dir)?;
        for att in &attachments {
            let data = mime::attachment_data(&raw, att.index).ok_or_else(|| anyhow!("attachment {} missing", att.name))?;
            let safe_name: String = att.name.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
            let path = dir.join(safe_name);
            std::fs::write(&path, data)?;
            draft.attachments.push(path);
        }
        Ok(())
    }

    /// Opens the selected message from Drafts in the composer.
    pub(super) fn edit_draft(&self) {
        let Some((account, m)) = self.selected_summary() else { return };
        let raw = match self.raw_of(account, &m) {
            Ok(Some(raw)) => raw,
            Ok(None) => return self.set_status("The draft is still downloading"),
            Err(e) => return self.show_error(&e),
        };
        let mut draft = mime::draft_from_raw(&raw);
        if let Err(e) = self.attach_original_files(account, &m, &mut draft) {
            return self.show_error(&e);
        }
        // shortcut: a reopened reply no longer marks its original answered; would need the original's UID saved with the draft.
        self.open_composer(account, draft, None);
    }

    /// `account`: who the message is sent from (and whose message `replied_to` is).
    pub(super) fn open_composer(&self, account: usize, mut draft: Draft, replied_to: Option<(String, u32)>) {
        let Some(email) = self.state.borrow().accounts.get(account).map(|a| a.config.email.clone()) else { return };
        if draft.message_id.is_empty() {
            draft.message_id = mime::new_message_id(&email);
        }
        if self.composer.borrow().is_some() {
            // One draft at a time; never throw away what the user is writing.
            return self.set_status("Send or discard the message you are writing first");
        }
        let weak_self = CONTROLLER.with(|c| c.borrow().as_ref().map(Rc::downgrade));
        let Some(weak_self) = weak_self else { return };
        let on_send: Box<dyn Fn(Draft, Option<(String, u32)>) -> u64> = {
            let weak = weak_self.clone();
            Box::new(move |draft, replied_to| {
                let Some(c) = weak.upgrade() else { return 0 };
                let id = c.next_send_id.get();
                c.next_send_id.set(id + 1);
                let from = usize::try_from(c.window().get_compose_from_index()).unwrap_or(account);
                // The replied-to message lives in `account`; another sender cannot flag it as answered.
                let replied_to = replied_to.filter(|_| from == account);
                c.send(from, Command::Send { id, draft, replied_to });
                id
            })
        };
        let on_save: Box<dyn Fn(Draft) -> u64> = {
            let weak = weak_self.clone();
            Box::new(move |draft| {
                let Some(c) = weak.upgrade() else { return 0 };
                let id = c.next_send_id.get();
                c.next_send_id.set(id + 1);
                // shortcut: changing From after a save leaves the older copy in the first account's Drafts.
                let from = usize::try_from(c.window().get_compose_from_index()).unwrap_or(account);
                c.send(from, Command::SaveDraft { id, draft });
                id
            })
        };
        let on_close: Box<dyn Fn(&Composer, bool)> = Box::new(move |composer, discarded| {
            if let Some(c) = weak_self.upgrade() {
                if discarded {
                    let from = usize::try_from(c.window().get_compose_from_index()).unwrap_or(account);
                    c.send(from, Command::DeleteDraft { message_id: composer.message_id().to_string() });
                }
                c.composer.borrow_mut().take();
            }
        });
        let senders: Vec<SharedString> = self
            .state
            .borrow()
            .accounts
            .iter()
            .map(|a| format::person(&a.config.name, &a.config.email).into())
            .collect();
        self.window().set_compose_from_options(ModelRc::from(senders.as_slice()));
        self.window().set_compose_from_index(account as i32);
        let composer = Composer::open(&self.window(), draft, replied_to, on_send, on_save, on_close);
        *self.composer.borrow_mut() = Some(composer);
    }
}
