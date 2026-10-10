# Features

What the mail client can do today. For ideas that are not built yet see
[feature-ideas.md](feature-ideas.md); for how the features are tested see
[user-stories.md](user-stories.md).

## Platforms

- **Linux desktop**: the main target, with HTML mail rendered by Servo.
- **Android**: works. HTML mail is shown in the system WebView (in a window of its own over
  the reader, with the security rules below). Files are attached and saved through the system
  file pickers. New mail keeps arriving in the background (see "New mail" below).
- **Windows/macOS**: not supported yet. The Servo rendering code is there but untested.
- `--no-default-features` builds without Servo. HTML is then shown as plain text and the
  build is much faster.

## Accounts

- **Setup form**: enter an email address and the client guesses the servers.

  | Provider | IMAP | SMTP | Sign-in |
  |---|---|---|---|
  | Gmail (`gmail.com`, `googlemail.com`) | `imap.gmail.com` | `smtp.gmail.com:465` TLS | app password |
  | Outlook.com, Hotmail, Live, MSN | `outlook.office365.com` | `smtp.office365.com:587` STARTTLS | Microsoft OAuth |
  | Fastmail | `imap.fastmail.com` | `smtp.fastmail.com:465` TLS | app password |
  | iCloud (`icloud.com`, `me.com`) | `imap.mail.me.com` | `smtp.mail.me.com:587` STARTTLS | app password |
  | GMX (`gmx.net`, `.at`, `.de`) | `imap.gmx.net` | `mail.gmx.net:587` STARTTLS | password |
  | Any other domain | `imap.<domain>` | `smtp.<domain>:465` TLS | password |

  You can edit every guess, and choose STARTTLS instead of TLS for IMAP and for SMTP
  separately. The login is checked before the account is saved.
- **Passwords** are kept in the system keyring: Secret Service on Linux, the Android
  keystore on Android.
- **Microsoft sign-in** (Outlook.com, Microsoft 365): you sign in through your browser
  with OAuth2, and the client then uses XOAUTH2 for IMAP and SMTP. This needs a one-time
  app registration: see [oauth-microsoft.md](oauth-microsoft.md).
- **More than one account**: use "Add account" in the side panel. Each account has its
  own sync and its own cache.
- **All inboxes**: one list with the inboxes of all accounts. A colored bar on each row
  shows which account the message belongs to.

## Folders

- Lists every folder, with subfolders indented.
- Recognizes special folders (Inbox, Sent, Drafts, Trash, Junk, Archive, All Mail,
  Flagged) and shows them with an icon.
- Shows unread counts, except for Sent, Drafts, Trash and Junk.

## Sync and offline use

- **Local cache** in SQLite (`~/.local/share/mailclient/<email>.sqlite`): headers of the
  newest 500 messages in each synced folder.
- **Message bodies** are downloaded when you open a message. The newest 40 are also
  downloaded on each sync, so they can be read offline.
- **Which folders sync**: Inbox, Sent and the folder you have open. Other folders sync
  when you open them.
- **New mail** is pushed on the inbox through IMAP IDLE. If that fails, the client checks
  every 5 minutes. The "Sync" button checks right away.
- **Push on Android in the background**: a foreground service ("Waiting for new mail", a
  silent notification in the shade) keeps the app running, and IDLE with it. Back on the
  main screen sends the app to the background instead of closing it. Swiping it away from
  Recents stops push until the app is opened again. Battery savers and Doze can still
  delay mail.
- **Notifications**: new unread mail arriving in an inbox while the app runs shows a system
  notification (sender and subject; a count for several). Linux desktops get it through the
  freedesktop notification service; Android asks for the permission on first start, and
  tapping the notification opens the app. Mail already there at setup is not announced.
- **Works offline**: browsing, reading cached mail and searching all work without a
  connection.
- **Changes apply at once**: read/unread, flag, archive and delete show immediately and
  are queued for the server. The queue is sent when the connection comes back.

## Message list and conversations

- **One row per conversation**: messages are grouped by their `References` and
  `In-Reply-To` headers. Each row shows the sender, subject, a preview of the text, the
  date, and icons for unread, flagged, answered and attachments.
- **Mini timeline**: conversation rows show one dot per message (the newest 12), filled
  while unread.
- **Open a conversation** to see all its messages in a timeline. Your own replies from
  the Sent folder are included.
- **The reader** shows the whole conversation, one message below the other.
- **On phones** the timeline sits at the top of the reader; tap an entry to jump to that
  message. The reader scrolls to the open message and marks it.
- **Opening a message** marks it as read.

## Reading mail

- **Headers**: From, To, Cc and date.
- **HTML mail** is rendered by an embedded Servo engine. Plain-text mail is shown as
  text, with links you can click.
- **Remote images** are blocked by default. A banner offers "Load images" for the open
  message.
- **Links**: `mailto:` links open the composer, and other links open in your web browser.
- **Copying text**: select text in a message and copy it: drag with the mouse and Ctrl+C on
  the desktop; long press, then Copy, on Android.
- **Attachments** are listed with their name and size, and can be saved to disk.
- **Open a single file**: `mailclient message.eml` opens a message file. No account is
  needed.

### Safety of HTML mail

HTML mail is treated as untrusted. Three separate layers keep it harmless:

1. **Cleaning**: scripts, event handlers, `javascript:` links, forms, frames, `<base>` and
   `target` are removed. Inline (`cid:`) images are rewritten so they still show.
2. **Content Security Policy sandbox**: scripts cannot run, and nothing loads unless the
   policy allows it.
3. **Request blocking**: the client checks every request the page makes. It only serves
   the message and its inline parts, and drops remote loads until you click "Load
   images". The page can never navigate away or open popups.

Servo also runs with JIT, WebAssembly, WebGL, WebGPU and service workers turned off.

## Actions

- Reply, reply all, forward (with the attachments), archive and delete.
- Mark as read or unread, and flag or unflag.
- **Several at once**: tick conversations with the circle at the left edge of a row (on
  phones also with a long press; while ticking, a tap ticks too), or with `x` on the
  current row. A bar above the list then shows the count and acts on all ticked rows:
  read/unread, flag/unflag, archive, delete, and clear (also Esc, or Back on Android).
- **Swipe** (phones): a list row swiped right is archived, swiped left deleted. A swipe
  shorter than about a third of the row springs back and does nothing.
- **Archive** moves the message to the Archive folder. If there is none, it uses All Mail,
  as on Gmail.
- **Delete** moves the message to Trash. If the message is already in Trash, or the
  account has no Trash folder, it is deleted for good.

## Writing mail

- **New message**, reply, reply all and forward. Replies quote the original message and
  set the threading headers. Messages you answer get the "answered" flag.
- **Fields**: To, Cc, Bcc and Subject. The body is plain text.
- **Attachments**: add files with "Attach…" and remove them again before sending.
- **Drafts**: leaving the composer (Close, or Back on Android) keeps the message in the
  Drafts folder, which is created if the account has none. Changes are also saved every
  15 seconds while you write. Open a draft from Drafts and click "Edit draft" to continue;
  each save replaces the previous copy. Sending removes the draft, and Discard deletes it.
- **Sending** uses SMTP. A copy of the sent message goes to the Sent folder (Gmail does
  this itself). If sending fails, the message stays in the compose window so you can try
  again.
- Messages written from "All inboxes" are sent from the first account.

## Search

- Full-text search (SQLite FTS5) over subject, sender and every downloaded message body.
- Searches the local cache, so it also works offline.
- **Mail from one sender**: click the sender's name in an open message ("More from this
  sender") to list all of their mail, in every folder and account. This searches for
  `from:address`, which you can also type, alone or with other words
  (`from:bob@example.org invoice`). It matches the sender's address exactly, so mail that
  only mentions the address is left out.

## Keyboard shortcuts

| Key | Action |
|---|---|
| `j` / ↓ | Next message |
| `k` / ↑ | Previous message |
| `c` | New message |
| `r` | Reply |
| `a` | Reply all |
| `f` | Forward |
| `e` | Archive |
| `#` / Delete | Delete |
| `u` | Mark read / unread |
| `s` | Flag / unflag |
| `x` | Tick the current conversation; `e`, `#`, `u`, `s` then act on all ticked ones |
| `/` | Search |
| Esc | Clear the search |
| Ctrl+Enter | Send (in the compose window) |
| Back (Android) | Close the message or the folder panel |

## Phone layout

- On windows narrower than 700 px, the folders, the list and the reader each fill the
  whole screen, one at a time. The folders open as a side panel.
- **Action buttons** for Delete, Archive and Next, either as a bar at the bottom or as
  floating buttons ("Floating buttons" setting).
- **Left-handed** setting: mirrors the buttons so they are easy to reach with the left
  thumb.
- Respects the phone's safe areas (notch, system bars).

## Configuration and debugging

- **Settings** are kept in `~/.config/mailclient/config.toml`. Use `--config <file>` to
  load a different file. Ready-made test configs are in [`config/`](../config/).
- **Environment variables**:
  - `MAILCLIENT_PASSWORD`: password to use instead of the keyring one.
  - `MAILCLIENT_EXTRA_CA`: one extra CA certificate to trust, for self-hosted or test
    servers.
  - `MAILCLIENT_SNAPSHOT=out.png`: saves a picture of the window after 6 seconds, then
    quits.
  - `RUST_LOG`: log level, for example `mailcore=debug,mailclient_app=debug`.
- **End-to-end tests** can drive the app through Slint's MCP server (`slint/mcp` feature).

## Not supported yet

- Removing an account from inside the app. For now, edit `config.toml`.
- Address autocomplete, and rich-text (HTML) writing.
- Grouping conversations by subject, or across accounts.
- CONDSTORE/QRESYNC. Flag changes are found by fetching the flags of cached messages
  again.
