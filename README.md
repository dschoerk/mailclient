# mailclient

A desktop email client in Rust with a [Slint](https://slint.dev) UI. HTML mail is
rendered by an embedded [Servo](https://servo.org) engine with scripting disabled.

Status: **v0.1**: one IMAP/SMTP account, Linux desktop; Android in progress.

## Features

- Account setup with server guesses for common providers; login verified before saving.
  Password stored in the system keyring (Secret Service).
- Multiple accounts ("Add account" in the sidebar), each with its own sync engine and cache,
  and an "All inboxes" view; a colored bar in the message list marks each account.
- Conversations: the list shows one row per conversation (by `References`/`In-Reply-To`),
  the open one unfolds into a timeline of its messages, and the reader shows them all
  one below the other, including your replies from Sent.
- Folder list with special-use detection, unread counts.
- Local SQLite cache: the last 500 messages per synced folder; bodies downloaded on
  open plus the 40 newest per sync. Works offline from the cache.
- Push via IMAP IDLE on the inbox, polling every 5 minutes as a fallback; a system
  notification for new unread mail while the app runs (Linux desktop and Android).
- Read, flag, mark read/unread, archive, delete, also for several conversations at once
  (tick them at the row's left edge, by long press on phones, or with `x`). Changes apply instantly and are
  queued for the server, so they also work offline.
- Full-text search (SQLite FTS5) over subject, sender and downloaded bodies; `from:address`
  limits it to one sender, and clicking the sender of an open message lists all of their mail.
- Compose, reply, reply all and forward (including attachments) as plain text, with
  file attachments. Sent mail is filed to the Sent folder (Gmail does this itself).
- Drafts: closing the composer (or Back on Android) saves to Drafts, plus an autosave every
  15 s; reopen with "Edit draft". Sending or discarding removes the draft.
- Save attachments; `mailto:` links open the composer; other links open in the
  system browser.
- Keyboard: `j`/`k` next/previous, `r` reply, `a` reply all, `f` forward, `e` archive,
  `#`/Delete delete, `u` read/unread, `s` flag, `x` tick for bulk actions, `c` compose, `/` search, Esc clear search,
  Ctrl+Enter send.
- Open a message file without an account: `mailclient message.eml`.

## Security model for HTML mail

HTML is untrusted. Three independent layers keep it inert:

1. **Sanitizing** (`mailcore::mime::sanitize_html`, ammonia) removes scripts, event
   handlers, `javascript:` URLs, forms, frames, `<base>` and `target`, and rewrites
   `cid:` references.
2. **CSP sandbox**: each message is served from memory under `https://message.invalid`
   with `Content-Security-Policy: sandbox; default-src 'none'; …`. A `sandbox` without
   `allow-scripts` disables scripting in Servo for the whole document.
3. **Request interception** (`webview/delegate.rs`): every fetch goes through
   `WebViewDelegate::load_web_resource`. Only message content and inline parts are
   served; remote loads are cancelled unless the user clicks "Load images" for that
   message. Navigation inside the view is always denied; popups are never created.

Servo itself runs with the JIT, WebAssembly, WebGL, WebGPU and service workers disabled.

Layers 2 and 3 were checked by feeding Servo the *unsanitized* test message
(inline `<script>`, `onload`, remote pixel, iframe): no script ran and all remote
loads were blocked.

## Building

Requirements: Rust 1.88+, clang, python3, cmake, plus the usual X11/Wayland, fontconfig,
freetype and Vulkan/GL development packages (Servo builds SpiderMonkey from source).
The first build takes a while; the Servo/SpiderMonkey dependencies are compiled with
optimizations even in debug builds.

```sh
cargo run                                   # with Servo HTML view
cargo run --no-default-features             # HTML shown as plain text, much faster build
cargo test --workspace
```

### Android

Builds without Servo: HTML mail is shown in the system WebView instead, with the same rules
(scripts off, content served from memory under a CSP, remote loads blocked until "Load images").
Its Java side (`crates/app/java/`) is compiled to dex by `build.rs`, which needs `JAVA_HOME` and
`ANDROID_HOME`, and loaded at runtime, so the APK stays a NativeActivity. Toolchain: JDK 21, Android SDK with
`platforms;android-35`, `build-tools;35.0.0`, `ndk;28.2.13676358`, Rust targets
`x86_64-linux-android`/`aarch64-linux-android`, and `cargo install cargo-apk`.

`scripts/run-android.sh` does all of the following: it starts the emulator (AVD `mailclient`) if
no phone is connected, builds for the device's architecture, installs, sets up access to the
local test server when there is one, and launches the app. By hand:

```sh
export JAVA_HOME=~/Android/jdk ANDROID_HOME=~/Android/Sdk ANDROID_NDK_ROOT=~/Android/Sdk/ndk/28.2.13676358 PATH=~/Android/jdk/bin:$PATH
cargo apk build -p mailclient --lib --no-default-features --target x86_64-linux-android   # emulator
cargo apk build -p mailclient --lib --no-default-features --target aarch64-linux-android  # phone
adb install -r target/debug/apk/mailclient_app.apk
```

Against the local test server from the emulator: `adb reverse tcp:3993 tcp:3993 && adb reverse tcp:3465 tcp:3465`,
copy the test CA into the app (debug builds only):
`adb push target/test-mailserver/ca.pem /data/local/tmp/ && adb shell run-as dev.mailclient.app cp /data/local/tmp/ca.pem files/extra-ca.pem`,
then set up `corpus@localhost` with IMAP `localhost:3993`, SMTP `localhost:3465`.
If the emulator crashes in its GL translator, start it with `-gpu swiftshader_indirect`.

Not yet on Android: attaching/saving files, push while the app is in
the background.

Outlook.com and Microsoft 365 need OAuth sign-in, which requires a one-time Microsoft app
registration: see [docs/oauth-microsoft.md](docs/oauth-microsoft.md).
Gmail signs in the same way with a Google OAuth client: see [docs/oauth-google.md](docs/oauth-google.md).

Slint is pinned to a git revision: Slint 1.18 on crates.io conflicts with Servo 0.7's
ICU dependency, and 1.17 lacks the wgpu 30 integration. `rusqlite` is pinned to 0.38
because Servo links the same SQLite.

End-to-end test against a local [GreenMail](https://greenmail-mail-test.github.io/greenmail/)
server in Docker (creates a test CA and a `localhost` certificate under `target/test-mailserver/`):

```sh
scripts/test-mailserver.sh                                  # start (stop: scripts/test-mailserver.sh stop)
cargo test -p mailcore --test greenmail -- --ignored        # send, sync, flag, delete, IDLE
MAILCLIENT_EXTRA_CA=target/test-mailserver/ca.pem cargo run # GUI: imap/smtp host localhost, ports 3993/3465, any password
```

Ready-made configs for GreenMail are in [config/](config/); fill their mailboxes with
`scripts/seed-greenmail.sh` (a few messages and a conversation between work and home) and
pick one with `--config`:

```sh
scripts/seed-greenmail.sh
MAILCLIENT_PASSWORD=x MAILCLIENT_EXTRA_CA=target/test-mailserver/ca.pem \
  cargo run -- --config config/greenmail-two-accounts.toml
```

User stories ([docs/user-stories.md](docs/user-stories.md), UML use-case diagram and results)
driven through the real UI on Slint's headless testing backend, against GreenMail:

```sh
cargo test -p mailclient --no-default-features --lib story_tests -- --ignored --nocapture
```

### End-to-end tests

`crates/e2e` drives the real app from outside, on the desktop or on Android, through
Slint's MCP server (enabled with `--features slint/mcp`; inactive unless `SLINT_MCP_PORT`
is set). Elements are found by accessible label and role. Mail bodies are rendered by Servo,
outside Slint's element tree, so only screenshots (`target/e2e/*.png`) show them.

```sh
scripts/test-mailserver.sh
# desktop
cargo build -p mailclient --features slint/mcp
cargo test -p e2e -- --ignored --test-threads=1   # one at a time: several Servo instances at once time out, and they share the clipboard
# Android: running emulator or phone, debug APK with the MCP server installed
cargo apk build -p mailclient --lib --no-default-features --features slint/mcp --target x86_64-linux-android
adb install -r target/debug/apk/mailclient_app.apk
E2E_TARGET=android cargo test -p e2e -- --ignored --test-threads=1
```

On Android the test wipes the app's data, copies the test CA and the MCP port into its
storage (`files/mcp-port`), and sets up `adb reverse`/`adb forward`.

Real-world mail corpus (downloaded into `target/testdata/`, never committed): the
SpamAssassin public corpus ham folders (4,150 real messages, 2002–2003) and MIT-licensed
HTML templates (Postmark, Cerberus, Lee Munroe) for modern layouts:

```sh
scripts/fetch-testdata.sh
cargo test -p mailcore --test corpus -- --ignored corpus_parses     # parse + sanitize all
cargo test -p mailcore --test corpus -- --ignored load_into_greenmail # browse as corpus@localhost (~9 min)
```

Debugging aids:

- `MAILCLIENT_PASSWORD=…` overrides the keyring.
- `MAILCLIENT_EXTRA_CA=ca.pem` trusts one extra CA (local test server, self-hosted servers).
- `MAILCLIENT_SNAPSHOT=out.png` saves a picture of the window after 6 s and exits.
- `RUST_LOG=mailcore=debug,mailclient_app=debug` for sync and request-blocking logs.

Data lives in `~/.config/mailclient/config.toml` (or the file given with `--config`) and `~/.local/share/mailclient/<email>.sqlite`.

Icons: [Lucide](https://lucide.dev) 1.47.0 (ISC; some derived from Feather, MIT), embedded
as SVG from `crates/app/ui/icons/` (license in that folder). SVGs instead of font symbols
because Android's fonts lack most of them.

## Layout

```
crates/mailcore   UI-independent backend
  config.rs       account settings, keyring
  store.rs        SQLite cache, FTS index, queue of pending changes
  imap.rs         IMAP session (async-imap, rustls)
  smtp.rs         sending (lettre)
  mime.rs         parsing, sanitizing, rendering, composing, reply templates
  engine.rs       background sync thread: commands in, events out; IDLE loop
crates/app        Slint UI
  ui/app.slint    main window, setup form, compose window
  src/app.rs      controller: wires UI, store and engine
  src/compose.rs  compose windows
  src/webview/    Servo integration, adapted from Slint's examples/servo (MIT)
```

## Known limitations / next steps

- Tested against GreenMail only, not yet against Gmail/Fastmail/Dovecot. The keyring
  path is untested.
- No OAuth2 (needed for Outlook.com, optional for Gmail). Accounts cannot be removed in the app yet
  (edit `config.toml`); new messages from "All inboxes" go out from the first account.
- No drafts saving, contacts autocomplete, or rich-text compose.
- Only the inbox, Sent and the open folder are synced; other folders sync when opened.
- Conversations are grouped by headers only, not by subject, and per account.
- No CONDSTORE/QRESYNC yet; flag changes are found by re-fetching flags of cached
  messages (fine for the 500-message window).
- Windows/macOS: the Servo rendering path from the Slint example is kept but untested,
  and the platform-specific dependencies are not wired up in `Cargo.toml`.
