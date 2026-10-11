//! The folder sidebar and the message list of the selected folder.

use super::*;

/// Most messages shown in one folder list.
const LIST_LIMIT: usize = 5000;
/// Messages shown in a list row's mini timeline; longer conversations show "+N" before them.
const MAX_DOTS: usize = 12;

/// Account colors (RGB) for the bar in the message list, by the order accounts were added.
const ACCOUNT_COLORS: [u32; 8] = [0x2f6fde, 0xe8730c, 0x2e9e5b, 0x8e44ad, 0xd64545, 0x1a9ba1, 0xd6457f, 0x9a6b3f];

/// Color of account `index` out of `count`; none with a single account, where there is nothing to tell apart.
pub(super) fn account_color(index: usize, count: usize) -> slint::Color {
    if count < 2 {
        return slint::Color::from_argb_encoded(0);
    }
    slint::Color::from_argb_encoded(0xff00_0000 | ACCOUNT_COLORS[index % ACCOUNT_COLORS.len()])
}

impl Controller {
    // ---- folders and lists ----

    pub(super) fn reload_folders(&self) {
        let loaded: Result<Vec<Vec<Folder>>> = self.state.borrow().accounts.iter().map(|a| a.store.folders()).collect();
        let loaded = match loaded {
            Ok(f) => f,
            Err(e) => return self.show_error(&e),
        };
        let count = loaded.len();
        let mut rows = Vec::new();
        let mut items = Vec::new();
        if count > 1 {
            rows.push(Row::Unified);
            items.push(FolderItem {
                role: "unified".into(),
                label: "All inboxes".into(),
                unread: loaded.iter().flatten().filter(|f| f.role == Role::Inbox).map(|f| f.unread as i32).sum(),
                ..Default::default()
            });
        }
        let needs_initial_folder = {
            let mut state = self.state.borrow_mut();
            for (i, (account, folders)) in state.accounts.iter_mut().zip(loaded).enumerate() {
                if count > 1 {
                    rows.push(Row::Account(i));
                    items.push(FolderItem {
                        role: "account".into(),
                        label: account.config.email.clone().into(),
                        color: account_color(i, count),
                        ..Default::default()
                    });
                }
                for f in &folders {
                    rows.push(Row::Folder(i, f.name.clone()));
                    items.push(FolderItem {
                        name: f.name.clone().into(),
                        role: f.role.as_str().into(),
                        label: f.display_name().into(),
                        unread: if matches!(f.role, Role::Sent | Role::Drafts | Role::Trash | Role::Junk) {
                            0
                        } else {
                            f.unread as i32
                        },
                        depth: f.depth() as i32,
                        color: account_color(i, count),
                    });
                }
                account.folders = folders;
            }
            state.rows = rows;
            if state.current.as_ref().is_some_and(|c| !state.rows.contains(c)) {
                state.current = None;
            }
            state.current.is_none() && state.rows.iter().any(|r| matches!(r, Row::Unified | Row::Folder(..)))
        };
        self.folder_model.set_vec(items);
        if needs_initial_folder {
            let start = {
                let state = self.state.borrow();
                let inbox = |r: &Row| match r {
                    Row::Unified => true,
                    Row::Folder(a, f) => state.is_inbox(*a, f),
                    Row::Account(_) => false,
                };
                state
                    .rows
                    .iter()
                    .position(inbox)
                    .or_else(|| state.rows.iter().position(|r| matches!(r, Row::Folder(..))))
                    .unwrap_or(0)
            };
            self.select_folder(start);
        } else {
            self.update_folder_selection();
        }
    }

    pub(super) fn update_folder_selection(&self) {
        let state = self.state.borrow();
        let index = state.current.as_ref().and_then(|c| state.rows.iter().position(|r| r == c));
        let window = self.window();
        window.set_current_folder(index.map(|i| i as i32).unwrap_or(-1));
        if state.search.is_empty() {
            let title = match &state.current {
                Some(Row::Unified) => "All inboxes".to_string(),
                Some(Row::Folder(a, f)) => state.folder(*a, f).map(|f| f.display_name()).unwrap_or_default(),
                _ => String::new(),
            };
            window.set_list_title(title.into());
        }
    }

    pub(super) fn select_folder(&self, index: usize) {
        let Some(row) = self.state.borrow().rows.get(index).cloned() else { return };
        if matches!(row, Row::Account(_)) {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.current = Some(row.clone());
            state.search.clear();
        }
        self.window().set_searching(false);
        self.update_folder_selection();
        self.reload_messages();
        // Inboxes, all the unified view shows, are synced by every engine anyway.
        if let Row::Folder(account, name) = row {
            self.send(account, Command::OpenFolder(name));
        }
    }

    /// Account that new messages are sent from: the one being browsed, else the first.
    pub(super) fn compose_account(&self) -> usize {
        match &self.state.borrow().current {
            Some(Row::Folder(a, _)) => *a,
            _ => 0,
        }
    }

    pub(super) fn search(&self, text: &str) {
        self.state.borrow_mut().search = text.trim().to_string();
        let searching = !text.trim().is_empty();
        let window = self.window();
        window.set_searching(searching);
        if searching {
            window.set_list_title(format!("Search: {}", text.trim()).into());
        } else {
            self.update_folder_selection();
        }
        self.reload_messages();
    }

    pub(super) fn reload_messages(&self) {
        let rows = {
            let state = self.state.borrow();
            let limit = if state.search.is_empty() { LIST_LIMIT } else { 500 };
            let mut rows = Vec::new();
            for (i, account) in state.accounts.iter().enumerate() {
                let store = &account.store;
                let found = if !state.search.is_empty() {
                    store.search(&state.search, limit)
                } else {
                    match &state.current {
                        Some(Row::Folder(a, folder)) if *a == i => store.messages(folder, limit),
                        Some(Row::Unified) => match store.folder_by_role(Role::Inbox) {
                            Ok(Some(inbox)) => store.messages(&inbox, limit),
                            other => other.map(|_| Vec::new()),
                        },
                        _ => Ok(Vec::new()),
                    }
                };
                // shortcut: threads the whole cache on every reload; fine for a few thousand
                // cached messages, store conversation ids at sync time when caches grow larger.
                let all = match found {
                    Ok(listed) if listed.is_empty() => continue,
                    Ok(listed) => store.all_messages().map(|all| (listed, all)),
                    Err(e) => Err(e),
                };
                let (listed, all) = match all {
                    Ok(found) => found,
                    Err(e) => return self.show_error(&e),
                };
                let role = |folder: &str| state.folder(i, folder).map(|f| f.role).unwrap_or(Role::Other);
                rows.extend(conversation_rows(listed, &all, role).into_iter().map(|(m, c)| (i, m, c)));
            }
            // Each account lists newest first; a stable sort merges them and keeps that order.
            rows.sort_by_key(|(_, m, _)| Reverse(m.date));
            rows.truncate(limit);
            rows
        };
        let items: Vec<MessageItem> = {
            let state = self.state.borrow();
            let count = state.accounts.len();
            let show_recipients = |account: usize, folder: &str| {
                state.folder(account, folder).is_some_and(|f| matches!(f.role, Role::Sent | Role::Drafts))
            };
            rows.iter()
                .map(|(a, m, conversation)| MessageItem {
                    folder: m.folder.clone().into(),
                    uid: m.uid as i32,
                    sender: if show_recipients(*a, &m.folder) {
                        format!("To: {}", m.to)
                    } else if m.from_name.is_empty() {
                        m.from_addr.clone()
                    } else {
                        m.from_name.clone()
                    }
                    .into(),
                    subject: if m.subject.is_empty() { "(no subject)".into() } else { m.subject.clone().into() },
                    snippet: m.snippet.clone().into(),
                    date: format::list_date(m.date).into(),
                    seen: conversation.iter().all(|c| c.seen),
                    flagged: conversation.iter().any(|c| c.flagged),
                    answered: m.answered,
                    attachments: conversation.iter().any(|c| c.has_attachments),
                    color: account_color(*a, count),
                    count: conversation.len() as i32,
                    checked: state.is_checked(*a, conversation),
                    dots: if conversation.len() > 1 {
                        let newest = &conversation[conversation.len().saturating_sub(MAX_DOTS)..];
                        ModelRc::new(VecModel::from(newest.iter().map(|c| c.seen).collect::<Vec<_>>()))
                    } else {
                        ModelRc::default()
                    },
                })
                .collect()
        };
        // Replacing the rows would rebuild them and drop a press in progress (syncs reload often).
        if self.message_model.row_count() == items.len() {
            for (i, item) in items.into_iter().enumerate() {
                if self.message_model.row_data(i).as_ref() != Some(&item) {
                    self.message_model.set_row_data(i, item);
                }
            }
        } else {
            self.message_model.set_vec(items);
        }
        let selected_index = {
            let mut state = self.state.borrow_mut();
            (state.messages, state.conversations) = rows.into_iter().map(|(a, m, c)| ((a, m), c)).unzip();
            // Rows that left the list (another folder, moved, deleted) are no longer ticked.
            let listed: HashSet<Key> = state
                .messages
                .iter()
                .zip(&state.conversations)
                .flat_map(|((a, _), c)| c.iter().map(move |m| (*a, m.folder.clone(), m.uid)))
                .collect();
            state.checked.retain(|k| listed.contains(k));
            let index = state.selected_row();
            if index.is_none() && state.selected.is_some() {
                // The selected message is gone (moved elsewhere or deleted on another device).
                state.selected = None;
                state.body = None;
            }
            index
        };
        self.update_checked_summary();
        self.window().set_current_message(selected_index.map(|i| i as i32).unwrap_or(-1));
        if selected_index.is_none() {
            if self.window().get_has_message() {
                self.clear_reader();
            }
        } else {
            self.update_reader_header();
        }
    }
}


/// Groups `listed` (newest first) into list rows, one per conversation: its newest listed
/// message and the whole conversation from `all`, oldest first. Copies of a message (one sent to
/// oneself is also in Sent) appear once, and Trash/Junk only when the row is there itself.
fn conversation_rows(
    listed: Vec<MessageSummary>,
    all: &[MessageSummary],
    role: impl Fn(&str) -> Role,
) -> Vec<(MessageSummary, Vec<MessageSummary>)> {
    let numbers = mailcore::thread::conversations(all);
    let number: HashMap<(&str, u32), usize> =
        all.iter().zip(&numbers).map(|(m, n)| ((m.folder.as_str(), m.uid), *n)).collect();
    let mut members: HashMap<usize, Vec<&MessageSummary>> = HashMap::new();
    for (m, n) in all.iter().zip(&numbers) {
        members.entry(*n).or_default().push(m);
    }
    let mut done = HashSet::new();
    let mut rows = Vec::new();
    for m in listed {
        let n = number.get(&(m.folder.as_str(), m.uid)).copied();
        if n.is_some_and(|n| !done.insert(n)) {
            continue;
        }
        let mut conversation: Vec<MessageSummary> = match n.and_then(|n| members.get(&n)) {
            Some(members) => members
                .iter()
                .filter(|c| c.folder == m.folder || !matches!(role(&c.folder), Role::Trash | Role::Junk))
                .map(|c| (*c).clone())
                .collect(),
            None => vec![m.clone()],
        };
        // Keep the copy in the row's folder, where changes to it show.
        conversation.sort_by_key(|c| c.folder != m.folder);
        let mut ids = HashSet::new();
        conversation.retain(|c| c.message_id.is_empty() || ids.insert(c.message_id.clone()));
        conversation.sort_by_key(|c| c.date);
        rows.push((m, conversation));
    }
    rows
}

#[cfg(test)]
#[test]
fn groups_list_into_conversations() {
    let msg = |folder: &str, uid, id: &str, reply_to: &str, date| MessageSummary {
        folder: folder.into(),
        uid,
        message_id: id.into(),
        in_reply_to: reply_to.into(),
        date,
        ..Default::default()
    };
    let all = [
        msg("INBOX", 1, "<a>", "", 1),
        msg("Sent", 1, "<b>", "<a>", 2),
        msg("INBOX", 2, "<c>", "<b>", 3),
        msg("INBOX", 3, "<other>", "", 4),
        msg("Trash", 1, "<d>", "<c>", 5),
        // A message to oneself: in Sent and in the inbox.
        msg("Sent", 2, "<self>", "", 6),
        msg("INBOX", 4, "<self>", "", 6),
    ];
    let role = |f: &str| match f {
        "Trash" => Role::Trash,
        "Sent" => Role::Sent,
        _ => Role::Inbox,
    };
    let mut listed: Vec<MessageSummary> = all.iter().filter(|m| m.folder == "INBOX").cloned().collect();
    listed.sort_by_key(|m| Reverse(m.date));
    let rows = conversation_rows(listed, &all, role);
    let shape: Vec<(u32, Vec<(&str, u32)>)> =
        rows.iter().map(|(m, c)| (m.uid, c.iter().map(|c| (c.folder.as_str(), c.uid)).collect())).collect();
    assert_eq!(
        shape,
        [
            (4, vec![("INBOX", 4)]),
            (3, vec![("INBOX", 3)]),
            (2, vec![("INBOX", 1), ("Sent", 1), ("INBOX", 2)]),
        ]
    );
}
