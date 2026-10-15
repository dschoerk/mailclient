//! The folder sidebar and the message list of the selected folder.

use super::*;

/// Most messages shown in one folder list.
const LIST_LIMIT: usize = 5000;
/// Typing pause after which the search box also searches the servers.
const SERVER_SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(600);
/// Messages shown in a list row's mini timeline; longer conversations show "+N" before them.
const MAX_DOTS: usize = 12;

/// Account colors (RGB) for the bar in the message list, by the order accounts were added; Settings offers them too.
pub(super) const ACCOUNT_COLORS: [u32; 8] = [0x2f6fde, 0xe8730c, 0x2e9e5b, 0x8e44ad, 0xd64545, 0x1a9ba1, 0xd6457f, 0x9a6b3f];

pub(super) fn palette_color(index: usize) -> slint::Color {
    slint::Color::from_argb_encoded(0xff00_0000 | ACCOUNT_COLORS[index % ACCOUNT_COLORS.len()])
}

/// Color of account `index`: the one chosen in Settings, else one by its place. A single account
/// without a chosen color gets none (nothing to tell apart), unless `always`.
pub(super) fn account_color(accounts: &[Account], index: usize, always: bool) -> slint::Color {
    match accounts.get(index).and_then(|a| calendar::hex_color(&a.config.color)) {
        Some(chosen) => chosen,
        None if accounts.len() < 2 && !always => slint::Color::from_argb_encoded(0),
        None => palette_color(index),
    }
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
        let colors: Vec<slint::Color> = {
            let state = self.state.borrow();
            (0..count).map(|i| account_color(&state.accounts, i, false)).collect()
        };
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
                        color: colors[i],
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
                        color: colors[i],
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
        let index = state.current.as_ref().filter(|_| !state.everywhere).and_then(|c| state.rows.iter().position(|r| r == c));
        let window = self.window();
        window.set_current_folder(index.map(|i| i as i32).unwrap_or(-1));
        // The filter pills: "in" is the open folder, none while searching everywhere.
        let folder = match &state.current {
            _ if state.everywhere => String::new(),
            Some(Row::Unified) => "All inboxes".to_string(),
            Some(Row::Folder(a, f)) => state.folder(*a, f).map(|f| f.display_name()).unwrap_or_default(),
            _ => String::new(),
        };
        window.set_filter_in(folder.into());
        window.set_filter_from(state.search_from.clone().unwrap_or_default().into());
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
            state.search_from = None;
            state.everywhere = false;
            state.search_hits.clear();
        }
        self.window().set_search_text("".into());
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

    /// The search box changed. Finished `from:` and `in:` words (a space after them) become filter pills.
    pub(super) fn search(&self, text: &str) {
        let mut kept = String::new();
        let mut converted = false;
        for word in text.split_inclusive(char::is_whitespace) {
            let finished = word.ends_with(char::is_whitespace);
            if let Some(from) = filter_word(word.trim(), "from:").filter(|_| finished) {
                self.state.borrow_mut().search_from = Some(from.to_string());
                converted = true;
            } else if filter_word(word.trim(), "in:").filter(|_| finished).is_some_and(|name| self.open_folder_named(name)) {
                converted = true;
            } else {
                kept.push_str(word);
            }
        }
        if converted {
            self.window().set_search_text(kept.as_str().into());
        }
        let query = kept.trim().to_string();
        {
            let mut state = self.state.borrow_mut();
            if state.search == query && !converted {
                return;
            }
            state.search = query;
        }
        self.run_search();
    }

    /// `in:name`: opens the folder of that name, in the browsed account first. Case, spaces and
    /// dashes don't matter: `in:alle-nachrichten` finds "Alle Nachrichten".
    fn open_folder_named(&self, name: &str) -> bool {
        let found = {
            let state = self.state.borrow();
            let browsed = match &state.current {
                Some(Row::Folder(a, _)) => *a,
                _ => 0,
            };
            let mut accounts: Vec<usize> = (0..state.accounts.len()).collect();
            accounts.sort_by_key(|a| *a != browsed);
            accounts.into_iter().find_map(|a| {
                let folder = state.accounts[a].folders.iter().find(|f| [f.name.clone(), f.display_name()].iter().any(|n| loose(n) == loose(name)))?;
                Some((a, folder.name.clone()))
            })
        };
        let Some((account, folder)) = found else { return false };
        {
            let mut state = self.state.borrow_mut();
            state.current = Some(Row::Folder(account, folder.clone()));
            state.everywhere = false;
        }
        self.send(account, Command::OpenFolder(folder));
        true
    }

    /// Applies the search text and filters: the pills, the list, and (with something to match) the server.
    fn run_search(&self) {
        let (searching, query, scopes) = {
            let mut state = self.state.borrow_mut();
            state.search_hits.clear();
            // The server searches one folder, or (None) where most mail is: Gmail's All Mail.
            let scopes: Vec<(usize, Option<String>)> = (0..state.accounts.len())
                .filter_map(|a| state.search_scope(a).map(|folders| (a, if folders.len() == 1 { folders.into_iter().next() } else { None })))
                .collect();
            (state.searching(), state.search_query(), scopes)
        };
        if query.is_empty() {
            self.search_timer.stop();
        } else {
            self.search_timer.start(slint::TimerMode::SingleShot, SERVER_SEARCH_DELAY, move || {
                let (query, scopes) = (query.clone(), scopes.clone());
                with_controller(move |c| {
                    for (account, folder) in scopes {
                        c.send(account, Command::Search { query: query.clone(), folder });
                    }
                });
            });
        }
        self.window().set_searching(searching);
        self.update_folder_selection();
        self.reload_messages();
    }

    /// Back or Escape while searching: no text, no sender, the open folder again.
    pub(super) fn clear_search(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.search.clear();
            state.search_from = None;
            state.everywhere = false;
        }
        self.window().set_search_text("".into());
        self.run_search();
    }

    pub(super) fn remove_filter_from(&self) {
        self.state.borrow_mut().search_from = None;
        self.run_search();
    }

    /// Removing the "in" pill: search every folder of every account.
    pub(super) fn remove_filter_in(&self) {
        self.state.borrow_mut().everywhere = true;
        self.run_search();
    }

    /// Asks the engines for mail older than the list shows, when it is scrolled to its end.
    pub(super) fn load_older(&self) {
        let targets: Vec<(usize, String)> = {
            let mut state = self.state.borrow_mut();
            let Some(current) = state.current.clone().filter(|_| !state.searching()) else { return };
            let requested = Some((current.clone(), state.messages.len()));
            if state.older_requested == requested {
                return;
            }
            state.older_requested = requested;
            match current {
                Row::Folder(a, folder) => vec![(a, folder)],
                Row::Unified => state
                    .accounts
                    .iter()
                    .enumerate()
                    .filter_map(|(i, a)| a.folders.iter().find(|f| f.role == Role::Inbox).map(|f| (i, f.name.clone())))
                    .collect(),
                Row::Account(_) => Vec::new(),
            }
        };
        for (account, folder) in targets {
            self.send(account, Command::FetchOlder(folder));
        }
    }

    /// Lists the mail from `address` in the open folder (clicking a sender, Find similar).
    pub(super) fn show_mail_from(&self, address: &str) {
        if address.is_empty() {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.search_from = Some(address.to_string());
            state.search.clear();
        }
        self.window().set_search_text("".into());
        self.run_search();
    }

    pub(super) fn reload_messages(&self) {
        let rows = {
            let state = self.state.borrow();
            let limit = if state.searching() { 500 } else { LIST_LIMIT };
            let mut rows = Vec::new();
            for (i, account) in state.accounts.iter().enumerate() {
                let store = &account.store;
                let found = if state.searching() {
                    let Some(scope) = state.search_scope(i) else { continue };
                    let query = state.search_query();
                    let searched = if scope.is_empty() {
                        store.search(&query, None, limit)
                    } else {
                        scope.iter().map(|f| store.search(&query, Some(f), limit)).collect::<Result<Vec<_>>>().map(|found| found.concat())
                    };
                    searched.and_then(|mut found| {
                        let in_scope = |folder: &String| scope.is_empty() || scope.contains(folder);
                        for (_, folder, uid) in state.search_hits.iter().filter(|h| h.0 == i && in_scope(&h.1)) {
                            if !found.iter().any(|m| m.folder == *folder && m.uid == *uid)
                                && let Some(m) = store.message(folder, *uid)?
                            {
                                found.push(m);
                            }
                        }
                        found.sort_by_key(|m| Reverse(m.date));
                        Ok(found)
                    })
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
            // Rows moved or deleted a moment ago stay where they were, as strips with Undo.
            {
                let view = state.view();
                for undo in state.undos.iter().filter(|u| u.view == view) {
                    for (m, c) in &undo.rows {
                        // The strip stands for the conversation: other copies of it (Gmail's All Mail,
                        // in a search of all folders) are not listed beside it.
                        let ids: HashSet<&str> = c.iter().map(|m| m.message_id.as_str()).filter(|id| !id.is_empty()).collect();
                        rows.retain(|(a, _, conv)| *a != undo.account || !conv.iter().any(|x| ids.contains(x.message_id.as_str())));
                        rows.push((undo.account, m.clone(), c.clone()));
                    }
                }
            }
            // Each account lists newest first; a stable sort merges them and keeps that order.
            rows.sort_by_key(|(_, m, _)| Reverse(m.date));
            rows.truncate(limit);
            rows
        };
        let items: Vec<MessageItem> = {
            let state = self.state.borrow();
            // A list holding several accounts' mail names each row's account.
            let mixed = state.accounts.len() > 1 && (state.everywhere || matches!(state.current, Some(Row::Unified)));
            let show_recipients = |account: usize, folder: &str| {
                state.folder(account, folder).is_some_and(|f| matches!(f.role, Role::Sent | Role::Drafts))
            };
            rows.iter()
                .map(|(a, m, conversation)| (a, m, conversation, state.undo_of(*a, m).map(|i| &state.undos[i])))
                .map(|(a, m, conversation, undo)| MessageItem {
                    folder_label: if state.everywhere { folder_tag(&state, *a, m).into() } else { Default::default() },
                    removed: undo.map_or("", |u| u.kind).into(),
                    removed_text: undo.map(|u| u.text.clone()).unwrap_or_default().into(),
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
                    color: account_color(&state.accounts, *a, false),
                    account: if mixed { state.accounts[*a].config.email.clone().into() } else { Default::default() },
                    count: conversation.len() as i32,
                    checked: state.is_checked(*a, conversation),
                    fresh: conversation.iter().any(|c| state.fresh.contains(&(*a, c.folder.clone(), c.uid))),
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
            // Animated once: the next reload shows them as they are.
            state.fresh.clear();
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
        // Changes just queued or sent change the count of those waiting.
        self.show_problems();
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

/// Where a listed message is, for the "all folders" view. Gmail's All Mail copies by their labels
/// ("Inbox · Archiv"); one without any is archived and named after All Mail.
fn folder_tag(state: &State, account: usize, m: &MessageSummary) -> String {
    let Some(a) = state.accounts.get(account) else { return String::new() };
    let name = |folder: &str| state.folder(account, folder).map(|f| f.display_name()).unwrap_or_else(|| folder.to_string());
    let in_all_mail = state.folder(account, &m.folder).is_some_and(|f| f.role == Role::All);
    if !(a.config.is_gmail() && in_all_mail) {
        return name(&m.folder);
    }
    let of_role = |role: Role| a.folders.iter().find(|f| f.role == role).map(|f| f.display_name());
    let shown: Vec<String> = a
        .store
        .labels(&m.folder, m.uid)
        .unwrap_or_default()
        .iter()
        .filter_map(|l| match l.as_str() {
            "\\Inbox" => of_role(Role::Inbox),
            "\\Sent" => of_role(Role::Sent),
            "\\Draft" => of_role(Role::Drafts),
            // Important, Starred: marks, not places.
            l if l.starts_with('\\') => None,
            l => Some(name(l)),
        })
        .collect();
    if shown.is_empty() { name(&m.folder) } else { shown.join(" · ") }
}

/// A folder name for comparing what was typed: lower case, letters and digits only.
fn loose(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// `from:x` with key "from:" → "x" (key in any case); None for other words or an empty value.
fn filter_word<'a>(word: &'a str, key: &str) -> Option<&'a str> {
    word.get(..key.len()).filter(|p| p.eq_ignore_ascii_case(key)).map(|_| &word[key.len()..]).filter(|v| !v.is_empty())
}

#[cfg(test)]
#[test]
fn reads_filter_words() {
    assert_eq!(filter_word("FROM:a@b.org", "from:"), Some("a@b.org"));
    assert_eq!(filter_word("in:Archive", "in:"), Some("Archive"));
    assert_eq!(filter_word("from:", "from:"), None);
    assert_eq!(filter_word("frö", "from:"), None);
    assert_eq!(filter_word("inbox", "in:"), None);
    assert_eq!(loose("Alle Nachrichten"), loose("alle-nachrichten"));
    assert_ne!(loose("Archiv"), loose("Archive"));
}
