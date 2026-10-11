//! Changing messages (flags, archive, delete) and starting replies in the composer.

use super::*;

/// How long Undo is offered after a move or delete; the server hears of it afterwards.
const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

impl Controller {
    // ---- changing messages ----

    /// Applies a change locally right away and queues it for the server. Moves and deletes are
    /// held back for [`UNDO_WINDOW`] while Undo is offered.
    pub(super) fn apply(&self, account: usize, messages: &[MessageSummary], op: PendingOp) {
        let held = matches!(op, PendingOp::Move(_) | PendingOp::Expunge);
        // Whole seconds round down: one more keeps the change back for the full window.
        let ready_at = if held { mailcore::store::now() + UNDO_WINDOW.as_secs() as i64 + 1 } else { 0 };
        let result = match self.state.borrow_mut().accounts.get_mut(account) {
            Some(a) => {
                let gmail = a.config.is_gmail();
                messages
                    .iter()
                    .map(|m| {
                        // Gmail's All Mail: a move is a change of labels.
                        let op = match &op {
                            PendingOp::Move(dest) => engine::move_op(&a.store, gmail, m, dest)?,
                            other => other.clone(),
                        };
                        a.store.apply_local_until(&m.folder, m.uid, &op, ready_at)
                    })
                    .collect::<Result<Vec<_>, _>>()
            }
            None => return,
        };
        let ids = match result {
            Ok(ids) => ids,
            Err(e) => return self.show_error(&e),
        };
        if !held {
            self.reload_messages();
            self.reload_folders();
            return self.send(account, Command::Flush);
        }
        let (kind, text) = match &op {
            PendingOp::Expunge => ("delete", "Deleted forever".to_string()),
            PendingOp::Move(dest) => match self.state.borrow().accounts[account].store.folder_role(dest) {
                Ok(Role::Trash) => ("delete", "Deleted".to_string()),
                Ok(Role::Archive | Role::All) => ("archive", "Archived".to_string()),
                _ => {
                    let name = self.state.borrow().folder(account, dest).map(|f| f.display_name()).unwrap_or_else(|| dest.clone());
                    ("move", format!("Moved to {name}"))
                }
            },
            _ => unreachable!(),
        };
        {
            let mut state = self.state.borrow_mut();
            // The list rows the change empties stay in place as strips with Undo.
            let rows = state
                .messages
                .iter()
                .zip(&state.conversations)
                .filter(|((a, _), c)| *a == account && c.iter().any(|c| messages.iter().any(|m| m.folder == c.folder && m.uid == c.uid)))
                .map(|((_, m), c)| (m.clone(), c.clone()))
                .collect();
            let view = state.view();
            state.undos.push(Undo { account, ids, kind, text, view, rows, expires: std::time::Instant::now() + UNDO_WINDOW });
        }
        self.reload_messages();
        self.reload_folders();
        self.arm_undo_timer();
    }

    /// Ends the next Undo offer when its time is up: its strips go, and its changes are sent.
    fn arm_undo_timer(&self) {
        let Some(next) = self.state.borrow().undos.iter().map(|u| u.expires).min() else { return };
        // A second extra: the change's ready time is in whole seconds (see `apply`).
        let wait = next.saturating_duration_since(std::time::Instant::now()) + std::time::Duration::from_secs(1);
        self.undo_timer.start(slint::TimerMode::SingleShot, wait, || {
            with_controller(|c| {
                let now = std::time::Instant::now();
                let expired: Vec<usize> = {
                    let mut state = c.state.borrow_mut();
                    let expired = state.undos.iter().filter(|u| u.expires <= now).map(|u| u.account).collect();
                    state.undos.retain(|u| u.expires > now);
                    expired
                };
                c.reload_messages();
                for account in expired {
                    c.send(account, Command::Flush);
                }
                c.arm_undo_timer();
            });
        });
    }

    /// Takes back the last move or delete (`z`).
    pub(super) fn undo(&self) {
        let last = self.state.borrow().undos.len().checked_sub(1);
        match last {
            Some(i) => self.undo_at(i),
            None => self.set_status("Nothing to undo"),
        }
    }

    /// The Undo on a strip in the list.
    pub(super) fn undo_row(&self, index: usize) {
        let found = {
            let state = self.state.borrow();
            state.messages.get(index).and_then(|(a, m)| state.undo_of(*a, m))
        };
        if let Some(i) = found {
            self.undo_at(i);
        }
    }

    fn undo_at(&self, i: usize) {
        let undo = self.state.borrow_mut().undos.remove(i);
        let result = self.state.borrow().accounts.get(undo.account).map(|a| a.store.undo(&undo.ids));
        self.reload_messages();
        self.reload_folders();
        match result {
            Some(Ok(0)) => self.set_status("Too late to undo: already on the server"),
            Some(Ok(_)) => self.set_status("Undone"),
            Some(Err(e)) => self.show_error(&e),
            None => {}
        }
    }

    /// A row dragged far: offers the folders to move its conversation to.
    pub(super) fn row_more(&self, index: usize) {
        let group = {
            let state = self.state.borrow();
            let (Some((account, row)), Some(c)) = (state.messages.get(index), state.conversations.get(index)) else { return };
            (*account, c.iter().filter(|m| m.folder == row.folder).cloned().collect())
        };
        self.offer_folders(vec![group]);
    }

    /// Opens "Move to" for messages of one or more accounts: the folders all of these accounts
    /// have (by name), except one every message is in already.
    fn offer_folders(&self, groups: Vec<(usize, Vec<MessageSummary>)>) {
        let names: Vec<String> = {
            let state = self.state.borrow();
            let Some(folders) = groups.first().and_then(|(a, _)| state.accounts.get(*a)).map(|a| &a.folders) else { return };
            folders
                .iter()
                .map(|f| f.name.clone())
                .filter(|n| groups.iter().all(|(a, _)| state.accounts[*a].folders.iter().any(|f| &f.name == n)))
                // Gmail's All Mail stays on offer: moving there archives (takes the labels off).
                .filter(|n| {
                    let all_mail = groups.iter().all(|(a, _)| {
                        let account = &state.accounts[*a];
                        account.config.is_gmail() && account.folders.iter().any(|f| &f.name == n && f.role == Role::All)
                    });
                    all_mail || !groups.iter().all(|(_, ms)| ms.iter().all(|m| &m.folder == n))
                })
                .collect()
        };
        // Shown by their readable names ("Entwürfe", not "[Gmail]/Entw&APw-rfe").
        let shown: Vec<SharedString> = {
            let state = self.state.borrow();
            let account = groups[0].0;
            names.iter().map(|n| state.folder(account, n).map(|f| f.display_name()).unwrap_or_else(|| n.clone()).into()).collect()
        };
        let window = self.window();
        window.set_move_targets(ModelRc::new(VecModel::from(shown)));
        window.set_move_open(true);
        self.state.borrow_mut().moving = Some((groups, names));
    }

    pub(super) fn move_chosen(&self, index: usize) {
        let Some((groups, names)) = self.state.borrow_mut().moving.take() else { return };
        let Some(dest) = names.get(index) else { return };
        // Moved rows are no longer ticked; other ticks (a row dragged while some were ticked) stay.
        self.state.borrow_mut().checked.retain(|(a, folder, uid)| {
            !groups.iter().any(|(account, ms)| account == a && ms.iter().any(|m| &m.folder == folder && m.uid == *uid))
        });
        for (account, messages) in groups {
            self.apply(account, &messages, PendingOp::Move(dest.clone()));
        }
        self.update_checked_summary();
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
        self.apply(account, &targets, op);
        // Select the message that takes this one's place, like most mail clients; its own row is now a strip.
        let next = {
            let state = self.state.borrow();
            (index..state.messages.len()).chain((0..index).rev()).find(|&i| !state.is_strip(i))
        };
        if let Some(next) = next {
            self.select_message(next);
        }
    }

    // ---- acting on several conversations ----

    /// The ticked rows: account, newest listed message, conversation.
    fn checked_rows(&self) -> Vec<(usize, MessageSummary, Vec<MessageSummary>)> {
        let state = self.state.borrow();
        state
            .messages
            .iter()
            .zip(&state.conversations)
            .filter(|((a, _), c)| state.is_checked(*a, c))
            .map(|((a, m), c)| (*a, m.clone(), c.clone()))
            .collect()
    }

    pub(super) fn update_checked_summary(&self) {
        let rows = self.checked_rows();
        let window = self.window();
        window.set_checked_count(rows.len() as i32);
        window.set_checked_all_seen(rows.iter().all(|(_, _, c)| c.iter().all(|m| m.seen)));
        window.set_checked_all_flagged(rows.iter().all(|(_, _, c)| c.iter().any(|m| m.flagged)));
    }

    pub(super) fn toggle_checked(&self, index: usize) {
        {
            let mut state = self.state.borrow_mut();
            let Some((account, m)) = state.messages.get(index).cloned() else { return };
            let conversation = state.conversations[index].clone();
            if state.is_checked(account, &conversation) {
                for c in &conversation {
                    state.checked.remove(&(account, c.folder.clone(), c.uid));
                }
            } else {
                state.checked.insert((account, m.folder, m.uid));
            }
        }
        self.reload_messages();
    }

    /// `action`: "seen" and "flag" toggle (set unless all ticked rows have it), "archive",
    /// "delete", "clear". The ticking ends with the action.
    pub(super) fn bulk(&self, action: &str) {
        let rows = self.checked_rows();
        if action == "move" {
            // The ticks stay until a folder is picked: closing the list keeps them.
            let mut groups: Vec<(usize, Vec<MessageSummary>)> = Vec::new();
            for (account, row, conversation) in rows {
                let messages = conversation.into_iter().filter(|m| m.folder == row.folder);
                match groups.iter_mut().find(|(a, _)| *a == account) {
                    Some((_, list)) => list.extend(messages),
                    None => groups.push((account, messages.collect())),
                }
            }
            return self.offer_folders(groups);
        }
        self.state.borrow_mut().checked.clear();
        self.act_on(rows, action);
    }

    /// A row swiped away: archived or deleted like a ticked row, other ticks left alone.
    pub(super) fn swiped(&self, index: usize, archive: bool) {
        let row = {
            let state = self.state.borrow();
            let (Some((account, m)), Some(c)) = (state.messages.get(index), state.conversations.get(index)) else { return };
            (*account, m.clone(), c.clone())
        };
        self.act_on(vec![row], if archive { "archive" } else { "delete" });
    }

    fn act_on(&self, rows: Vec<(usize, MessageSummary, Vec<MessageSummary>)>, action: &str) {
        let all_seen = rows.iter().all(|(_, _, c)| c.iter().all(|m| m.seen));
        let all_flagged = rows.iter().all(|(_, _, c)| c.iter().any(|m| m.flagged));
        // Batched per account and change, so the list reloads once per batch.
        let mut batches: Vec<(usize, PendingOp, Vec<MessageSummary>)> = Vec::new();
        for (account, row, conversation) in rows {
            let (op, targets) = match action {
                // Like opening: the whole conversation.
                "seen" => (PendingOp::SetSeen(!all_seen), conversation),
                "flag" if all_flagged => (PendingOp::SetFlagged(false), conversation.into_iter().filter(|m| m.flagged).collect()),
                "flag" => (PendingOp::SetFlagged(true), vec![row]),
                // Like the reader's Archive and Delete: the conversation's messages in the listed folder.
                "archive" | "delete" => {
                    let op = {
                        let state = self.state.borrow();
                        let Some(store) = state.accounts.get(account).map(|a| &a.store) else { continue };
                        if action == "archive" { engine::archive_op(store, &row.folder) } else { engine::delete_op(store, &row.folder) }
                    };
                    match op {
                        Ok(op) => (op, conversation.into_iter().filter(|m| m.folder == row.folder).collect()),
                        Err(e) => {
                            self.show_error(&e);
                            continue;
                        }
                    }
                }
                _ => continue,
            };
            match batches.iter_mut().find(|(a, o, _)| *a == account && *o == op) {
                Some((_, _, list)) => list.extend(targets),
                None => batches.push((account, op, targets)),
            }
        }
        for (account, op, targets) in batches {
            self.apply(account, &targets, op);
        }
        self.reload_messages();
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
