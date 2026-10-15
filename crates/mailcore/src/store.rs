//! Local SQLite cache of folders, message headers and bodies, plus the queue of
//! changes that still have to be applied on the server.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::ical::{Attendee, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
    Flagged,
    Other,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Inbox => "inbox",
            Role::Sent => "sent",
            Role::Drafts => "drafts",
            Role::Trash => "trash",
            Role::Junk => "junk",
            Role::Archive => "archive",
            Role::All => "all",
            Role::Flagged => "flagged",
            Role::Other => "",
        }
    }

    fn parse(s: &str) -> Role {
        match s {
            "inbox" => Role::Inbox,
            "sent" => Role::Sent,
            "drafts" => Role::Drafts,
            "trash" => Role::Trash,
            "junk" => Role::Junk,
            "archive" => Role::Archive,
            "all" => Role::All,
            "flagged" => Role::Flagged,
            _ => Role::Other,
        }
    }

    /// Display order in the folder list: well-known folders first.
    fn rank(self) -> i64 {
        match self {
            Role::Inbox => 0,
            Role::Flagged => 1,
            Role::Drafts => 2,
            Role::Sent => 3,
            Role::Archive => 4,
            Role::All => 5,
            Role::Junk => 6,
            Role::Trash => 7,
            Role::Other => 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Folder {
    pub name: String,
    pub delimiter: Option<String>,
    pub role: Role,
    pub unread: i64,
    pub total: i64,
}

/// IMAP's modified UTF-7 mailbox names (RFC 3501 5.1.3), e.g. "Entw&APw-rfe" -> "Entwürfe".
/// Malformed runs are kept as sent.
fn decode_mutf7(name: &str) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut rest = name;
    while let Some((plain, tail)) = rest.split_once('&') {
        out.push_str(plain);
        let Some((run, after)) = tail.split_once('-') else {
            out.push('&');
            rest = tail;
            continue;
        };
        rest = after;
        if run.is_empty() {
            out.push('&');
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD.decode(run.replace(',', "/"));
        let units: Option<Vec<u16>> = bytes.ok().filter(|b| b.len() % 2 == 0).map(|b| {
            b.chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect()
        });
        match units.and_then(|u| String::from_utf16(&u).ok()) {
            Some(text) => out.push_str(&text),
            None => out.push_str(&format!("&{run}-")),
        }
    }
    out.push_str(rest);
    out
}

impl Folder {
    /// Last path segment, e.g. "Work" for "INBOX/Work".
    pub fn display_name(&self) -> String {
        if self.role == Role::Inbox {
            return "Inbox".into();
        }
        let last = match &self.delimiter {
            Some(d) if !d.is_empty() => self.name.rsplit(d.as_str()).next().unwrap_or(&self.name),
            _ => &self.name,
        };
        decode_mutf7(last)
    }

    pub fn depth(&self) -> usize {
        match &self.delimiter {
            Some(d) if !d.is_empty() && self.role == Role::Other => {
                self.name.matches(d.as_str()).count()
            }
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageSummary {
    pub folder: String,
    pub uid: u32,
    pub message_id: String,
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub cc: String,
    pub date: i64,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub has_attachments: bool,
    pub snippet: String,
    pub in_reply_to: String,
    pub references: String,
    /// Gmail labels (`\Inbox`, `[Imap]/Archiv`), one per line, as the server names them; empty elsewhere.
    pub labels: String,
    /// Gmail's conversation id (X-GM-THRID); 0 elsewhere, and until it is fetched.
    pub thread: u64,
}

/// A message's flags on the server, from a refresh of the cached ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Flags {
    pub uid: u32,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    /// Gmail labels, one per line; None from servers without them.
    pub labels: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingOp {
    SetSeen(bool),
    SetFlagged(bool),
    SetAnswered,
    Move(String),
    /// Mark \Deleted and expunge, for messages already in Trash.
    Expunge,
    /// Gmail: adds and removes labels, which is how a message in All Mail moves.
    Relabel { add: Vec<String>, remove: Vec<String> },
}

impl PendingOp {
    fn encode(&self) -> (&'static str, String) {
        match self {
            PendingOp::SetSeen(v) => ("seen", v.to_string()),
            PendingOp::SetFlagged(v) => ("flagged", v.to_string()),
            PendingOp::SetAnswered => ("answered", String::new()),
            PendingOp::Move(dest) => ("move", dest.clone()),
            PendingOp::Expunge => ("expunge", String::new()),
            PendingOp::Relabel { add, remove } => {
                let lines: Vec<String> = add.iter().map(|l| format!("+{l}")).chain(remove.iter().map(|l| format!("-{l}"))).collect();
                ("labels", lines.join("\n"))
            }
        }
    }

    fn decode(kind: &str, arg: String) -> Option<PendingOp> {
        Some(match kind {
            "seen" => PendingOp::SetSeen(arg == "true"),
            "flagged" => PendingOp::SetFlagged(arg == "true"),
            "answered" => PendingOp::SetAnswered,
            "move" => PendingOp::Move(arg),
            "expunge" => PendingOp::Expunge,
            "labels" => {
                let (mut add, mut remove) = (Vec::new(), Vec::new());
                for line in arg.lines() {
                    match line.split_at_checked(1) {
                        Some(("+", l)) => add.push(l.to_string()),
                        Some(("-", l)) => remove.push(l.to_string()),
                        _ => {}
                    }
                }
                PendingOp::Relabel { add, remove }
            }
            _ => return None,
        })
    }
}

pub struct Store {
    conn: Connection,
}

const SUMMARY_COLUMNS: &str = "folder, uid, message_id, subject, from_name, from_addr, to_addrs, cc_addrs, \
     date, seen, flagged, answered, has_attachments, snippet, in_reply_to, refs, labels, thread";

fn summary_from_row(row: &rusqlite::Row) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        folder: row.get(0)?,
        uid: row.get(1)?,
        message_id: row.get(2)?,
        subject: row.get(3)?,
        from_name: row.get(4)?,
        from_addr: row.get(5)?,
        to: row.get(6)?,
        cc: row.get(7)?,
        date: row.get(8)?,
        seen: row.get(9)?,
        flagged: row.get(10)?,
        answered: row.get(11)?,
        has_attachments: row.get(12)?,
        snippet: row.get(13)?,
        in_reply_to: row.get(14)?,
        references: row.get(15)?,
        labels: row.get(16)?,
        // SQLite integers are signed; the id's bits are kept as they are.
        thread: row.get::<_, i64>(17)? as u64,
    })
}

/// Schema version of a new cache; [`migrate`] brings older ones up to it.
const SCHEMA_VERSION: i64 = 6;

/// SQL condition on a message `m`: not waiting to be moved away or deleted.
const KEPT: &str = "NOT EXISTS (SELECT 1 FROM pending_ops p WHERE p.folder = m.folder AND p.uid = m.uid AND p.kind IN ('move', 'expunge'))";

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Upgrades a cache from an older version, one step per version, in one transaction. The steps
/// are history: they spell out the schema of their time instead of reusing today's.
fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        // Caches from before folder_sync hold one contiguous window per folder.
        tx.execute("INSERT OR IGNORE INTO folder_sync (folder, synced_from) SELECT folder, MIN(uid) FROM messages GROUP BY folder", [])?;
    }
    if version < 2 {
        // Added while version 1 was current, so a cache may have some of them already.
        for (column, definition) in [
            ("href", "TEXT NOT NULL DEFAULT ''"),
            ("recurring", "INTEGER NOT NULL DEFAULT 0"),
            ("attendees", "TEXT NOT NULL DEFAULT ''"),
            ("ics", "TEXT NOT NULL DEFAULT ''"),
        ] {
            let exists: bool = tx.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('events') WHERE name = ?1", [column], |r| r.get(0))?;
            if !exists {
                tx.execute(&format!("ALTER TABLE events ADD COLUMN {column} {definition}"), [])?;
            }
        }
    }
    if version < 3 {
        // A cache older than pending_ops got the table, column included, from init.
        let exists: bool = tx.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('pending_ops') WHERE name = 'ready_at'", [], |r| r.get(0))?;
        if !exists {
            tx.execute("ALTER TABLE pending_ops ADD COLUMN ready_at INTEGER NOT NULL DEFAULT 0", [])?;
        }
    }
    if version < 4 {
        let exists: bool = tx.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('messages') WHERE name = 'labels'", [], |r| r.get(0))?;
        if !exists {
            tx.execute("ALTER TABLE messages ADD COLUMN labels TEXT NOT NULL DEFAULT ''", [])?;
        }
        // The next sync then refreshes the flags of everything cached, Gmail labels with them.
        tx.execute("UPDATE folder_sync SET modseq = NULL, exists_count = NULL", [])?;
    }
    if version < 5 {
        // A cache older than the messages table got it, column included, from init. Gmail's cached
        // mail gets its conversation ids on the next sync (`uids_without_thread`).
        let exists: bool = tx.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('messages') WHERE name = 'thread'", [], |r| r.get(0))?;
        if !exists {
            tx.execute("ALTER TABLE messages ADD COLUMN thread INTEGER NOT NULL DEFAULT 0", [])?;
        }
    }
    if version < 6 {
        // A cache older than the calendars table got it, column included, from init.
        let exists: bool = tx.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('calendars') WHERE name = 'tag'", [], |r| r.get(0))?;
        if !exists {
            tx.execute("ALTER TABLE calendars ADD COLUMN tag TEXT NOT NULL DEFAULT ''", [])?;
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let fresh = conn.query_row("SELECT COUNT(*) = 0 FROM sqlite_master", [], |r| r.get::<_, bool>(0))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS folders (
                 name TEXT PRIMARY KEY,
                 delimiter TEXT,
                 role TEXT NOT NULL DEFAULT '',
                 uidvalidity INTEGER
             );
             CREATE TABLE IF NOT EXISTS messages (
                 folder TEXT NOT NULL REFERENCES folders(name) ON DELETE CASCADE,
                 uid INTEGER NOT NULL,
                 message_id TEXT NOT NULL DEFAULT '',
                 subject TEXT NOT NULL DEFAULT '',
                 from_name TEXT NOT NULL DEFAULT '',
                 from_addr TEXT NOT NULL DEFAULT '',
                 to_addrs TEXT NOT NULL DEFAULT '',
                 cc_addrs TEXT NOT NULL DEFAULT '',
                 date INTEGER NOT NULL DEFAULT 0,
                 seen INTEGER NOT NULL DEFAULT 0,
                 flagged INTEGER NOT NULL DEFAULT 0,
                 answered INTEGER NOT NULL DEFAULT 0,
                 has_attachments INTEGER NOT NULL DEFAULT 0,
                 snippet TEXT NOT NULL DEFAULT '',
                 in_reply_to TEXT NOT NULL DEFAULT '',
                 refs TEXT NOT NULL DEFAULT '',
                 labels TEXT NOT NULL DEFAULT '',
                 raw BLOB,
                 thread INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (folder, uid)
             );
             CREATE INDEX IF NOT EXISTS messages_by_date ON messages(folder, date DESC);
             -- Joined on messages' implicit rowid: VACUUM keeps it (tested), a table rebuild must copy it.
             CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, sender, body);
             CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
                 DELETE FROM messages_fts WHERE rowid = old.rowid;
             END;
             CREATE TABLE IF NOT EXISTS folder_sync (
                 folder TEXT PRIMARY KEY REFERENCES folders(name) ON DELETE CASCADE,
                 synced_from INTEGER,
                 modseq INTEGER,
                 exists_count INTEGER
             );
             CREATE TABLE IF NOT EXISTS pending_ops (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 folder TEXT NOT NULL,
                 uid INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 arg TEXT NOT NULL DEFAULT '',
                 -- Unix seconds; until then the change can still be undone and is not sent.
                 ready_at INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS calendars (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL DEFAULT '',
                 color TEXT NOT NULL DEFAULT '',
                 tag TEXT NOT NULL DEFAULT ''
             );
             -- One row per instance: repeating events are expanded when stored.
             CREATE TABLE IF NOT EXISTS events (
                 calendar TEXT NOT NULL,
                 uid TEXT NOT NULL,
                 start INTEGER NOT NULL,
                 end INTEGER NOT NULL,
                 all_day INTEGER NOT NULL DEFAULT 0,
                 summary TEXT NOT NULL DEFAULT '',
                 location TEXT NOT NULL DEFAULT '',
                 description TEXT NOT NULL DEFAULT '',
                 organizer TEXT NOT NULL DEFAULT '',
                 organizer_name TEXT NOT NULL DEFAULT '',
                 -- Where it lives: a CalDAV resource URL or a Graph event id; empty on the device.
                 href TEXT NOT NULL DEFAULT '',
                 recurring INTEGER NOT NULL DEFAULT 0,
                 -- One line per attendee: partstat, address, name, tab-separated.
                 attendees TEXT NOT NULL DEFAULT '',
                 -- Device-only events: the VCALENDAR they were saved as, for editing.
                 ics TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS events_by_start ON events(start);",
        )?;
        if fresh {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else {
            migrate(&mut conn)?;
        }
        Ok(Store { conn })
    }

    // ---- folders ----

    /// Replaces the folder list with what the server reported. Folders that
    /// disappeared are deleted together with their cached messages. Returns whether anything changed.
    pub fn sync_folders(&mut self, folders: &[(String, Option<String>, Role)]) -> Result<bool> {
        let mut changed = 0;
        let tx = self.conn.transaction()?;
        {
            let mut existing = tx.prepare("SELECT name FROM folders")?;
            let names: Vec<String> = existing
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for name in names {
                if !folders.iter().any(|(n, _, _)| *n == name) {
                    tx.execute("DELETE FROM messages WHERE folder = ?1", [&name])?;
                    changed += tx.execute("DELETE FROM folders WHERE name = ?1", [&name])?;
                }
            }
            let mut upsert = tx.prepare(
                "INSERT INTO folders (name, delimiter, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET delimiter = excluded.delimiter, role = excluded.role
                 WHERE delimiter IS NOT excluded.delimiter OR role IS NOT excluded.role",
            )?;
            for (name, delim, role) in folders {
                changed += upsert.execute(params![name, delim, role.as_str()])?;
            }
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT f.name, f.delimiter, f.role,
                    (SELECT COUNT(*) FROM messages m WHERE m.folder = f.name AND m.seen = 0 AND {KEPT}),
                    (SELECT COUNT(*) FROM messages m WHERE m.folder = f.name AND {KEPT})
             FROM folders f"
        ))?;
        let mut folders: Vec<Folder> = stmt
            .query_map([], |r| {
                Ok(Folder {
                    name: r.get(0)?,
                    delimiter: r.get(1)?,
                    role: Role::parse(&r.get::<_, String>(2)?),
                    unread: r.get(3)?,
                    total: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        // Subfolders follow their parent: a special folder's role places it, a plain one
        // goes where its top-level ancestor goes (so "INBOX/Work" sits under Inbox).
        let ranks: std::collections::HashMap<String, i64> = folders
            .iter()
            .map(|f| (f.name.clone(), f.role.rank()))
            .collect();
        folders.sort_by_cached_key(|f| {
            let path: Vec<String> = match &f.delimiter {
                Some(d) if !d.is_empty() => {
                    f.name.split(d.as_str()).map(str::to_lowercase).collect()
                }
                _ => vec![f.name.to_lowercase()],
            };
            let root = f
                .delimiter
                .as_deref()
                .filter(|d| !d.is_empty())
                .map_or(&*f.name, |d| f.name.split(d).next().unwrap());
            let rank = if f.role == Role::Other {
                ranks.get(root).copied().unwrap_or(f.role.rank())
            } else {
                f.role.rank()
            };
            (rank, path)
        });
        Ok(folders)
    }

    pub fn folder_by_role(&self, role: Role) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM folders WHERE role = ?1 ORDER BY name LIMIT 1",
                [role.as_str()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn folder_role(&self, folder: &str) -> Result<Role> {
        let role: Option<String> = self
            .conn
            .query_row("SELECT role FROM folders WHERE name = ?1", [folder], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(role.map(|r| Role::parse(&r)).unwrap_or(Role::Other))
    }

    /// Records the server's UIDVALIDITY. Returns true if it changed, in which case
    /// all cached messages of the folder were dropped (their UIDs are meaningless now).
    pub fn check_uidvalidity(&mut self, folder: &str, uidvalidity: u32) -> Result<bool> {
        let current: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT uidvalidity FROM folders WHERE name = ?1",
                [folder],
                |r| r.get(0),
            )
            .optional()?;
        let changed = matches!(current, Some(Some(v)) if v != uidvalidity as i64);
        if changed {
            self.conn
                .execute("DELETE FROM messages WHERE folder = ?1", [folder])?;
            self.conn
                .execute("DELETE FROM pending_ops WHERE folder = ?1", [folder])?;
            self.conn
                .execute("DELETE FROM folder_sync WHERE folder = ?1", [folder])?;
        }
        self.conn.execute(
            "UPDATE folders SET uidvalidity = ?2 WHERE name = ?1",
            params![folder, uidvalidity],
        )?;
        Ok(changed)
    }

    // ---- messages ----

    pub fn max_uid(&self, folder: &str) -> Result<Option<u32>> {
        Ok(self.conn.query_row(
            "SELECT MAX(uid) FROM messages WHERE folder = ?1",
            [folder],
            |r| r.get(0),
        )?)
    }

    /// Lowest UID of the folder's synced window: every server message from it up is cached.
    /// Older cached messages are search hits, cached one by one. None before the first sync.
    pub fn synced_from(&self, folder: &str) -> Result<Option<u32>> {
        Ok(self
            .conn
            .query_row("SELECT synced_from FROM folder_sync WHERE folder = ?1", [folder], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn set_synced_from(&self, folder: &str, uid: u32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO folder_sync (folder, synced_from) VALUES (?1, ?2)
             ON CONFLICT(folder) DO UPDATE SET synced_from = excluded.synced_from",
            params![folder, uid],
        )?;
        Ok(())
    }

    /// (HIGHESTMODSEQ, message count) the folder's last finished sync saw.
    pub fn sync_mark(&self, folder: &str) -> Result<Option<(u64, u32)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT modseq, exists_count FROM folder_sync WHERE folder = ?1 AND modseq IS NOT NULL",
                [folder],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_sync_mark(&self, folder: &str, mark: Option<(u64, u32)>) -> Result<()> {
        let (modseq, exists) = (mark.map(|m| m.0 as i64), mark.map(|m| m.1));
        self.conn.execute(
            "INSERT INTO folder_sync (folder, modseq, exists_count) VALUES (?1, ?2, ?3)
             ON CONFLICT(folder) DO UPDATE SET modseq = excluded.modseq, exists_count = excluded.exists_count",
            params![folder, modseq, exists],
        )?;
        Ok(())
    }

    pub fn uids(&self, folder: &str) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM messages WHERE folder = ?1")?;
        let uids = stmt
            .query_map([folder], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(uids)
    }

    /// Inserts new messages or refreshes flags of known ones. Locally cached
    /// bodies and snippets are kept.
    pub fn upsert_messages(&mut self, messages: &[MessageSummary]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut upsert = tx.prepare(&format!(
                "INSERT INTO messages ({SUMMARY_COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
                 ON CONFLICT(folder, uid) DO UPDATE SET
                     seen = excluded.seen, flagged = excluded.flagged, answered = excluded.answered,
                     labels = excluded.labels
                 RETURNING rowid"
            ))?;
            let mut fts_exists = tx.prepare("SELECT 1 FROM messages_fts WHERE rowid = ?1")?;
            let mut fts_insert = tx.prepare(
                "INSERT INTO messages_fts (rowid, subject, sender, body) VALUES (?1, ?2, ?3, '')",
            )?;
            for m in messages {
                let rowid: i64 = upsert.query_row(
                    params![
                        m.folder,
                        m.uid,
                        m.message_id,
                        m.subject,
                        m.from_name,
                        m.from_addr,
                        m.to,
                        m.cc,
                        m.date,
                        m.seen,
                        m.flagged,
                        m.answered,
                        m.has_attachments,
                        m.snippet,
                        m.in_reply_to,
                        m.references,
                        m.labels,
                        m.thread as i64
                    ],
                    |r| r.get(0),
                )?;
                if !fts_exists.exists([rowid])? {
                    fts_insert.execute(params![
                        rowid,
                        m.subject,
                        format!("{} {}", m.from_name, m.from_addr)
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Updates flags from the server, skipping messages with local changes not yet
    /// pushed (the server state is stale for those). Returns whether any flag changed.
    pub fn update_flags(&mut self, folder: &str, flags: &[Flags]) -> Result<bool> {
        let pending: std::collections::HashSet<u32> = {
            let mut stmt = self
                .conn
                .prepare("SELECT uid FROM pending_ops WHERE folder = ?1")?;
            stmt.query_map([folder], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let mut changed = 0;
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET seen = ?3, flagged = ?4, answered = ?5 WHERE folder = ?1 AND uid = ?2
                 AND (seen, flagged, answered) IS NOT (?3, ?4, ?5)",
            )?;
            // Labels a pending relabel changes are shown from the queue (see `labels`), so these stay current.
            let mut labels = tx.prepare("UPDATE messages SET labels = ?3 WHERE folder = ?1 AND uid = ?2 AND labels != ?3")?;
            for f in flags {
                if !pending.contains(&f.uid) {
                    changed += stmt.execute(params![folder, f.uid, f.seen, f.flagged, f.answered])?;
                }
                if let Some(l) = &f.labels {
                    changed += labels.execute(params![folder, f.uid, l])?;
                }
            }
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Cached messages in `folder` whose conversation id is not known yet.
    pub fn uids_without_thread(&self, folder: &str) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare("SELECT uid FROM messages WHERE folder = ?1 AND thread = 0")?;
        let uids = stmt.query_map([folder], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(uids)
    }

    /// Records the conversation id of messages in `folder`: (uid, id).
    pub fn set_threads(&mut self, folder: &str, threads: &[(u32, u64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE messages SET thread = ?3 WHERE folder = ?1 AND uid = ?2")?;
            for (uid, thread) in threads {
                stmt.execute(params![folder, uid, *thread as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_messages(&mut self, folder: &str, uids: &[u32]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM messages WHERE folder = ?1 AND uid = ?2")?;
            for uid in uids {
                stmt.execute(params![folder, uid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn messages(&self, folder: &str, limit: usize) -> Result<Vec<MessageSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m WHERE folder = ?1 AND {KEPT} ORDER BY date DESC, uid DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![folder, limit as i64], summary_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Every cached message, for grouping into conversations.
    pub fn all_messages(&self) -> Result<Vec<MessageSummary>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM messages m WHERE {KEPT}"))?;
        let rows = stmt
            .query_map([], summary_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// The folders holding this message, `folder` included: on Gmail one message is in several
    /// (Inbox, All Mail, its labels), and a move or delete can change all of them.
    pub fn folders_with_copies(&self, folder: &str, uid: u32) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT c.folder FROM messages m JOIN messages c ON c.message_id = m.message_id
             WHERE m.folder = ?1 AND m.uid = ?2 AND m.message_id != ''
             UNION SELECT ?1",
        )?;
        let folders = stmt.query_map(params![folder, uid], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(folders)
    }

    /// None also while it waits to be moved or deleted: the server still lists it there until then.
    pub fn message(&self, folder: &str, uid: u32) -> Result<Option<MessageSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m WHERE folder = ?1 AND uid = ?2 AND {KEPT}"
        ))?;
        Ok(stmt
            .query_row(params![folder, uid], summary_from_row)
            .optional()?)
    }

    /// Full-text search over subject, sender and downloaded bodies, in `folder` or all of them.
    /// A `from:address` word keeps only mail from exactly that sender (any case); the other words
    /// still match text. Nothing to match lists the folder's (or all) mail, newest first.
    pub fn search(&self, query: &str, folder: Option<&str>, limit: usize) -> Result<Vec<MessageSummary>> {
        let (from, words): (Vec<&str>, Vec<&str>) =
            query.split_whitespace().partition(|w| w.len() > 5 && w[..5].eq_ignore_ascii_case("from:"));
        let from = from.last().map(|w| w[5..].to_lowercase());
        let fts_query = fts_query(&words.join(" "));
        let columns = SUMMARY_COLUMNS
            .split(", ")
            .map(|c| format!("m.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let text = if fts_query.is_empty() {
            "SELECT {columns} FROM messages m WHERE ?1 = ''"
        } else {
            "SELECT {columns} FROM messages_fts f JOIN messages m ON m.rowid = f.rowid WHERE messages_fts MATCH ?1"
        };
        let mut stmt = self.conn.prepare(&format!(
            "{} AND {KEPT} AND (?2 IS NULL OR lower(m.from_addr) = ?2) AND (?4 IS NULL OR m.folder = ?4) ORDER BY m.date DESC LIMIT ?3",
            text.replace("{columns}", &columns)
        ))?;
        let rows = stmt
            .query_map(params![fts_query, from, limit as i64, folder], summary_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn raw(&self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT raw FROM messages WHERE folder = ?1 AND uid = ?2",
                params![folder, uid],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// UIDs of up to `limit` messages among the newest `within` of a folder whose body has not been downloaded.
    pub fn uids_missing_body(&self, folder: &str, limit: usize, within: usize) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT uid FROM (SELECT uid, raw FROM messages WHERE folder = ?1 ORDER BY date DESC, uid DESC LIMIT ?3)
             WHERE raw IS NULL LIMIT ?2",
        )?;
        let uids = stmt
            .query_map(params![folder, limit as i64, within as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(uids)
    }

    /// Drops downloaded bodies beyond the newest `keep` of a folder. Their text stays
    /// searchable; opening one downloads it again.
    pub fn evict_bodies(&self, folder: &str, keep: usize) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET raw = NULL WHERE folder = ?1 AND raw IS NOT NULL AND uid NOT IN
             (SELECT uid FROM messages WHERE folder = ?1 ORDER BY date DESC, uid DESC LIMIT ?2)",
            params![folder, keep as i64],
        )?;
        Ok(())
    }

    pub fn set_body(
        &mut self,
        folder: &str,
        uid: u32,
        raw: &[u8],
        snippet: &str,
        body_text: &str,
        has_attachments: bool,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let rowid: Option<i64> = tx
            .query_row(
                "UPDATE messages SET raw = ?3, snippet = ?4, has_attachments = ?5
                 WHERE folder = ?1 AND uid = ?2 RETURNING rowid",
                params![folder, uid, raw, snippet, has_attachments],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(rowid) = rowid {
            tx.execute(
                "UPDATE messages_fts SET body = ?2 WHERE rowid = ?1",
                params![rowid, body_text],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- local changes ----

    /// Applies a change to the cache immediately and queues it for the server.
    pub fn apply_local(&mut self, folder: &str, uid: u32, op: &PendingOp) -> Result<i64> {
        self.apply_local_until(folder, uid, op, 0)
    }

    /// The same, held back from the server until `ready_at` (Unix seconds) so [`Store::undo`] can take it back.
    /// Returns the queued change's id.
    pub fn apply_local_until(&mut self, folder: &str, uid: u32, op: &PendingOp, ready_at: i64) -> Result<i64> {
        match op {
            PendingOp::SetSeen(v) => {
                self.conn.execute(
                    "UPDATE messages SET seen = ?3 WHERE folder = ?1 AND uid = ?2",
                    params![folder, uid, v],
                )?;
            }
            PendingOp::SetFlagged(v) => {
                self.conn.execute(
                    "UPDATE messages SET flagged = ?3 WHERE folder = ?1 AND uid = ?2",
                    params![folder, uid, v],
                )?;
            }
            PendingOp::SetAnswered => {
                self.conn.execute(
                    "UPDATE messages SET answered = 1 WHERE folder = ?1 AND uid = ?2",
                    params![folder, uid],
                )?;
            }
            // Hidden from the lists while queued (see KEPT); the engine deletes it once the server has moved it.
            PendingOp::Move(_) | PendingOp::Expunge => {}
            // Shown from the queue by `labels`, so Undo needs nothing restored.
            PendingOp::Relabel { .. } => {}
        }
        let (kind, arg) = op.encode();
        self.conn.execute(
            "INSERT INTO pending_ops (folder, uid, kind, arg, ready_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![folder, uid, kind, arg, ready_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A message's Gmail labels as they will be once queued relabels are sent.
    pub fn labels(&self, folder: &str, uid: u32) -> Result<Vec<String>> {
        let stored: String = self
            .conn
            .query_row("SELECT labels FROM messages WHERE folder = ?1 AND uid = ?2", params![folder, uid], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        let mut labels: Vec<String> = stored.lines().map(str::to_string).collect();
        let mut stmt = self.conn.prepare("SELECT arg FROM pending_ops WHERE folder = ?1 AND uid = ?2 AND kind = 'labels' ORDER BY id")?;
        for arg in stmt.query_map(params![folder, uid], |r| r.get::<_, String>(0))? {
            if let Some(PendingOp::Relabel { add, remove }) = PendingOp::decode("labels", arg?) {
                labels.retain(|l| !remove.contains(l));
                labels.extend(add.into_iter().filter(|l| !labels.contains(l)).collect::<Vec<_>>());
            }
        }
        Ok(labels)
    }

    /// Takes back queued changes not sent yet; returns how many were. A moved or deleted message
    /// reappears; flags stay as they are (undo of those is not offered).
    pub fn undo(&self, ids: &[i64]) -> Result<usize> {
        let mut stmt = self.conn.prepare("DELETE FROM pending_ops WHERE id = ?1")?;
        let mut undone = 0;
        for id in ids {
            undone += stmt.execute([id])?;
        }
        Ok(undone)
    }

    pub fn pending_ops(&self) -> Result<Vec<(i64, String, u32, PendingOp)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, folder, uid, kind, arg FROM pending_ops WHERE ready_at <= ?1 ORDER BY id")?;
        let rows = stmt
            .query_map([now()], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, String>(3)?,
                    r.get(4)?,
                ))
            })?
            .collect::<Result<Vec<(i64, String, u32, String, String)>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, folder, uid, kind, arg)| {
                PendingOp::decode(&kind, arg).map(|op| (id, folder, uid, op))
            })
            .collect())
    }

    pub fn remove_pending_op(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_ops WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- calendar ----

    /// Replaces everything synced from the server; [`LOCAL_CALENDAR`] stays.
    /// `kept`: calendars the server reports unchanged, whose stored events stay as they are.
    pub fn replace_synced_events(&mut self, calendars: &[CalendarInfo], events: &[StoredEvent], kept: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM calendars", [])?;
        let stored: Vec<String> = tx
            .prepare("SELECT DISTINCT calendar FROM events WHERE calendar != ?1")?
            .query_map([LOCAL_CALENDAR], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in stored.iter().filter(|id| !kept.contains(id)) {
            tx.execute("DELETE FROM events WHERE calendar = ?1", [id])?;
        }
        for c in calendars {
            tx.execute("INSERT OR REPLACE INTO calendars (id, name, color, tag) VALUES (?1, ?2, ?3, ?4)", params![c.id, c.name, c.color, c.tag])?;
        }
        insert_events(&tx, events, "")?;
        tx.commit()?;
        Ok(())
    }

    /// Keeps the instances of the VCALENDAR `ics` on this device, for accounts without a calendar
    /// server; replaces older copies by UID.
    pub fn add_local_events(&mut self, ics: &str, instances: &[Event]) -> Result<()> {
        let recurring = crate::ical::parse(ics).events.iter().any(|e| e.rrule.is_some() || e.recurrence_id.is_some());
        let tx = self.conn.transaction()?;
        for e in instances {
            tx.execute("DELETE FROM events WHERE calendar = ?1 AND uid = ?2", params![LOCAL_CALENDAR, e.uid])?;
        }
        let rows: Vec<StoredEvent> = instances
            .iter()
            .map(|e| StoredEvent { calendar: LOCAL_CALENDAR.into(), recurring, event: e.clone(), ..Default::default() })
            .collect();
        insert_events(&tx, &rows, ics)?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_local_events(&mut self, uid: &str) -> Result<()> {
        self.conn.execute("DELETE FROM events WHERE calendar = ?1 AND uid = ?2", params![LOCAL_CALENDAR, uid])?;
        Ok(())
    }

    /// The VCALENDAR a device-only event was saved as.
    pub fn local_ics(&self, uid: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT ics FROM events WHERE calendar = ?1 AND uid = ?2 AND ics != '' LIMIT 1", params![LOCAL_CALENDAR, uid], |r| r.get(0))
            .optional()?)
    }

    /// Where the event with this UID is stored (any instance of it), if it is.
    pub fn event_by_uid(&self, uid: &str) -> Result<Option<StoredEvent>> {
        Ok(self
            .conn
            .query_row("SELECT calendar, href, recurring FROM events WHERE uid = ?1 LIMIT 1", [uid], |r| {
                Ok(StoredEvent { calendar: r.get(0)?, href: r.get(1)?, recurring: r.get(2)?, ..Default::default() })
            })
            .optional()?)
    }

    /// The synced calendars, in the server's order.
    pub fn calendars(&self) -> Result<Vec<CalendarInfo>> {
        let mut stmt = self.conn.prepare("SELECT id, name, color, tag FROM calendars ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| Ok(CalendarInfo { id: r.get(0)?, name: r.get(1)?, color: r.get(2)?, tag: r.get(3)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Instances overlapping `from..to`, by start.
    pub fn events(&self, from: i64, to: i64) -> Result<Vec<StoredEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, start, end, all_day, summary, location, description, organizer, organizer_name,
                    COALESCE(c.color, ''), calendar, href, recurring, COALESCE(c.name, ''), attendees
             FROM events LEFT JOIN calendars c ON c.id = events.calendar
             WHERE start < ?2 AND (end > ?1 OR (end = start AND start >= ?1)) ORDER BY start",
        )?;
        let rows = stmt.query_map(params![from, to], |r| {
            let organizer: String = r.get(7)?;
            let attendees: String = r.get(14)?;
            Ok(StoredEvent {
                color: r.get(9)?,
                calendar: r.get(10)?,
                href: r.get(11)?,
                recurring: r.get(12)?,
                calendar_name: r.get(13)?,
                event: Event {
                    uid: r.get(0)?,
                    start: r.get(1)?,
                    end: r.get(2)?,
                    all_day: r.get(3)?,
                    summary: r.get(4)?,
                    location: r.get(5)?,
                    description: r.get(6)?,
                    organizer: (!organizer.is_empty())
                        .then(|| Ok::<_, rusqlite::Error>(Attendee { email: organizer, name: r.get(8)?, partstat: String::new() }))
                        .transpose()?,
                    attendees: attendees
                        .lines()
                        .filter_map(|l| {
                            let mut f = l.splitn(3, '\t');
                            let (partstat, email, name) = (f.next()?, f.next()?, f.next().unwrap_or(""));
                            Some(Attendee { email: email.into(), name: name.into(), partstat: partstat.into() })
                        })
                        .collect(),
                    ..Default::default()
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// An event instance as stored: where it lives, and what it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoredEvent {
    /// [`CalendarInfo::id`], or [`LOCAL_CALENDAR`].
    pub calendar: String,
    /// The CalDAV resource URL or Graph event id (of the series, for a repeating event's instance).
    pub href: String,
    /// One instance of a repeating event: changes apply to the whole series.
    pub recurring: bool,
    /// Calendar color (`#rrggbb…`) and name, empty when unknown; filled when reading.
    pub color: String,
    pub calendar_name: String,
    pub event: Event,
}

/// The calendar of events kept only on this device.
pub const LOCAL_CALENDAR: &str = "local";

/// A calendar on the server: its URL (CalDAV) or id (Microsoft Graph).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalendarInfo {
    pub id: String,
    pub name: String,
    pub color: String,
    /// The server's change tag (sync-token, getctag) its events were read at; empty when it has none.
    pub tag: String,
}

fn insert_events(tx: &rusqlite::Transaction, events: &[StoredEvent], ics: &str) -> Result<()> {
    let mut insert = tx.prepare(
        "INSERT INTO events (calendar, uid, start, end, all_day, summary, location, description, organizer, organizer_name, href, recurring, ics, attendees)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?;
    for s in events {
        let e = &s.event;
        let (org, org_name) = e.organizer.as_ref().map_or(("", ""), |o| (o.email.as_str(), o.name.as_str()));
        let clean = |s: &str| s.replace(['\t', '\n'], " ");
        let attendees: Vec<String> = e.attendees.iter().map(|a| format!("{}\t{}\t{}", a.partstat, clean(&a.email), clean(&a.name))).collect();
        insert.execute(params![s.calendar, e.uid, e.start, e.end, e.all_day, e.summary, e.location, e.description, org, org_name, s.href, s.recurring, ics, attendees.join("\n")])?;
    }
    Ok(())
}

/// Turns user input into an FTS5 query: every word must match as a prefix.
fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '@' || *c == '.')
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_columns_to_older_caches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE events (calendar TEXT NOT NULL, uid TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, all_day INTEGER NOT NULL DEFAULT 0, summary TEXT NOT NULL DEFAULT '', location TEXT NOT NULL DEFAULT '', description TEXT NOT NULL DEFAULT '', organizer TEXT NOT NULL DEFAULT '', organizer_name TEXT NOT NULL DEFAULT '');
                 INSERT INTO events (calendar, uid, start, end, summary) VALUES ('local', 'kept', 100, 200, 'Kept');")
            .unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.events(0, 1000).unwrap()[0].event.summary, "Kept");
        // Opening again finds the columns there.
        drop(store);
        Store::open(&path).unwrap();
    }

    #[test]
    fn search_survives_vacuum() {
        let mut store = Store::open_in_memory().unwrap();
        store.sync_folders(&[("INBOX".into(), None, Role::Inbox)]).unwrap();
        let message = |uid: u32, subject: &str| MessageSummary { folder: "INBOX".into(), uid, subject: subject.into(), ..Default::default() };
        store.upsert_messages(&[message(1, "Gone"), message(2, "Lunch plans")]).unwrap();
        // A gap in the rowids, which a VACUUM that renumbered them would close.
        store.delete_messages("INBOX", &[1]).unwrap();
        store.conn.execute_batch("VACUUM").unwrap();
        let found: Vec<u32> = store.search("lunch", None, 10).unwrap().into_iter().map(|m| m.uid).collect();
        assert_eq!(found, [2]);
    }

    #[test]
    fn calendar_sync_keeps_local_events() {
        let mut store = Store::open_in_memory().unwrap();
        let max = Attendee { email: "max@example.org".into(), name: "Doe, Max".into(), partstat: "ACCEPTED".into() };
        let ev = |uid: &str, start: i64| Event { uid: uid.into(), summary: uid.into(), start, end: start + 3600, attendees: vec![max.clone()], ..Default::default() };
        let synced = |uid: &str, start: i64| StoredEvent { calendar: "https://dav/cal/".into(), href: format!("https://dav/cal/{uid}.ics"), event: ev(uid, start), ..Default::default() };
        store.add_local_events("BEGIN:VCALENDAR", &[ev("accepted", 1000)]).unwrap();
        let cal = CalendarInfo { id: "https://dav/cal/".into(), name: "Work".into(), color: "#ff0000".into(), tag: "t1".into() };
        store.replace_synced_events(std::slice::from_ref(&cal), &[synced("a", 5000), synced("b", 90_000)], &[]).unwrap();
        store.replace_synced_events(std::slice::from_ref(&cal), &[synced("a", 5000)], &[]).unwrap();
        // Unchanged on the server: its events stay without being read again.
        store.replace_synced_events(std::slice::from_ref(&cal), &[], std::slice::from_ref(&cal.id)).unwrap();
        let found: Vec<(String, String, String)> =
            store.events(0, 10_000).unwrap().into_iter().map(|s| (s.color, s.href, s.event.uid)).collect();
        assert_eq!(found, [("".into(), "".into(), "accepted".into()), ("#ff0000".into(), "https://dav/cal/a.ics".into(), "a".into())]);
        assert_eq!(store.events(0, 10_000).unwrap()[1].calendar_name, "Work");
        assert_eq!(store.events(0, 10_000).unwrap()[1].event.attendees, std::slice::from_ref(&max));
        assert_eq!(store.calendars().unwrap(), [cal]);
        // A newer copy of a local event replaces the old one.
        store.add_local_events("BEGIN:VCALENDAR\r\nX", &[ev("accepted", 2000)]).unwrap();
        assert_eq!(store.events(0, 10_000).unwrap()[0].event.start, 2000);
        assert_eq!(store.local_ics("accepted").unwrap().as_deref(), Some("BEGIN:VCALENDAR\r\nX"));
        store.remove_local_events("accepted").unwrap();
        assert_eq!(store.events(0, 10_000).unwrap().len(), 1);
    }

    fn msg(uid: u32, subject: &str) -> MessageSummary {
        MessageSummary {
            folder: "INBOX".into(),
            uid,
            subject: subject.into(),
            from_name: "Alice".into(),
            from_addr: "alice@example.org".into(),
            date: uid as i64 * 100,
            ..Default::default()
        }
    }

    fn store_with_inbox() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_folders(&[
                ("INBOX".into(), Some("/".into()), Role::Inbox),
                ("Trash".into(), None, Role::Trash),
            ])
            .unwrap();
        store
    }

    #[test]
    fn upsert_keeps_body_and_updates_flags() {
        let mut store = store_with_inbox();
        store
            .upsert_messages(&[msg(1, "Hello"), msg(2, "World")])
            .unwrap();
        store
            .set_body("INBOX", 1, b"raw", "snip", "the body text", false)
            .unwrap();

        let mut seen = msg(1, "Hello");
        seen.seen = true;
        store.upsert_messages(&[seen]).unwrap();

        let m = store.message("INBOX", 1).unwrap().unwrap();
        assert!(m.seen);
        assert_eq!(m.snippet, "snip");
        assert_eq!(store.raw("INBOX", 1).unwrap().as_deref(), Some(&b"raw"[..]));
        assert_eq!(
            store
                .messages("INBOX", 10)
                .unwrap()
                .iter()
                .map(|m| m.uid)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(store.folders().unwrap()[0].unread, 1);
    }

    #[test]
    fn search_matches_subject_sender_and_body() {
        let mut store = store_with_inbox();
        store
            .upsert_messages(&[msg(1, "Quarterly report"), msg(2, "Lunch")])
            .unwrap();
        store
            .set_body("INBOX", 2, b"raw", "", "let's meet at the pizzeria", false)
            .unwrap();

        assert_eq!(store.search("quart", None, 10).unwrap()[0].uid, 1);
        assert_eq!(store.search("pizzeria", None, 10).unwrap()[0].uid, 2);
        assert_eq!(store.search("alice", None, 10).unwrap().len(), 2);
        assert!(store.search("\"; DROP", None, 10).unwrap().is_empty());

        store.delete_messages("INBOX", &[2]).unwrap();
        assert!(store.search("pizzeria", None, 10).unwrap().is_empty());
    }

    #[test]
    fn search_by_sender() {
        let mut store = store_with_inbox();
        let bob = |uid, subject: &str| MessageSummary { from_addr: "Bob@Example.org".into(), from_name: "Bob".into(), ..msg(uid, subject) };
        store.upsert_messages(&[msg(1, "Invoice"), bob(2, "Invoice"), bob(3, "Lunch")]).unwrap();
        // Alice's mail mentions Bob's address: a text search finds it, a sender search must not.
        store.set_body("INBOX", 1, b"raw", "", "forwarding bob@example.org's invoice", false).unwrap();
        let uids = |q: &str| store.search(q, None, 10).unwrap().iter().map(|m| m.uid).collect::<Vec<_>>();
        assert_eq!(uids("from:bob@example.org"), [3, 2]);
        assert_eq!(uids("FROM:BOB@example.ORG invoice"), [2]);
        assert_eq!(uids("bob@example.org"), [3, 2, 1]);
        assert!(uids("from:nobody@example.org").is_empty());
        assert!(uids("from:'; DROP TABLE messages; --").is_empty());
    }

    #[test]
    fn queued_relabels_show_in_labels() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[MessageSummary { labels: "\\Inbox\nWork".into(), ..msg(1, "a") }]).unwrap();
        let op = PendingOp::Relabel { add: vec!["Projects".into()], remove: vec!["\\Inbox".into(), "Work".into()] };
        let (kind, arg) = op.encode();
        assert_eq!(PendingOp::decode(kind, arg), Some(op.clone()));
        let id = store.apply_local_until("INBOX", 1, &op, now() + 60).unwrap();
        assert_eq!(store.labels("INBOX", 1).unwrap(), ["Projects"]);
        // Not hidden: a relabelled message stays where it is listed.
        assert_eq!(store.messages("INBOX", 10).unwrap().len(), 1);
        store.undo(&[id]).unwrap();
        assert_eq!(store.labels("INBOX", 1).unwrap(), ["\\Inbox", "Work"]);
        // A refresh from the server replaces them.
        store.update_flags("INBOX", &[Flags { uid: 1, labels: Some("Work".into()), ..Default::default() }]).unwrap();
        assert_eq!(store.labels("INBOX", 1).unwrap(), ["Work"]);
    }

    #[test]
    fn search_in_one_folder() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "Invoice"), MessageSummary { folder: "Trash".into(), ..msg(2, "Invoice") }]).unwrap();
        let uids = |q: &str, folder| store.search(q, folder, 10).unwrap().iter().map(|m| m.uid).collect::<Vec<_>>();
        assert_eq!(uids("invoice", None), [2, 1]);
        assert_eq!(uids("invoice", Some("Trash")), [2]);
        assert_eq!(uids("from:alice@example.org", Some("INBOX")), [1]);
        // Nothing to match: everything, the "in" filter alone.
        assert_eq!(uids("", None), [2, 1]);
    }

    #[test]
    fn syncs_report_changes() {
        let mut store = store_with_inbox();
        let folders = [("INBOX".into(), Some("/".into()), Role::Inbox), ("Trash".into(), None, Role::Trash)];
        assert!(!store.sync_folders(&folders).unwrap());
        assert!(store.sync_folders(&folders[..1]).unwrap());
        store.upsert_messages(&[msg(1, "a")]).unwrap();
        assert!(!store.update_flags("INBOX", &[Flags { uid: 1, ..Default::default() }]).unwrap());
        assert!(store.update_flags("INBOX", &[Flags { uid: 1, seen: true, ..Default::default() }]).unwrap());
        assert!(!store.update_flags("INBOX", &[Flags { uid: 1, seen: true, ..Default::default() }]).unwrap());
    }

    #[test]
    fn uidvalidity_change_drops_cache() {
        let mut store = store_with_inbox();
        assert!(!store.check_uidvalidity("INBOX", 7).unwrap());
        store.upsert_messages(&[msg(1, "a")]).unwrap();
        assert!(!store.check_uidvalidity("INBOX", 7).unwrap());
        assert!(store.check_uidvalidity("INBOX", 8).unwrap());
        assert!(store.messages("INBOX", 10).unwrap().is_empty());
    }

    #[test]
    fn old_bodies_are_evicted_and_not_prefetched_again() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "old"), msg(2, "mid"), msg(3, "new")]).unwrap();
        assert_eq!(store.uids_missing_body("INBOX", 10, 2).unwrap(), [3, 2]);
        for uid in 1..=3 {
            store.set_body("INBOX", uid, b"raw", "", &format!("body{uid}"), false).unwrap();
        }
        store.evict_bodies("INBOX", 2).unwrap();
        assert!(store.raw("INBOX", 1).unwrap().is_none());
        assert!(store.raw("INBOX", 2).unwrap().is_some());
        assert!(store.uids_missing_body("INBOX", 10, 2).unwrap().is_empty());
        assert_eq!(store.search("body1", None, 10).unwrap()[0].uid, 1);
    }

    #[test]
    fn sync_state_resets_with_uidvalidity() {
        let store = &mut store_with_inbox();
        store.check_uidvalidity("INBOX", 7).unwrap();
        assert_eq!(store.synced_from("INBOX").unwrap(), None);
        store.set_synced_from("INBOX", 40).unwrap();
        store.set_sync_mark("INBOX", Some((90060115205545359, 12))).unwrap();
        assert_eq!(store.synced_from("INBOX").unwrap(), Some(40));
        assert_eq!(store.sync_mark("INBOX").unwrap(), Some((90060115205545359, 12)));
        store.check_uidvalidity("INBOX", 8).unwrap();
        assert_eq!((store.synced_from("INBOX").unwrap(), store.sync_mark("INBOX").unwrap()), (None, None));
    }

    #[test]
    fn pending_ops_protect_local_flags() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "a")]).unwrap();
        store
            .apply_local("INBOX", 1, &PendingOp::SetSeen(true))
            .unwrap();
        store
            .update_flags("INBOX", &[Flags { uid: 1, ..Default::default() }])
            .unwrap();
        assert!(store.message("INBOX", 1).unwrap().unwrap().seen);

        let ops = store.pending_ops().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].3, PendingOp::SetSeen(true));
        store.remove_pending_op(ops[0].0).unwrap();

        store
            .apply_local("INBOX", 1, &PendingOp::Move("Trash".into()))
            .unwrap();
        assert!(store.messages("INBOX", 10).unwrap().is_empty());
    }

    #[test]
    fn held_move_hides_until_undone() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "a")]).unwrap();
        let id = store.apply_local_until("INBOX", 1, &PendingOp::Move("Trash".into()), now() + 60).unwrap();
        assert!(store.messages("INBOX", 10).unwrap().is_empty());
        assert!(store.all_messages().unwrap().is_empty());
        assert!(store.message("INBOX", 1).unwrap().is_none(), "a server search hit would bring it back");
        assert!(store.pending_ops().unwrap().is_empty(), "held op sent before its time");
        assert_eq!(store.undo(&[id]).unwrap(), 1);
        assert_eq!(store.messages("INBOX", 10).unwrap().len(), 1);
        assert_eq!(store.undo(&[id]).unwrap(), 0);
    }

    #[test]
    fn decodes_modified_utf7_names() {
        assert_eq!(decode_mutf7("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_mutf7("Gel&APY-scht"), "Gelöscht");
        assert_eq!(decode_mutf7("&ZeVnLIqe-"), "日本語");
        assert_eq!(decode_mutf7("Tom &- Jerry"), "Tom & Jerry");
        assert_eq!(decode_mutf7("bad&!!-x&open"), "bad&!!-x&open");
    }

    #[test]
    fn folders_sort_by_role() {
        let store = store_with_inbox();
        let names: Vec<_> = store
            .folders()
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["INBOX", "Trash"]);
    }

    #[test]
    fn subfolders_follow_their_parent() {
        let mut store = Store::open_in_memory().unwrap();
        let f = |name: &str, role| (name.to_string(), Some("/".to_string()), role);
        store
            .sync_folders(&[
                f("projects-old", Role::Other),
                f("Projects/Web", Role::Other),
                f("Trash", Role::Trash),
                f("INBOX/Receipts/2026", Role::Other),
                f("Projects", Role::Other),
                f("INBOX", Role::Inbox),
                f("INBOX/Receipts", Role::Other),
            ])
            .unwrap();
        let folders = store.folders().unwrap();
        let rows: Vec<_> = folders
            .iter()
            .map(|f| (f.display_name(), f.depth()))
            .collect();
        let row = |name: &str, depth| (name.to_string(), depth);
        assert_eq!(
            rows,
            vec![
                row("Inbox", 0),
                row("Receipts", 1),
                row("2026", 2),
                row("Trash", 0),
                row("Projects", 0),
                row("Web", 1),
                row("projects-old", 0),
            ]
        );
    }
}
