//! Local SQLite cache of folders, message headers and bodies, plus the queue of
//! changes that still have to be applied on the server.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

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

impl Folder {
    /// Last path segment, e.g. "Work" for "INBOX/Work".
    pub fn display_name(&self) -> String {
        if self.role == Role::Inbox {
            return "Inbox".into();
        }
        match &self.delimiter {
            Some(d) if !d.is_empty() => self.name.rsplit(d.as_str()).next().unwrap_or(&self.name).to_string(),
            _ => self.name.clone(),
        }
    }

    pub fn depth(&self) -> usize {
        match &self.delimiter {
            Some(d) if !d.is_empty() && self.role == Role::Other => self.name.matches(d.as_str()).count(),
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingOp {
    SetSeen(bool),
    SetFlagged(bool),
    SetAnswered,
    Move(String),
    /// Mark \Deleted and expunge, for messages already in Trash.
    Expunge,
}

impl PendingOp {
    fn encode(&self) -> (&'static str, String) {
        match self {
            PendingOp::SetSeen(v) => ("seen", v.to_string()),
            PendingOp::SetFlagged(v) => ("flagged", v.to_string()),
            PendingOp::SetAnswered => ("answered", String::new()),
            PendingOp::Move(dest) => ("move", dest.clone()),
            PendingOp::Expunge => ("expunge", String::new()),
        }
    }

    fn decode(kind: &str, arg: String) -> Option<PendingOp> {
        Some(match kind {
            "seen" => PendingOp::SetSeen(arg == "true"),
            "flagged" => PendingOp::SetFlagged(arg == "true"),
            "answered" => PendingOp::SetAnswered,
            "move" => PendingOp::Move(arg),
            "expunge" => PendingOp::Expunge,
            _ => return None,
        })
    }
}

pub struct Store {
    conn: Connection,
}

const SUMMARY_COLUMNS: &str = "folder, uid, message_id, subject, from_name, from_addr, to_addrs, cc_addrs, \
     date, seen, flagged, answered, has_attachments, snippet, in_reply_to, refs";

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
    })
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

    fn init(conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
                 raw BLOB,
                 PRIMARY KEY (folder, uid)
             );
             CREATE INDEX IF NOT EXISTS messages_by_date ON messages(folder, date DESC);
             CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(subject, sender, body);
             CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
                 DELETE FROM messages_fts WHERE rowid = old.rowid;
             END;
             CREATE TABLE IF NOT EXISTS pending_ops (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 folder TEXT NOT NULL,
                 uid INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 arg TEXT NOT NULL DEFAULT ''
             );",
        )?;
        Ok(Store { conn })
    }

    // ---- folders ----

    /// Replaces the folder list with what the server reported. Folders that
    /// disappeared are deleted together with their cached messages.
    pub fn sync_folders(&mut self, folders: &[(String, Option<String>, Role)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut existing = tx.prepare("SELECT name FROM folders")?;
            let names: Vec<String> = existing.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for name in names {
                if !folders.iter().any(|(n, _, _)| *n == name) {
                    tx.execute("DELETE FROM messages WHERE folder = ?1", [&name])?;
                    tx.execute("DELETE FROM folders WHERE name = ?1", [&name])?;
                }
            }
            let mut upsert = tx.prepare(
                "INSERT INTO folders (name, delimiter, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET delimiter = excluded.delimiter, role = excluded.role",
            )?;
            for (name, delim, role) in folders {
                upsert.execute(params![name, delim, role.as_str()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.name, f.delimiter, f.role,
                    (SELECT COUNT(*) FROM messages m WHERE m.folder = f.name AND m.seen = 0),
                    (SELECT COUNT(*) FROM messages m WHERE m.folder = f.name)
             FROM folders f",
        )?;
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
        folders.sort_by(|a, b| a.role.rank().cmp(&b.role.rank()).then_with(|| a.name.cmp(&b.name)));
        Ok(folders)
    }

    pub fn folder_by_role(&self, role: Role) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT name FROM folders WHERE role = ?1 ORDER BY name LIMIT 1", [role.as_str()], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn folder_role(&self, folder: &str) -> Result<Role> {
        let role: Option<String> =
            self.conn.query_row("SELECT role FROM folders WHERE name = ?1", [folder], |r| r.get(0)).optional()?;
        Ok(role.map(|r| Role::parse(&r)).unwrap_or(Role::Other))
    }

    /// Records the server's UIDVALIDITY. Returns true if it changed, in which case
    /// all cached messages of the folder were dropped (their UIDs are meaningless now).
    pub fn check_uidvalidity(&mut self, folder: &str, uidvalidity: u32) -> Result<bool> {
        let current: Option<Option<i64>> = self
            .conn
            .query_row("SELECT uidvalidity FROM folders WHERE name = ?1", [folder], |r| r.get(0))
            .optional()?;
        let changed = matches!(current, Some(Some(v)) if v != uidvalidity as i64);
        if changed {
            self.conn.execute("DELETE FROM messages WHERE folder = ?1", [folder])?;
            self.conn.execute("DELETE FROM pending_ops WHERE folder = ?1", [folder])?;
        }
        self.conn.execute("UPDATE folders SET uidvalidity = ?2 WHERE name = ?1", params![folder, uidvalidity])?;
        Ok(changed)
    }

    // ---- messages ----

    pub fn max_uid(&self, folder: &str) -> Result<Option<u32>> {
        Ok(self.conn.query_row("SELECT MAX(uid) FROM messages WHERE folder = ?1", [folder], |r| r.get(0))?)
    }

    pub fn min_uid(&self, folder: &str) -> Result<Option<u32>> {
        Ok(self.conn.query_row("SELECT MIN(uid) FROM messages WHERE folder = ?1", [folder], |r| r.get(0))?)
    }

    pub fn uids(&self, folder: &str) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare("SELECT uid FROM messages WHERE folder = ?1")?;
        let uids = stmt.query_map([folder], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(uids)
    }

    /// Inserts new messages or refreshes flags of known ones. Locally cached
    /// bodies and snippets are kept.
    pub fn upsert_messages(&mut self, messages: &[MessageSummary]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut upsert = tx.prepare(&format!(
                "INSERT INTO messages ({SUMMARY_COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
                 ON CONFLICT(folder, uid) DO UPDATE SET
                     seen = excluded.seen, flagged = excluded.flagged, answered = excluded.answered
                 RETURNING rowid"
            ))?;
            let mut fts_exists = tx.prepare("SELECT 1 FROM messages_fts WHERE rowid = ?1")?;
            let mut fts_insert =
                tx.prepare("INSERT INTO messages_fts (rowid, subject, sender, body) VALUES (?1, ?2, ?3, '')")?;
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
                        m.references
                    ],
                    |r| r.get(0),
                )?;
                if !fts_exists.exists([rowid])? {
                    fts_insert.execute(params![rowid, m.subject, format!("{} {}", m.from_name, m.from_addr)])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Updates flags from the server, skipping messages with local changes not yet
    /// pushed (the server state is stale for those).
    pub fn update_flags(&mut self, folder: &str, flags: &[(u32, bool, bool, bool)]) -> Result<()> {
        let pending: std::collections::HashSet<u32> = {
            let mut stmt = self.conn.prepare("SELECT uid FROM pending_ops WHERE folder = ?1")?;
            stmt.query_map([folder], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET seen = ?3, flagged = ?4, answered = ?5 WHERE folder = ?1 AND uid = ?2",
            )?;
            for (uid, seen, flagged, answered) in flags {
                if !pending.contains(uid) {
                    stmt.execute(params![folder, uid, seen, flagged, answered])?;
                }
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
            "SELECT {SUMMARY_COLUMNS} FROM messages WHERE folder = ?1 ORDER BY date DESC, uid DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![folder, limit as i64], summary_from_row)?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Every cached message, for grouping into conversations.
    pub fn all_messages(&self) -> Result<Vec<MessageSummary>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM messages"))?;
        let rows = stmt.query_map([], summary_from_row)?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn message(&self, folder: &str, uid: u32) -> Result<Option<MessageSummary>> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM messages WHERE folder = ?1 AND uid = ?2"))?;
        Ok(stmt.query_row(params![folder, uid], summary_from_row).optional()?)
    }

    /// Full-text search over subject, sender and downloaded bodies across all folders.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MessageSummary>> {
        let fts_query = fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let columns = SUMMARY_COLUMNS.split(", ").map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {columns} FROM messages_fts f JOIN messages m ON m.rowid = f.rowid
             WHERE messages_fts MATCH ?1 ORDER BY m.date DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![fts_query, limit as i64], summary_from_row)?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn raw(&self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT raw FROM messages WHERE folder = ?1 AND uid = ?2", params![folder, uid], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// UIDs of the newest messages in a folder whose body has not been downloaded.
    pub fn uids_missing_body(&self, folder: &str, limit: usize) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT uid FROM messages WHERE folder = ?1 AND raw IS NULL ORDER BY date DESC LIMIT ?2",
        )?;
        let uids = stmt.query_map(params![folder, limit as i64], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(uids)
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
            tx.execute("UPDATE messages_fts SET body = ?2 WHERE rowid = ?1", params![rowid, body_text])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- local changes ----

    /// Applies a change to the cache immediately and queues it for the server.
    pub fn apply_local(&mut self, folder: &str, uid: u32, op: &PendingOp) -> Result<()> {
        match op {
            PendingOp::SetSeen(v) => {
                self.conn.execute("UPDATE messages SET seen = ?3 WHERE folder = ?1 AND uid = ?2", params![folder, uid, v])?;
            }
            PendingOp::SetFlagged(v) => {
                self.conn
                    .execute("UPDATE messages SET flagged = ?3 WHERE folder = ?1 AND uid = ?2", params![folder, uid, v])?;
            }
            PendingOp::SetAnswered => {
                self.conn.execute("UPDATE messages SET answered = 1 WHERE folder = ?1 AND uid = ?2", params![folder, uid])?;
            }
            // The destination copy shows up with its new UID on the next sync.
            PendingOp::Move(_) | PendingOp::Expunge => {
                self.conn.execute("DELETE FROM messages WHERE folder = ?1 AND uid = ?2", params![folder, uid])?;
            }
        }
        let (kind, arg) = op.encode();
        self.conn.execute(
            "INSERT INTO pending_ops (folder, uid, kind, arg) VALUES (?1, ?2, ?3, ?4)",
            params![folder, uid, kind, arg],
        )?;
        Ok(())
    }

    pub fn pending_ops(&self) -> Result<Vec<(i64, String, u32, PendingOp)>> {
        let mut stmt = self.conn.prepare("SELECT id, folder, uid, kind, arg FROM pending_ops ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?, r.get(4)?)))?
            .collect::<Result<Vec<(i64, String, u32, String, String)>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, folder, uid, kind, arg)| PendingOp::decode(&kind, arg).map(|op| (id, folder, uid, op)))
            .collect())
    }

    pub fn remove_pending_op(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", [id])?;
        Ok(())
    }
}

/// Turns user input into an FTS5 query: every word must match as a prefix.
fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric() || *c == '@' || *c == '.').collect::<String>())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .sync_folders(&[("INBOX".into(), Some("/".into()), Role::Inbox), ("Trash".into(), None, Role::Trash)])
            .unwrap();
        store
    }

    #[test]
    fn upsert_keeps_body_and_updates_flags() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "Hello"), msg(2, "World")]).unwrap();
        store.set_body("INBOX", 1, b"raw", "snip", "the body text", false).unwrap();

        let mut seen = msg(1, "Hello");
        seen.seen = true;
        store.upsert_messages(&[seen]).unwrap();

        let m = store.message("INBOX", 1).unwrap().unwrap();
        assert!(m.seen);
        assert_eq!(m.snippet, "snip");
        assert_eq!(store.raw("INBOX", 1).unwrap().as_deref(), Some(&b"raw"[..]));
        assert_eq!(store.messages("INBOX", 10).unwrap().iter().map(|m| m.uid).collect::<Vec<_>>(), vec![2, 1]);
        assert_eq!(store.folders().unwrap()[0].unread, 1);
    }

    #[test]
    fn search_matches_subject_sender_and_body() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "Quarterly report"), msg(2, "Lunch")]).unwrap();
        store.set_body("INBOX", 2, b"raw", "", "let's meet at the pizzeria", false).unwrap();

        assert_eq!(store.search("quart", 10).unwrap()[0].uid, 1);
        assert_eq!(store.search("pizzeria", 10).unwrap()[0].uid, 2);
        assert_eq!(store.search("alice", 10).unwrap().len(), 2);
        assert!(store.search("\"; DROP", 10).unwrap().is_empty());

        store.delete_messages("INBOX", &[2]).unwrap();
        assert!(store.search("pizzeria", 10).unwrap().is_empty());
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
    fn pending_ops_protect_local_flags() {
        let mut store = store_with_inbox();
        store.upsert_messages(&[msg(1, "a")]).unwrap();
        store.apply_local("INBOX", 1, &PendingOp::SetSeen(true)).unwrap();
        store.update_flags("INBOX", &[(1, false, false, false)]).unwrap();
        assert!(store.message("INBOX", 1).unwrap().unwrap().seen);

        let ops = store.pending_ops().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].3, PendingOp::SetSeen(true));
        store.remove_pending_op(ops[0].0).unwrap();

        store.apply_local("INBOX", 1, &PendingOp::Move("Trash".into())).unwrap();
        assert!(store.message("INBOX", 1).unwrap().is_none());
    }

    #[test]
    fn folders_sort_by_role() {
        let store = store_with_inbox();
        let names: Vec<_> = store.folders().unwrap().into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["INBOX", "Trash"]);
    }
}
