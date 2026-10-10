# Feature ideas

Features users expect from desktop and mobile mail clients, from an online search
(2026-10-09; sources at the end), checked against what this client already does
([README](../README.md#features), [user stories](user-stories.md)).

Status: **done** · **partial** · **missing**.

## Baseline: missing ones make a client feel broken

| Feature | Status | Notes |
|---|---|---|
| Fast, reliable search | done | SQLite FTS5 over subject, sender, downloaded bodies (US7). Top request in the Jolla poll. |
| Offline local store that never needs manual cleanup | done | SQLite cache, offline queue (US11; reconnect after no network not yet tested). |
| Push for new mail | partial | IMAP IDLE on the inbox while the app runs (US10). Mobile users want real push notifications with the app in the background, with actions (archive, mark read) on the notification. |
| Read, flag, archive, delete | done | US5, US6. |
| Modern sign-in (OAuth) | partial | Microsoft OAuth written, untested ([oauth-microsoft.md](oauth-microsoft.md)). Gmail OAuth missing. |
| Unified inbox across accounts | missing | Most common item in every 2026 guide. Needs multi-account support first. |
| Fast account switching | missing | K-9 wish list: switch like browser tabs, unread/starred counts per account. |
| Threaded conversation view | done | On the Thunderbird for Android roadmap; high in the Jolla poll. |
| Bulk select and actions | missing | Mark or delete many messages at once. |
| Rules / filters, labels | missing | |
| Customisable swipe actions (mobile) | missing | Archive, delete, snooze, flag. |

## Frequently requested

| Feature | Status | Notes |
|---|---|---|
| Remote images and tracking pixels blocked by default | done | Desktop, per-message "Load images" (US4). Android shows text only. |
| Scheduled / delayed send | missing | Thunderbird top-20 voted idea. Needs the app running at send time, or server support. |
| Undo send | missing | Cheap version: hold outgoing mail in the queue for a few seconds. |
| Snooze | missing | Often cited as missing from Gmail; Mozilla Connect request. Needs a local timer plus a folder or keyword on the server. |
| Rich (HTML) compose and signatures | missing | Compose is plain text. Signatures missing entirely. |
| Attachments: save all, shown at the top of long threads | partial | Single attachments can be saved; Android has no file picker (US12). |
| Raw source and full headers | missing | Power-user request (Vivaldi wish list). |
| Contacts: add sender, autocomplete, vCard import/export, CardDAV | missing | "Add sender to contacts" was #2 in the Jolla poll. |
| Calendar (CalDAV), invites | missing | Users are split on built-in vs separate app. |
| PGP / S/MIME | missing | Expected by technical users. |
| Per-account colours | missing | Thunderbird top-20. |
| Desktop integration (GNOME notifications, tray, default mail handler) | partial | `mailto:` handled inside the app; system integration missing. |
| JMAP, IMAP4rev2, SMTPUTF8 | missing | Recurring in Mozilla Connect top-voted lists. |
| AI triage / summaries | missing | Selling point of Spark, Superhuman, Outlook Copilot. Conflicts with the local-first, privacy-minded audience; low priority. |

## Suggested next steps

The biggest gaps among baseline features, roughly in order of user impact per effort:

1. ~~Threaded view~~ (done).
2. Bulk select and actions: reuses the existing single-message actions.
3. ~~Multiple accounts, then the unified inbox~~ (done).
4. ~~Notifications for new mail (desktop and Android)~~ (done while the app runs; Android
   background push still missing).
5. Undo send, then snooze and scheduled send.

## Caveats

Most current sources are vendor-written listicles (many from Mailbird). The user-voted
lists are older: Thunderbird top-20 from April 2023, Jolla poll from 2014. No 2025–26
Reddit or Hacker News discussion was found.

## Sources

- [Mailbird: Best desktop email clients 2026](https://www.getmailbird.com/best-desktop-email-clients-2026-windows-and-mac-comparison/)
- [Mailbird: Managing multiple accounts](https://www.getmailbird.com/best-desktop-email-client-for-managing-multiple-email-accounts-in-2026-what-to-look-for/)
- [Mailbird: AI email clients](https://www.getmailbird.com/best-ai-email-client-in-2026-desktop-apps-with-built-in-ai-compared/)
- [Leave Me Alone: Best iPhone email apps 2026](https://leavemealone.com/blog/best-email-apps-for-iphone/amp/)
- [Alphonso Labs: Best Android email apps 2026](https://www.alphonsolabs.com/best-email-apps-android-2026/amp/)
- [Thunderbird blog: Top 20 ideas](https://blog.thunderbird.net/?p=1173)
- [Mozilla Connect: weekly top-voted recap](https://connect.mozilla.org/t5/discussions/mozilla-connect-weekly-recap-top-voted-ideas-5-8-5-15/m-p/126074/highlight/true)
- [Mozilla Connect: Thunderbird Android ideas](https://connect.mozilla.org/t5/ideas/idb-p/ideas/label-name/thunderbird%20android/tab/most-kudoed)
- [Thunderbird for Android roadmap](https://github.com/thunderbird/developer-docs/blob/master/planning/android-roadmap.md)
- [K-9 Mail forum: 2025 wish list](https://forum.k9mail.app/t/ui-and-feature-wish-list-for-2025/12664)
- [Jolla email poll](https://together.jolla.com/question/62166/poll-results-native-email-improvements-for-jolla-to-consider)
- [Vivaldi mail wish list](https://forum.vivaldi.net/post/23843)
- [Hacker News discussion](https://news.ycombinator.com/item?id=4971804)
