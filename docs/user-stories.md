# User stories

![Use-case diagram](use-cases.svg)

Source: [use-cases.puml](use-cases.puml) (render with `java -jar plantuml.jar -tsvg docs/use-cases.puml`).

Each story has acceptance criteria (AC). The desktop suite
`crates/app/src/story_tests.rs` checks the ones marked **auto** against a local
GreenMail server; the rest are checked by hand. Results: [Test results](#test-results).

| ID | Story | Acceptance criteria |
|---|---|---|
| US1 | As a user I set up my account so the app can reach my mail. | Server settings are guessed from the address (**auto**). Login is verified before anything is saved (**auto**). The password goes to the system keyring (Android: Keystore), not to a file. |
| US2 | As a user I browse folders and messages. | Folders appear with the inbox first (**auto**). The inbox lists messages newest first with sender, subject, preview and unread marker (**auto**). On phones one pane is shown at a time. |
| US3 | As a user I read a message. | Opening shows subject, sender, recipients, date and body (**auto**), and marks it read (**auto**). HTML is shown without running scripts (desktop: Servo; Android: as text for now). |
| US4 | As a user I load remote images only when I choose to. | Remote images are blocked and a banner offers loading them; after "Load images" they show (desktop only). |
| US5 | As a user I mark messages read/unread and flag them. | The change shows immediately (**auto**) and reaches the server (**auto**). |
| US6 | As a user I archive or delete a message. | It leaves the list immediately and the next message is selected (**auto**); on the server it is in Archive or Trash (**auto**). |
| US7 | As a user I search my mail. | Typing filters across folders by subject, sender and downloaded text (**auto**); clearing the search restores the folder. |
| US8 | As a user I write and send a message. | A compose form opens (**auto**); sending delivers it (**auto**) and files a copy in Sent (**auto**); errors keep the draft open with a message. |
| US9 | As a user I reply, reply all or forward. | Recipients, `Re:`/`Fwd:` subject and quoted text are prefilled (**auto**); the original is marked answered (**auto**); forwarding keeps attachments. |
| US10 | As a user I see new mail without refreshing. | Mail arriving while the app is open appears within seconds (**auto**). |
| US11 | As a user I keep working offline. | Flags and moves made offline apply locally and reach the server after reconnecting. |
| US12 | As a user I attach files and save attachments. | File pickers open; attachments are sent and saved correctly (desktop only for now). |
| US13 | As a user I open a `.eml` file. | `mailclient file.eml` shows the message without an account (desktop). |
| US14 | As a user I follow links safely. | Clicking a link opens the system browser; `mailto:` opens the composer; nothing navigates inside the message view (desktop). |
| US15 | As a user I read the mail of several accounts in one place. | Further accounts are added from the sidebar; the same address is refused and Cancel returns to the mail (**auto**). "All inboxes" lists every account's inbox newest first, each message with its account's color bar (**auto**). Replies go out through the message's account (**auto**); a folder shows only its account's mail (**auto**). |
| US16 | As a user I read a conversation as a whole. | Replies and the messages they answer share one list row with a message count, also when one side is in Sent (**auto**). The open conversation unfolds in the list as a timeline; the reader shows all its messages oldest first, marks the open one and starts with the newest; any of them can be opened from the timeline (**auto**). Opening reads the whole conversation; archive and delete act on its messages in the listed folder (**auto**). |

## Test results

Run of 2026-10-09 against GreenMail 2.1.14. Desktop: `story_tests.rs` on Slint's headless
testing backend (plain-text view, `--no-default-features`), plus window snapshots for the
Servo view. Android: emulator (Pixel 7, Android 15, x86_64), driven with adb taps; server
state checked over IMAP after each step.

| Story | Desktop | Android | Notes |
|---|---|---|---|
| US1 Set up account | pass (auto) | pass | Wrong port shows the error and saves nothing; password in Secret Service / Android Keystore. |
| US2 Browse | pass (auto) | pass | Phones: one pane at a time, Folders button, system Back. |
| US3 Read | pass (auto) | pass | Read flag reaches the server. HTML: Servo on desktop, text on Android. |
| US4 Remote images | pass (manual) | n/a | Desktop only (Servo). Blocked by default, banner shown. |
| US5 Unread / flag | pass (auto) | pass | Both reach the server. |
| US6 Archive / delete | pass (auto) | pass | Land in Archive / Trash on the server. |
| US7 Search | pass (auto) | pass | |
| US8 Write and send | pass (auto) | pass | Copy in Sent; second draft refused while writing; Back discards. |
| US9 Reply | pass (auto) | pass | Prefilled; original marked answered. Forward not covered by a story test. |
| US10 New mail | pass (auto) | pass | Arrives via IDLE without a manual sync. |
| US11 Offline | partial (auto) | partial | Covered: the server dropping an idle connection. Not covered: working with no network, then reconnecting. |
| US12 Attachments | not tested | not supported | Desktop uses native file pickers (not automatable here); Android has no picker yet. |
| US13 Open .eml | pass (manual) | n/a | |
| US14 Follow links | not tested | not supported | Needs a real click in the Servo view; Android cannot open links yet. |
| US15 Multiple accounts | pass (auto) | not tested | Two GreenMail accounts. |
| US16 Conversations | pass (auto) | not tested | Conversation between the two GreenMail accounts. |

Bugs found by this run and fixed:

- A connection the server had dropped returned empty results instead of an error, and
  sync took that as "all messages deleted", emptying the local cache (async-imap 0.12
  ends response streams silently on EOF). Every IMAP read is now confirmed with a NOOP,
  and the engine reconnects and retries once.
- Setup guessed the name from the first typed character and never updated it.
- Android: a second window (compose) cannot be shown, so composing moved into the main
  window on both platforms; system Back closed the app from any screen; compose fields
  sat under the status bar; symbols (☰ ⟳ ✎ ↩ ☆) are missing from Android's fonts.
