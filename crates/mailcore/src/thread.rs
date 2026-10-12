//! Conversations: messages linked through Message-ID, In-Reply-To and References.

use std::collections::HashMap;

use crate::store::MessageSummary;

/// A conversation number for each message; messages with equal numbers belong together.
/// Like JWZ threading without its subject fallback: a message joins every message it
/// names in In-Reply-To or References, also through ids of messages not in `messages`.
/// Gmail's conversations are Gmail's own (its thread id): such a message joins its thread,
/// and only messages naming it, never the ones it names, as Gmail splits a thread whose
/// subject changed.
pub fn conversations(messages: &[MessageSummary]) -> Vec<usize> {
    // Union-find: nodes 0..n are the messages, further nodes are Message-IDs and Gmail threads.
    let mut parent: Vec<usize> = (0..messages.len()).collect();
    let mut ids: HashMap<&str, usize> = HashMap::new();
    let mut threads: HashMap<u64, usize> = HashMap::new();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (i, m) in messages.iter().enumerate() {
        if m.thread != 0 {
            let node = *threads.entry(m.thread).or_insert_with(|| {
                parent.push(parent.len());
                parent.len() - 1
            });
            let (a, b) = (root(&mut parent, i), root(&mut parent, node));
            parent[a] = b;
        }
        let named: &[&String] = if m.thread != 0 { &[&m.message_id] } else { &[&m.message_id, &m.in_reply_to, &m.references] };
        let linked = named.iter().flat_map(|s| s.split_whitespace());
        for id in linked {
            let node = *ids.entry(id).or_insert_with(|| {
                parent.push(parent.len());
                parent.len() - 1
            });
            let (a, b) = (root(&mut parent, i), root(&mut parent, node));
            parent[a] = b;
        }
    }
    (0..messages.len()).map(|i| root(&mut parent, i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, in_reply_to: &str, references: &str) -> MessageSummary {
        MessageSummary {
            message_id: id.into(),
            in_reply_to: in_reply_to.into(),
            references: references.into(),
            ..Default::default()
        }
    }

    #[test]
    fn groups_replies_into_conversations() {
        let messages = [
            msg("<a>", "", ""),
            msg("<b>", "<a>", "<a>"),
            msg("<other>", "", ""),
            // Its parent <c> is not cached; References still name the root.
            msg("<d>", "<c>", "<a> <b> <c>"),
            // Only In-Reply-To, as some clients send.
            msg("<e>", "<d>", ""),
            msg("", "", ""),
            msg("", "", ""),
        ];
        let c = conversations(&messages);
        assert!(c[0] == c[1] && c[1] == c[3] && c[3] == c[4], "{c:?}");
        assert_ne!(c[0], c[2]);
        // Messages without any id stay on their own.
        assert!(c[5] != c[6] && c[5] != c[0], "{c:?}");
    }

    #[test]
    fn gmail_threads_are_gmails() {
        let gmail = |id: &str, in_reply_to: &str, thread| MessageSummary { thread, ..msg(id, in_reply_to, in_reply_to) };
        let messages = [
            gmail("<a>", "", 1),
            // Gmail started a new conversation (new subject) although it answers <a>.
            gmail("<b>", "<a>", 2),
            // Same thread without naming anything.
            gmail("<c>", "", 1),
            // Cached before thread ids were kept: still joins what it answers.
            msg("<d>", "<b>", "<b>"),
        ];
        let c = conversations(&messages);
        assert_eq!(c[0], c[2]);
        assert_ne!(c[0], c[1]);
        assert_eq!(c[1], c[3]);
    }
}
