//! User stories (docs/user-stories.md) driven through the real UI, controller and sync
//! engine on Slint's headless testing backend, against a local GreenMail server:
//!   scripts/test-mailserver.sh
//!   cargo test -p mailclient --no-default-features --lib story_tests -- --ignored --nocapture

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use mailcore::config;
use mailcore::imap::Imap;
use mailcore::mime::{self, Draft};
use mailcore::{AccountConfig, Security, MessageSummary};
use slint::{ComponentHandle, Model};

use crate::{AppWindow, ReaderPart};
use crate::app::Controller;

type Outcome = Result<(), String>;

async fn sleep(ms: u64) {
    let (tx, rx) = futures::channel::oneshot::channel();
    slint::Timer::single_shot(Duration::from_millis(ms), move || {
        let _ = tx.send(());
    });
    let _ = rx.await;
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) -> Outcome {
    for _ in 0..150 {
        if cond() {
            return Ok(());
        }
        sleep(200).await;
    }
    Err(format!("timed out waiting for {what}"))
}

/// Labels repeat between plain text and controls ("Email" label and field), so match the role too.
fn element(c: &impl ComponentHandle, label: &str, roles: &[AccessibleRole]) -> Result<ElementHandle, String> {
    ElementHandle::find_by_accessible_label(c, label)
        .find(|e| e.accessible_role().is_some_and(|r| roles.contains(&r)))
        .ok_or_else(|| format!("no {roles:?} labelled {label:?}"))
}

fn click(c: &impl ComponentHandle, label: &str) -> Outcome {
    element(c, label, &[AccessibleRole::Button, AccessibleRole::ListItem])?.invoke_accessible_default_action();
    Ok(())
}

fn type_in(c: &impl ComponentHandle, label: &str, text: &str) -> Outcome {
    element(c, label, &[AccessibleRole::TextInput])?.set_accessible_value(text);
    Ok(())
}

fn subjects(w: &AppWindow) -> Vec<String> {
    w.get_messages().iter().map(|m| m.subject.to_string()).collect()
}

fn ensure(cond: bool, msg: impl Into<String>) -> Outcome {
    if cond { Ok(()) } else { Err(msg.into()) }
}

/// Server-side view of a folder, fetched on a separate thread (blocking is fine in tests).
fn server(account: &AccountConfig, folder: &str) -> Vec<MessageSummary> {
    let (account, folder) = (account.clone(), folder.to_string());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut imap = Imap::connect(&account, "x").await.unwrap();
            let messages = imap.fetch_new(&folder, 0).await.unwrap_or_default(); // folder may not exist yet
            imap.logout().await;
            messages
        })
    })
    .join()
    .unwrap()
}

/// Plain-text bodies of the messages with this subject in a server folder.
fn server_texts(account: &AccountConfig, folder: &str, subject: &str) -> Vec<String> {
    server_bodies(account, folder, subject).into_iter().map(|b| b.text).collect()
}

fn server_bodies(account: &AccountConfig, folder: &str, subject: &str) -> Vec<mime::Body> {
    let uids: Vec<u32> = server(account, folder).into_iter().filter(|m| m.subject == subject).map(|m| m.uid).collect();
    let (account, folder) = (account.clone(), folder.to_string());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut imap = Imap::connect(&account, "x").await.unwrap();
            let bodies = if uids.is_empty() { Vec::new() } else { imap.fetch_bodies(&folder, &uids).await.unwrap() };
            imap.logout().await;
            bodies.iter().map(|(_, raw)| mime::body_from_raw(raw)).collect()
        })
    })
    .join()
    .unwrap()
}

fn on_server(account: &AccountConfig, folder: &str, subject: &str) -> Option<MessageSummary> {
    server(account, folder).into_iter().find(|m| m.subject == subject)
}

fn deliver(account: &AccountConfig, from: (&str, &str), subject: &str, body: &str) {
    let draft = Draft {
        from_name: from.0.into(),
        from_addr: from.1.into(),
        to: account.email.clone(),
        subject: subject.into(),
        body: body.into(),
        ..Default::default()
    };
    let built = mime::build_message(&draft).unwrap();
    let account = account.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(mailcore::smtp::send(&account, "x", &built.raw, &built.recipients)).unwrap();
    })
    .join()
    .unwrap();
}

fn create_folders(account: &AccountConfig) {
    edit_folders(account, &["Sent", "Trash", "Archive"], &[]);
}

/// Creates, then deletes folders on the server, as another client would.
fn edit_folders(account: &AccountConfig, create: &[&str], delete: &[&str]) {
    let account = account.clone();
    let (create, delete): (Vec<String>, Vec<String>) =
        (create.iter().map(|f| f.to_string()).collect(), delete.iter().map(|f| f.to_string()).collect());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut imap = Imap::connect(&account, "x").await.unwrap();
            for f in &create {
                imap.create(f).await.unwrap();
            }
            for f in &delete {
                imap.delete(f).await.unwrap();
            }
            imap.logout().await;
        })
    })
    .join()
    .unwrap();
}

/// The folder pane as (label, depth) rows.
fn folder_rows(w: &AppWindow) -> Vec<(String, i32)> {
    w.get_folders().iter().map(|f| (f.label.to_string(), f.depth)).collect()
}

fn test_account(email: &str) -> AccountConfig {
    AccountConfig {
        name: "Story".into(),
        email: email.into(),
        username: email.into(),
        imap_host: "localhost".into(),
        imap_port: 3993,
        imap_security: Security::Tls,
        smtp_host: "localhost".into(),
        smtp_port: 3465,
        smtp_security: Security::Tls,
        auth: mailcore::Auth::Password,
        caldav_url: String::new(),
    }
}

/// Fills the setup form for a GreenMail account and submits it.
fn submit_setup(w: &AppWindow, email: &str) -> Outcome {
    type_in(w, "Email", email)?;
    type_in(w, "Password", "x")?;
    type_in(w, "IMAP server", "localhost")?;
    type_in(w, "IMAP port", "3993")?;
    type_in(w, "SMTP server", "localhost")?;
    type_in(w, "SMTP port", "3465")?;
    click(w, "Save and connect")
}

#[test]
#[ignore = "needs scripts/test-mailserver.sh"]
fn user_stories() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let dirs = tempfile::tempdir().unwrap();
    let ca = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-mailserver/ca.pem");
    // SAFETY: set before any other thread starts.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", dirs.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", dirs.path().join("data"));
        std::env::set_var("MAILCLIENT_PASSWORD", "x"); // keeps the test out of the real keyring
        std::env::set_var("MAILCLIENT_EXTRA_CA", ca);
    }
    mailcore::init_tls();

    let user = format!("story{}@localhost", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());
    let account = test_account(&user);
    // A realistic mailbox: special folders and two received messages.
    create_folders(&account);
    deliver(&account, ("Bob", "bob@example.org"), "Quarterly invoice", "Please find the invoice details below.");
    std::thread::sleep(Duration::from_millis(1100)); // distinct Date headers, so the order is stable
    deliver(&account, ("Alice", "alice@example.org"), "Lunch on Friday?", "Pizza place at noon?");

    let window = AppWindow::new().unwrap();
    Controller::install(&window, None).unwrap();
    window.window().set_size(slint::LogicalSize::new(1280., 800.)); // desktop layout
    window.show().unwrap();
    let results: Rc<RefCell<Vec<(&'static str, Outcome)>>> = Rc::default();

    let w = window.as_weak();
    let res = results.clone();
    slint::spawn_local(async move {
        let w = w.upgrade().unwrap();
        let record = |id: &'static str, r: Outcome| {
            println!("{id}: {}", match &r { Ok(()) => "pass".to_string(), Err(e) => format!("FAIL ({e})") });
            res.borrow_mut().push((id, r));
        };

        // US1: setup guesses settings, verifies login before saving.
        let r = async {
            ensure(w.get_needs_setup(), "setup screen not shown")?;
            // Typed per character like a keyboard does: the name guess must follow.
            for end in 1..=user.len() {
                type_in(&w, "Email", &user[..end])?;
            }
            let local = user.split('@').next().unwrap();
            ensure(w.get_setup().name == local, format!("name guess stuck at {:?}", w.get_setup().name))?;
            ensure(w.get_setup().imap_host == "imap.localhost", "IMAP server not guessed")?;
            ensure(w.get_setup().username == user.as_str(), "username not guessed")?;
            type_in(&w, "Password", "x")?;
            type_in(&w, "IMAP server", "localhost")?;
            type_in(&w, "SMTP server", "localhost")?;
            type_in(&w, "SMTP port", "3465")?;
            type_in(&w, "IMAP port", "1")?; // wrong on purpose
            click(&w, "Save and connect")?;
            wait_for("login error", || !w.get_setup_error().is_empty()).await?;
            ensure(w.get_needs_setup(), "setup closed despite failed login")?;
            ensure(config::load_accounts().unwrap().is_empty(), "config saved despite failed login")?;
            type_in(&w, "IMAP port", "3993")?;
            click(&w, "Save and connect")?;
            wait_for("setup to finish", || !w.get_needs_setup()).await?;
            ensure(config::load_accounts().unwrap().len() == 1, "config not saved")?;
            ensure(w.get_messages().iter().all(|m| m.color.alpha() == 0), "account bar shown with a single account")
        }
        .await;
        record("US1 set up account", r);

        // US2: folders and the inbox, newest first.
        let r = async {
            wait_for("both messages listed", || subjects(&w).len() == 2).await?;
            let folders: Vec<String> = w.get_folders().iter().map(|f| f.label.to_string()).collect();
            ensure(folders.first().map(String::as_str) == Some("Inbox"), format!("folders: {folders:?}"))?;
            for f in ["Sent", "Trash", "Archive"] {
                ensure(folders.iter().any(|x| x == f), format!("{f} missing in {folders:?}"))?;
            }
            ensure(subjects(&w) == ["Lunch on Friday?", "Quarterly invoice"], format!("order: {:?}", subjects(&w)))?;
            ensure(w.get_messages().iter().all(|m| !m.seen), "messages should start unread")?;
            let shown = crate::platform::NOTIFIED.with(|n| n.borrow().clone());
            ensure(shown.is_empty(), format!("mail already there when set up was announced: {shown:?}"))?;
            click(&w, "Folder Sent")?;
            wait_for("Sent list", || subjects(&w).is_empty() && w.get_list_title() == "Sent").await?;
            click(&w, "Folder Inbox")?;
            ensure(subjects(&w).len() == 2, format!("inbox after switching back: {:?}", subjects(&w)))?;
            sleep(2000).await; // let the folder sync finish
            ensure(subjects(&w).len() == 2, format!("inbox after sync: {:?}", subjects(&w)))
        }
        .await;
        record("US2 browse folders and messages", r);

        // US3: reading shows the message and marks it read, also on the server.
        let r = async {
            click(&w, "Alice: Lunch on Friday?")?;
            wait_for("body shown", || w.get_msg_text().contains("Pizza place")).await?;
            ensure(w.get_msg_subject() == "Lunch on Friday?", "subject")?;
            ensure(w.get_msg_from().contains("alice@example.org"), "sender")?;
            ensure(w.get_msg_to().contains(user.as_str()), "recipient")?;
            ensure(w.get_messages().row_data(0).unwrap().seen, "not marked read in list")?;
            wait_for("read flag on server", || on_server(&account, "INBOX", "Lunch on Friday?").is_some_and(|m| m.seen)).await
        }
        .await;
        record("US3 read a message", r);

        // US5: unread and flag, locally at once and then on the server.
        let r = async {
            click(&w, "Mark unread")?;
            ensure(!w.get_messages().row_data(0).unwrap().seen, "not unread in list")?;
            click(&w, "Flag")?;
            ensure(w.get_messages().row_data(0).unwrap().flagged, "not flagged in list")?;
            wait_for("flags on server", || {
                on_server(&account, "INBOX", "Lunch on Friday?").is_some_and(|m| !m.seen && m.flagged)
            })
            .await
        }
        .await;
        record("US5 mark unread and flag", r);

        // US7: search filters, clearing restores the folder.
        let r = async {
            type_in(&w, "Search", "invoice")?;
            ensure(subjects(&w) == ["Quarterly invoice"], format!("search: {:?}", subjects(&w)))?;
            type_in(&w, "Search", "")?;
            ensure(subjects(&w).len() == 2, "list not restored after clearing search")
        }
        .await;
        record("US7 search", r);

        // Phones: Next in the thumb bar steps through the list; the hand setting persists.
        let r = async {
            click(&w, "Alice: Lunch on Friday?")?;
            w.window().set_size(slint::LogicalSize::new(400., 800.));
            let next = click(&w, "Next");
            w.window().set_size(slint::LogicalSize::new(1280., 800.));
            next?;
            ensure(w.get_current_message() == 1, format!("Next selected {}", w.get_current_message()))?;
            click(&w, "Settings")?;
            let checkbox = element(&w, "Left-handed", &[AccessibleRole::Checkbox])?;
            checkbox.invoke_accessible_default_action();
            ensure(w.get_left_handed(), "setting not toggled")?;
            ensure(config::load_settings().is_ok_and(|s| s.left_handed), "setting not saved")?;
            ensure(config::load_accounts().is_ok_and(|a| a.len() == 1), "account lost when saving the setting")?;
            // The floating variant has its own Next.
            element(&w, "Floating buttons", &[AccessibleRole::Checkbox])?.invoke_accessible_default_action();
            ensure(config::load_settings().is_ok_and(|s| s.floating_buttons), "floating setting not saved")?;
            click(&w, "Close")?;
            click(&w, "Alice: Lunch on Friday?")?;
            w.window().set_size(slint::LogicalSize::new(400., 800.));
            let next = click(&w, "Next");
            w.window().set_size(slint::LogicalSize::new(1280., 800.));
            next?;
            ensure(w.get_current_message() == 1, format!("floating Next selected {}", w.get_current_message()))
        }
        .await;
        record("phone thumb bar and hand setting", r);

        // US9: reply is prefilled and marks the original answered.
        let r = async {
            click(&w, "Alice: Lunch on Friday?")?;
            wait_for("message open", || w.get_msg_text().contains("Pizza")).await?;
            click(&w, "Reply")?;
            ensure(w.get_composing(), "compose form not shown")?;
            ensure(w.get_compose_to() == "Alice <alice@example.org>", format!("to: {}", w.get_compose_to()))?;
            ensure(w.get_compose_subject() == "Re: Lunch on Friday?", format!("subject: {}", w.get_compose_subject()))?;
            ensure(w.get_compose_body().contains("> Pizza place at noon?"), "quote missing")?;
            type_in(&w, "To", &user)?; // send to ourselves so it arrives here
            click(&w, "Send")?;
            wait_for("compose form to close", || Controller::current().composer().is_none() && !w.get_composing()).await?;
            wait_for("original answered on server", || {
                on_server(&account, "INBOX", "Lunch on Friday?").is_some_and(|m| m.answered)
            })
            .await?;
            wait_for("reply in inbox", || subjects(&w).iter().any(|s| s == "Re: Lunch on Friday?")).await
        }
        .await;
        record("US9 reply", r);

        // US8: compose and send; delivered and filed in Sent.
        let r = async {
            click(&w, "New message")?;
            ensure(w.get_composing(), "compose form not shown")?;
            click(&w, "Send")?;
            ensure(!w.get_compose_error().is_empty(), "sending without recipient should be refused")?;
            click(&w, "New message")?;
            ensure(w.get_status().contains("first"), "second draft should be refused while writing")?;
            type_in(&w, "To", &user)?;
            type_in(&w, "Subject", "Story send")?;
            type_in(&w, "Body", "Hello from the story test")?;
            click(&w, "Send")?;
            wait_for("compose form to close", || Controller::current().composer().is_none() && !w.get_composing()).await?;
            wait_for("copy in Sent", || on_server(&account, "Sent", "Story send").is_some()).await?;
            wait_for("delivered to inbox", || subjects(&w).iter().any(|s| s == "Story send")).await
        }
        .await;
        record("US8 write and send", r);

        // US18: unfinished messages are kept in Drafts (created here, the account has none),
        // reopened from there, replaced on every save, and removed when sent or discarded.
        let r = async {
            // Drafts lists the recipient, like Sent.
            let row = |subject: &str| format!("To: {user}: {subject}");
            click(&w, "New message")?;
            type_in(&w, "To", &user)?;
            type_in(&w, "Subject", "Draft story")?;
            type_in(&w, "Body", "First version")?;
            wait_for("autosave to Drafts", || server_texts(&account, "Drafts", "Draft story").len() == 1).await?;
            click(&w, "Close")?;
            wait_for("composer closed", || !w.get_composing() && Controller::current().composer().is_none()).await?;

            click(&w, "Folder Drafts")?;
            wait_for("draft listed", || subjects(&w) == ["Draft story"]).await?;
            click(&w, &row("Draft story"))?;
            wait_for("draft open", || w.get_msg_is_draft() && w.get_msg_text().contains("First version")).await?;
            click(&w, "Edit draft")?;
            ensure(w.get_composing(), "draft not opened in the composer")?;
            ensure(w.get_compose_to() == user.as_str(), format!("to: {}", w.get_compose_to()))?;
            ensure(w.get_compose_subject() == "Draft story", format!("subject: {}", w.get_compose_subject()))?;
            ensure(w.get_compose_body().trim_end() == "First version", format!("body: {:?}", w.get_compose_body()))?;
            type_in(&w, "Body", "Second version")?;
            click(&w, "Close")?;
            wait_for("composer closed after saving", || !w.get_composing()).await?;
            let texts = server_texts(&account, "Drafts", "Draft story");
            ensure(texts.len() == 1 && texts[0].contains("Second version"), format!("Drafts on server: {texts:?}"))?;
            wait_for("list shows the new copy only", || subjects(&w) == ["Draft story"]).await?;

            // Sending a reopened draft removes it from Drafts.
            click(&w, &row("Draft story"))?;
            wait_for("draft open", || w.get_msg_text().contains("Second version")).await?;
            click(&w, "Edit draft")?;
            click(&w, "Send")?;
            wait_for("composer closed after sending", || !w.get_composing()).await?;
            wait_for("draft gone after sending", || on_server(&account, "Drafts", "Draft story").is_none()).await?;
            wait_for("sent copy", || on_server(&account, "Sent", "Draft story").is_some()).await?;

            // Discard removes a saved draft.
            click(&w, "New message")?;
            type_in(&w, "To", &user)?;
            type_in(&w, "Subject", "Discarded draft")?;
            click(&w, "Close")?;
            wait_for("saved", || !w.get_composing() && on_server(&account, "Drafts", "Discarded draft").is_some()).await?;
            click(&w, &row("Discarded draft"))?;
            wait_for("draft open", || w.get_msg_subject() == "Discarded draft" && !w.get_msg_loading()).await?;
            click(&w, "Edit draft")?;
            click(&w, "Discard")?;
            ensure(!w.get_composing(), "composer still open after Discard")?;
            wait_for("discarded draft gone", || on_server(&account, "Drafts", "Discarded draft").is_none()).await?;

            // An untouched new message closes without leaving a draft.
            click(&w, "New message")?;
            click(&w, "Close")?;
            ensure(!w.get_composing(), "empty composer did not close")?;
            click(&w, "Folder Inbox")?;
            wait_for("inbox", || w.get_list_title() == "Inbox").await
        }
        .await;
        record("US18 drafts", r);

        // US10: mail from elsewhere shows up without a manual sync (IDLE).
        let r = async {
            deliver(&account, ("Carol", "carol@example.org"), "Pushed via IDLE", "Arrived while open.");
            wait_for("pushed message listed", || subjects(&w).iter().any(|s| s == "Pushed via IDLE")).await?;
            // US19: and announces it with a notification.
            let notified = || crate::platform::NOTIFIED.with(|n| n.borrow().clone());
            wait_for("notification", || notified().iter().any(|(_, body)| body.contains("Pushed via IDLE"))).await?;
            let (title, _) = notified().into_iter().find(|(_, body)| body.contains("Pushed via IDLE")).unwrap();
            ensure(title.contains("Carol"), format!("notification title: {title}"))
        }
        .await;
        record("US10/US19 receive new mail, notified", r);

        // US6: archive and delete leave the list at once and land in Archive/Trash.
        let r = async {
            // Sender name is what setup guessed from the address: its local part.
            click(&w, &format!("{}: Story send", user.split('@').next().unwrap()))?;
            wait_for("message open", || w.get_msg_subject() == "Story send").await?;
            click(&w, "Archive")?;
            ensure(!subjects(&w).iter().any(|s| s == "Story send"), "archived message still listed")?;
            ensure(w.get_has_message(), "next message not selected")?;
            wait_for("in Archive on server", || on_server(&account, "Archive", "Story send").is_some()).await?;
            click(&w, "Carol: Pushed via IDLE")?;
            wait_for("message open", || w.get_msg_subject() == "Pushed via IDLE").await?;
            click(&w, "Delete")?;
            ensure(!subjects(&w).iter().any(|s| s == "Pushed via IDLE"), "deleted message still listed")?;
            wait_for("in Trash on server", || on_server(&account, "Trash", "Pushed via IDLE").is_some()).await?;
            ensure(on_server(&account, "INBOX", "Pushed via IDLE").is_none(), "still in INBOX on server")
        }
        .await;
        record("US6 archive and delete", r);

        // US20: several conversations ticked and changed at once.
        let r = async {
            for subject in ["Bulk one", "Bulk two", "Bulk three"] {
                deliver(&account, ("Erin", "erin@example.org"), subject, "One of several.");
            }
            wait_for("bulk messages listed", || ["Bulk one", "Bulk two", "Bulk three"].iter().all(|s| subjects(&w).iter().any(|x| x == s))).await?;
            let tick = |subject: &str| -> Outcome {
                element(&w, &format!("Select Erin: {subject}"), &[AccessibleRole::Checkbox])?.invoke_accessible_default_action();
                Ok(())
            };
            let row = |subject: &str| w.get_messages().iter().find(|m| m.subject == subject);
            tick("Bulk one")?;
            tick("Bulk two")?;
            ensure(w.get_checked_count() == 2, format!("{} ticked", w.get_checked_count()))?;
            ensure(row("Bulk one").is_some_and(|m| m.checked) && row("Bulk three").is_some_and(|m| !m.checked), "tick marks")?;

            click(&w, "Mark selected read")?;
            ensure(w.get_checked_count() == 0, "ticks kept after the action")?;
            ensure(row("Bulk one").is_some_and(|m| m.seen) && row("Bulk two").is_some_and(|m| m.seen), "not read in the list")?;
            ensure(row("Bulk three").is_some_and(|m| !m.seen), "unticked message changed")?;
            tick("Bulk one")?;
            tick("Bulk two")?;
            click(&w, "Flag selected")?;
            wait_for("read and flagged on server", || {
                ["Bulk one", "Bulk two"].iter().all(|s| on_server(&account, "INBOX", s).is_some_and(|m| m.seen && m.flagged))
            })
            .await?;
            tick("Bulk one")?;
            tick("Bulk two")?;
            click(&w, "Unflag selected")?;
            ensure(row("Bulk one").is_some_and(|m| !m.flagged), "still flagged in the list")?;

            tick("Bulk one")?;
            tick("Bulk two")?;
            tick("Bulk two")?; // unticked again
            ensure(w.get_checked_count() == 1, "untick")?;
            tick("Bulk two")?;
            click(&w, "Archive selected")?;
            ensure(row("Bulk one").is_none() && row("Bulk two").is_none(), "archived rows still listed")?;
            ensure(row("Bulk three").is_some(), "unticked row archived")?;
            wait_for("in Archive on server", || {
                on_server(&account, "Archive", "Bulk one").is_some_and(|m| !m.flagged) && on_server(&account, "Archive", "Bulk two").is_some()
            })
            .await?;

            tick("Bulk three")?;
            click(&w, "Clear selection")?;
            ensure(w.get_checked_count() == 0 && row("Bulk three").is_some_and(|m| !m.checked), "Clear left ticks")?;
            tick("Bulk three")?;
            click(&w, "Delete selected")?;
            ensure(row("Bulk three").is_none(), "deleted row still listed")?;
            wait_for("in Trash on server", || on_server(&account, "Trash", "Bulk three").is_some()).await
        }
        .await;
        record("US20 bulk actions", r);

        // US21: all mail from the open message's sender, and only theirs.
        let r = async {
            deliver(&account, ("Frank", "frank@example.org"), "Garden party", "Saturday at three.");
            deliver(&account, ("Grace", "grace@example.org"), "About Frank", "Ask frank@example.org about the party.");
            std::thread::sleep(Duration::from_millis(1100)); // a later Date, so the order is known
            deliver(&account, ("Frank", "frank@example.org"), "Bring a chair", "We are short on chairs.");
            wait_for("mail from Frank and Grace", || {
                ["Garden party", "About Frank", "Bring a chair"].iter().all(|s| subjects(&w).iter().any(|x| x == s))
            })
            .await?;
            let inbox = subjects(&w);
            click(&w, "Frank: Garden party")?;
            wait_for("message open", || w.get_msg_subject() == "Garden party").await?;
            click(&w, "More from this sender")?;
            ensure(w.get_search_text() == "from:frank@example.org", format!("search box: {}", w.get_search_text()))?;
            ensure(subjects(&w) == ["Bring a chair", "Garden party"], format!("from Frank: {:?}", subjects(&w)))?;
            // Typed by hand, with a word: only Frank's mail about chairs.
            type_in(&w, "Search", "from:FRANK@example.org chair")?;
            ensure(subjects(&w) == ["Bring a chair"], format!("from Frank about chairs: {:?}", subjects(&w)))?;
            type_in(&w, "Search", "")?;
            ensure(subjects(&w) == inbox, format!("inbox not restored: {:?}", subjects(&w)))
        }
        .await;
        record("US21 mail from the same sender", r);

        // US17: folders created and removed elsewhere show up after a sync, subfolders under their parent.
        let r = async {
            // GreenMail's hierarchy delimiter is ".".
            edit_folders(&account, &["Projects", "Projects.Website", "INBOX.Receipts", "INBOX.Receipts.2026"], &[]);
            click(&w, "Sync")?;
            let row = |label: &str, depth| (label.to_string(), depth);
            let all = vec![
                row("Inbox", 0),
                row("Receipts", 1),
                row("2026", 2),
                row("Drafts", 0), // created by US18
                row("Sent", 0),
                row("Archive", 0),
                row("Trash", 0),
                row("Projects", 0),
                row("Website", 1),
            ];
            wait_for("new folders listed", || folder_rows(&w) == all)
                .await
                .map_err(|e| format!("{e}; folders: {:?}", folder_rows(&w)))?;
            click(&w, "Folder Receipts")?;
            wait_for("subfolder opened", || w.get_list_title() == "Receipts").await?;
            click(&w, "Folder Inbox")?;

            edit_folders(&account, &[], &["Projects.Website", "INBOX.Receipts.2026"]);
            click(&w, "Sync")?;
            let rest: Vec<_> = all.into_iter().filter(|(l, _)| l != "Website" && l != "2026").collect();
            wait_for("removed folders gone", || folder_rows(&w) == rest)
                .await
                .map_err(|e| format!("{e}; folders: {:?}", folder_rows(&w)))
        }
        .await;
        record("US17 folders created and removed on the server", r);

        // US11 (part): the server drops the idle connection (GreenMail after ~30 s, phones
        // and NAT routers at random); the next action must reconnect, not empty the cache.
        let r = async {
            let before = subjects(&w);
            sleep(40_000).await;
            click(&w, "Folder Sent")?;
            wait_for("Sent list", || w.get_list_title() == "Sent").await?;
            click(&w, "Folder Inbox")?;
            sleep(3000).await;
            ensure(subjects(&w) == before, format!("inbox changed after reconnect: {:?} -> {:?}", before, subjects(&w)))?;
            ensure(w.get_online(), "not back online")?;
            ensure(!w.get_status().starts_with('⚠'), format!("error shown: {}", w.get_status()))
        }
        .await;
        record("US11 survive a dropped connection", r);

        // US15: a second account; one inbox for both, each message marked with its account's color.
        let r = async {
            let user2 = format!("second-{user}");
            let second = test_account(&user2);
            create_folders(&second);
            deliver(&second, ("Dave", "dave@example.org"), "Second account hello", "Hi there.");
            click(&w, "Settings")?;
            click(&w, "Add account")?;
            ensure(w.get_needs_setup(), "setup not shown")?;
            submit_setup(&w, &user2)?;
            wait_for("second account set up", || !w.get_needs_setup()).await?;
            ensure(config::load_accounts().is_ok_and(|a| a.len() == 2), "second account not saved")?;
            wait_for("unified inbox with both accounts", || {
                w.get_list_title() == "All inboxes"
                    && subjects(&w).iter().any(|s| s == "Second account hello")
                    && subjects(&w).iter().any(|s| s == "Quarterly invoice")
            })
            .await?;
            let folders: Vec<String> = w.get_folders().iter().map(|f| f.label.to_string()).collect();
            ensure(folders.first().map(String::as_str) == Some("All inboxes"), format!("folders: {folders:?}"))?;
            ensure(folders.contains(&user) && folders.contains(&user2), format!("account headings: {folders:?}"))?;
            let color = |subject: &str| w.get_messages().iter().find(|m| m.subject == subject).map(|m| m.color);
            let (first, other) = (color("Quarterly invoice").unwrap(), color("Second account hello").unwrap());
            ensure(first.alpha() == 255 && other.alpha() == 255, "account bar missing")?;
            ensure(first != other, "both accounts have the same color")?;
            let order = subjects(&w);
            ensure(order.first().map(String::as_str) == Some("Second account hello"), format!("not newest first: {order:?}"))?;

            // A reply from the unified inbox goes out through the message's own account.
            click(&w, "Dave: Second account hello")?;
            wait_for("message open", || w.get_msg_text().contains("Hi there")).await?;
            click(&w, "Reply")?;
            type_in(&w, "To", &user2)?;
            click(&w, "Send")?;
            wait_for("compose form to close", || Controller::current().composer().is_none()).await?;
            wait_for("reply in second Sent", || on_server(&second, "Sent", "Re: Second account hello").is_some()).await?;
            ensure(on_server(&account, "Sent", "Re: Second account hello").is_none(), "sent through the first account")?;

            // Opening one account's folder shows only that account.
            click(&w, "Folder Archive")?;
            ensure(w.get_list_title() == "Archive", format!("title: {}", w.get_list_title()))?;
            ensure(!subjects(&w).iter().any(|s| s == "Second account hello"), "other account's mail in folder")?;

            // The same address cannot be added twice; Cancel returns to the mail.
            click(&w, "Settings")?;
            click(&w, "Add account")?;
            submit_setup(&w, &user2)?;
            ensure(w.get_setup_error().contains("already"), format!("error: {}", w.get_setup_error()))?;
            click(&w, "Cancel")?;
            ensure(!w.get_needs_setup(), "Cancel did not close setup")?;
            click(&w, "Close")?;
            ensure(!w.get_show_settings(), "Close did not close settings")?;
            ensure(config::load_accounts().is_ok_and(|a| a.len() == 2), "accounts changed")
        }
        .await;
        record("US15 multiple accounts, unified inbox", r);

        // US16: a conversation between the two accounts; the original from Sent joins the reply.
        let r = async {
            let user2 = format!("second-{user}");
            let second = test_account(&user2);
            let (me, other) = (user.split('@').next().unwrap(), user2.split('@').next().unwrap());
            click(&w, "Folder All inboxes")?;
            click(&w, "New message")?; // from the first account
            type_in(&w, "To", &user2)?;
            type_in(&w, "Subject", "Project kickoff")?;
            type_in(&w, "Body", "Shall we start on Monday?")?;
            click(&w, "Send")?;
            wait_for("compose form to close", || Controller::current().composer().is_none()).await?;
            wait_for("arrival in second account", || subjects(&w).iter().any(|s| s == "Project kickoff")).await?;
            click(&w, &format!("{me}: Project kickoff"))?;
            wait_for("message open", || w.get_msg_text().contains("Monday")).await?;
            click(&w, "Reply")?; // from the second account, to the first
            click(&w, "Send")?;
            wait_for("compose form to close", || Controller::current().composer().is_none()).await?;
            wait_for("reply grouped with the original from Sent", || {
                w.get_messages().iter().any(|m| m.subject == "Re: Project kickoff" && m.count == 2)
            })
            .await
            .map_err(|e| format!("{e}; rows: {:?}", w.get_messages().iter().map(|m| (m.subject.to_string(), m.count)).collect::<Vec<_>>()))?;

            click(&w, &format!("{other}: Re: Project kickoff"))?;
            wait_for("reply open", || w.get_msg_subject() == "Re: Project kickoff").await?;
            ensure(w.get_conversation().row_count() == 2, format!("conversation of {}", w.get_conversation().row_count()))?;
            ensure(w.get_current_in_conversation() == 1, "the newest message should be open")?;
            // The text reader lists both messages, the original fetched from Sent.
            wait_for("whole conversation in the reader", || {
                let parts: Vec<ReaderPart> = w.get_msg_parts().iter().collect();
                parts.len() == 2 && parts.iter().all(|p| p.text != "Loading…") && parts[0].heading.contains(&user)
            })
            .await?;
            let row = w.get_current_message();
            w.invoke_conversation_selected(0);
            wait_for("original from Sent shown", || {
                w.get_msg_subject() == "Project kickoff" && w.get_msg_text().contains("Monday")
            })
            .await?;
            ensure(w.get_current_message() == row, "list selection moved")?;

            // Scrolling to the end highlights the last message, the reply; the original stays open.
            ensure(w.get_scrolled_in_conversation() == -1, "highlight kept from the previous render")?;
            w.window().set_size(slint::LogicalSize::new(800., 300.));
            sleep(50).await;
            let reader = ElementHandle::find_by_element_id(&w, "AppWindow::reader-scroll").next().ok_or("no text reader")?;
            let (pos, size) = (reader.absolute_position(), reader.size());
            let position = slint::LogicalPosition::new(pos.x + size.width / 2., pos.y + size.height / 2.);
            w.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled { position, delta_x: 0., delta_y: -2000. });
            wait_for("timeline follows the scroll", || w.get_scrolled_in_conversation() == 1).await?;
            ensure(w.get_msg_subject() == "Project kickoff", "scrolling opened another message")?;

            // Archiving from the inbox moves the conversation's inbox messages; Sent keeps its copy.
            click(&w, "Archive")?;
            ensure(!subjects(&w).iter().any(|s| s == "Re: Project kickoff"), "conversation still listed")?;
            wait_for("in Archive on server", || on_server(&account, "Archive", "Re: Project kickoff").is_some()).await?;
            ensure(on_server(&account, "Sent", "Project kickoff").is_some(), "Sent copy moved")?;
            ensure(on_server(&second, "INBOX", "Project kickoff").is_some(), "other account's copy moved")
        }
        .await;
        record("US16 conversation between accounts", r);

        // US26: invitations between the accounts and from an Outlook-style organizer; answers go back by mail.
        let r = async {
            let user2 = format!("second-{user}");
            let second = test_account(&user2);
            let me = user.split('@').next().unwrap();
            w.window().set_size(slint::LogicalSize::new(1280., 800.));
            click(&w, "Folder All inboxes")?;
            click(&w, "Calendar")?;
            ensure(w.get_show_calendar(), "calendar not shown")?;
            click(&w, "New event")?; // from the first account
            type_in(&w, "Title", "Design sync")?;
            type_in(&w, "Starts", "10:00")?;
            type_in(&w, "Ends", "11:00")?;
            // The attendee list: refuses non-addresses, skips duplicates and the organizer, removes.
            let add = |text: &str| -> Outcome {
                type_in(&w, "Add attendee", text)?;
                click(&w, "Add")
            };
            let invited = || w.get_event_attendees().iter().map(|a| a.email.to_string()).collect::<Vec<_>>();
            add("not an address")?;
            ensure(w.get_event_error().contains("not an email address") && invited().is_empty(), format!("error {:?}, list {:?}", w.get_event_error(), invited()))?;
            add(&format!("Second Story <{user2}>, {user}, extra@example.org"))?;
            add(&user2)?;
            ensure(invited() == [user2.clone(), "extra@example.org".to_string()], format!("list {:?}", invited()))?;
            ensure(w.get_event_attendees().row_data(0).is_some_and(|a| a.name == "Second Story" && a.status == "Will be invited"), "name or status wrong")?;
            click(&w, "Remove extra@example.org")?;
            ensure(invited() == [user2.clone()], format!("after removing: {:?}", invited()))?;
            ensure(w.get_event_error().is_empty(), format!("error left: {}", w.get_event_error()))?;
            click(&w, "Send invitations")?;
            ensure(w.get_event_error().is_empty(), format!("form error: {}", w.get_event_error()))?;
            wait_for("event in the organizer's calendar", || w.get_calendar_agenda().iter().any(|a| a.title == "Design sync")).await?;
            w.set_show_calendar(false);

            wait_for("invitation arrives", || subjects(&w).iter().any(|s| s == "Invitation: Design sync")).await?;
            click(&w, &format!("{me}: Invitation: Design sync"))?;
            wait_for("invitation card", || w.get_invite_kind() == "REQUEST").await?;
            ensure(w.get_invite_title() == "Design sync", format!("card title {:?}", w.get_invite_title()))?;
            ensure(w.get_invite_when().contains("10:00 – 11:00"), format!("card time {:?}", w.get_invite_when()))?;
            click(&w, "Accept")?; // as the second account
            ensure(w.get_invite_detail().contains("You answered: Accepted"), format!("card detail {:?}", w.get_invite_detail()))?;

            // The organizer gets a REPLY that names the attendee as accepted.
            wait_for("answer on the server", || !server_bodies(&account, "INBOX", "Accepted: Design sync").is_empty()).await?;
            let answer = server_bodies(&account, "INBOX", "Accepted: Design sync").remove(0);
            let reply = mailcore::ical::parse(answer.calendar.as_deref().ok_or("answer has no calendar part")?);
            ensure(reply.method == "REPLY", format!("method {:?}", reply.method))?;
            let who: Vec<(String, String)> = reply.events[0].attendees.iter().map(|a| (a.email.clone(), a.partstat.clone())).collect();
            ensure(who == [(user2.clone(), "ACCEPTED".to_string())], format!("attendees {who:?}"))?;
            wait_for("answer listed", || subjects(&w).iter().any(|s| s == "Accepted: Design sync")).await?;
            click(&w, &format!("{}: Accepted: Design sync", user2.split('@').next().unwrap()))?;
            wait_for("reply card", || w.get_invite_kind() == "REPLY").await?;
            ensure(w.get_invite_detail().contains("Accepted"), format!("reply detail {:?}", w.get_invite_detail()))?;

            // Both accounts keep the event on the device (no calendar server).
            click(&w, "Calendar")?;
            click(&w, "Today")?;
            wait_for("event in both calendars", || w.get_calendar_agenda().iter().filter(|a| a.title == "Design sync").count() == 2).await?;

            // The organizer opens their copy (the attendee's is read-only) and moves it.
            let open_mine = |title: &str| {
                for item in w.get_calendar_agenda().iter().filter(|a| a.title == title) {
                    w.invoke_calendar_event_clicked(item.id);
                    if w.get_event_detail().can_edit {
                        return Ok(());
                    }
                }
                Err(format!("no editable {title:?}"))
            };
            open_mine("Design sync")?;
            ensure(w.get_calendar_showing_event(), "event not opened")?;
            // The attendee's acceptance arrived by mail and is recorded on the organizer's event.
            let listed = || w.get_event_detail().attendees.iter().map(|a| (a.email.to_string(), a.status.to_string())).collect::<Vec<_>>();
            wait_for("the answer recorded", || listed() == [(user2.clone(), "Accepted".to_string())]).await
                .map_err(|e| format!("{e}; detail attendees: {:?}", listed()))?;
            ensure(w.get_event_detail().organizer.contains(&user), format!("organizer: {}", w.get_event_detail().organizer))?;
            click(&w, "Edit")?;
            ensure(w.get_event_attendees().iter().any(|a| a.email == user2.as_str()), "attendees not in the edit form")?;
            type_in(&w, "Title", "Design sync (moved)")?;
            type_in(&w, "Starts", "14:00")?;
            type_in(&w, "Ends", "15:00")?;
            click(&w, "Save and notify")?;
            ensure(w.get_event_error().is_empty(), format!("form error: {}", w.get_event_error()))?;
            wait_for("moved event listed", || w.get_calendar_agenda().iter().any(|a| a.title == "Design sync (moved)" && a.time == "14:00 – 15:00")).await?;
            // The panel returns to the event, showing the saved change.
            ensure(w.get_calendar_showing_event(), "edited event not shown")?;
            wait_for("details updated", || w.get_event_detail().title == "Design sync (moved)" && w.get_event_detail().when.contains("14:00")).await
                .map_err(|e| format!("{e}; detail: {:?}, {:?}", w.get_event_detail().title, w.get_event_detail().when))?;
            wait_for("update at the attendee", || !server_bodies(&second, "INBOX", "Updated invitation: Design sync (moved)").is_empty()).await?;
            let update = server_bodies(&second, "INBOX", "Updated invitation: Design sync (moved)").remove(0);
            let update = mailcore::ical::parse(update.calendar.as_deref().ok_or("update has no calendar part")?);
            ensure(update.method == "REQUEST" && update.events[0].sequence == 1, format!("update: {:?} seq {}", update.method, update.events[0].sequence))?;

            // Week and day views show it at 14:00.
            w.invoke_calendar_set_view(1);
            let columns: Vec<crate::DayColumn> = w.get_calendar_columns().iter().collect();
            ensure(columns.len() == 7, format!("{} columns in the week", columns.len()))?;
            ensure(
                columns.iter().any(|c| c.blocks.iter().any(|b| b.title == "Design sync (moved)" && b.start == 14 * 60 && b.length == 60)),
                "moved event not in the week at 14:00",
            )?;
            w.invoke_calendar_set_view(2);
            ensure(w.get_calendar_columns().row_count() == 1, "day view is not one column")?;
            w.invoke_calendar_set_view(0);

            // Editing the attendees: a third person is added and invited, the second is taken off
            // and gets a cancellation; the event lists the third only.
            let third = test_account(&format!("third-{user}"));
            open_mine("Design sync (moved)")?;
            click(&w, "Edit")?;
            type_in(&w, "Add attendee", &format!("Third Story <{}>", third.email))?;
            click(&w, "Add")?;
            click(&w, &format!("Remove {user2}"))?;
            let invited: Vec<String> = w.get_event_attendees().iter().map(|a| a.email.to_string()).collect();
            ensure(invited == [third.email.clone()], format!("form attendees: {invited:?}"))?;
            click(&w, "Save and notify")?;
            ensure(w.get_event_error().is_empty(), format!("form error: {}", w.get_event_error()))?;
            wait_for("new attendee invited", || !server_bodies(&third, "INBOX", "Updated invitation: Design sync (moved)").is_empty()).await?;
            let invitation = server_bodies(&third, "INBOX", "Updated invitation: Design sync (moved)").remove(0);
            let invitation = mailcore::ical::parse(invitation.calendar.as_deref().ok_or("invitation has no calendar part")?);
            ensure(invitation.method == "REQUEST", format!("method {:?}", invitation.method))?;
            wait_for("removed attendee cancelled", || !server_bodies(&second, "INBOX", "Cancelled: Design sync (moved)").is_empty()).await?;
            let cancel = server_bodies(&second, "INBOX", "Cancelled: Design sync (moved)").remove(0);
            let cancel = mailcore::ical::parse(cancel.calendar.as_deref().ok_or("cancellation has no calendar part")?);
            let cancelled: Vec<&str> = cancel.events[0].attendees.iter().map(|a| a.email.as_str()).collect();
            ensure(cancel.method == "CANCEL" && cancelled == [user2.as_str()], format!("cancellation: {:?} for {cancelled:?}", cancel.method))?;
            let listed = || w.get_event_detail().attendees.iter().map(|a| a.email.to_string()).collect::<Vec<_>>();
            wait_for("event lists the third only", || listed() == [third.email.clone()]).await
                .map_err(|e| format!("{e}; detail attendees: {:?}", listed()))?;

            // Deleting asks first, then cancels it for the attendee (now the third).
            open_mine("Design sync (moved)")?;
            click(&w, "Delete this event")?;
            ensure(w.get_calendar_confirm_delete(), "no confirmation step")?;
            click(&w, "Delete event")?;
            wait_for("deleted event gone", || !w.get_calendar_agenda().iter().any(|a| a.title == "Design sync (moved)")).await?;
            wait_for("cancellation at the attendee", || !server_bodies(&third, "INBOX", "Cancelled: Design sync (moved)").is_empty()).await?;
            w.set_show_calendar(false);

            // A calendar that cannot be reached says so on the calendar page, until it works again.
            let set_calendar_url = |url: &str| -> Outcome {
                click(&w, "Settings")?;
                click(&w, "Edit")?;
                type_in(&w, "Calendar", url)?;
                click(&w, "Save")
            };
            set_calendar_url("http://localhost:9/")?;
            wait_for("account saved", || !w.get_needs_setup()).await?;
            click(&w, "Close")?;
            wait_for("calendar error shown", || w.get_calendar_error().contains("localhost")).await
                .map_err(|e| format!("{e}; banner: {:?}", w.get_calendar_error()))?;
            set_calendar_url("")?;
            wait_for("account saved", || !w.get_needs_setup()).await?;
            click(&w, "Close")?;
            wait_for("calendar error cleared", || w.get_calendar_error().is_empty()).await?;

            // Outlook writes names as "Last, First": the answer must still reach the organizer.
            let jane = test_account(&format!("jane-{user}"));
            let invite = mailcore::ical::Event {
                uid: format!("budget-{user}"),
                summary: "Budget".into(),
                start: chrono::Utc::now().timestamp() + 86400,
                end: chrono::Utc::now().timestamp() + 90000,
                organizer: Some(mailcore::ical::Attendee { email: jane.email.clone(), name: "Doe, Jane".into(), partstat: String::new() }),
                attendees: vec![mailcore::ical::Attendee { email: user2.clone(), name: String::new(), partstat: "NEEDS-ACTION".into() }],
                ..Default::default()
            };
            let draft = Draft {
                from_name: "Doe, Jane".into(),
                from_addr: jane.email.clone(),
                to: user2.clone(),
                subject: "Budget".into(),
                body: "Numbers.".into(),
                calendar: Some(("REQUEST".into(), mailcore::ical::write("REQUEST", &[invite]))),
                ..Default::default()
            };
            let built = mime::build_message(&draft).unwrap();
            let sender = second.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(mailcore::smtp::send(&sender, "x", &built.raw, &built.recipients)).unwrap();
            })
            .join()
            .unwrap();
            wait_for("Outlook invitation arrives", || subjects(&w).iter().any(|s| s == "Budget")).await?;
            click(&w, "Doe, Jane: Budget")?;
            wait_for("invitation card", || w.get_invite_kind() == "REQUEST" && w.get_invite_title() == "Budget").await?;
            click(&w, "Decline")?;
            wait_for("answer at the organizer", || !server_bodies(&jane, "INBOX", "Declined: Budget").is_empty()).await
        }
        .await;
        record("US26 calendar invitations", r);

        // Settings: preferences are saved, accounts can be edited and removed.
        let r = async {
            let user2 = format!("second-{user}");
            let second = test_account(&user2);
            let db = |email: &str| config::data_dir().unwrap().join(format!("{email}.sqlite"));
            click(&w, "Folder All inboxes")?;
            click(&w, "Settings")?;
            ensure(w.get_show_settings(), "settings not shown")?;
            element(&w, "New-mail notifications", &[AccessibleRole::Checkbox])?.invoke_accessible_default_action();
            ensure(config::load_settings().is_ok_and(|s| s.mute_notifications), "mute not saved")?;
            deliver(&second, ("Erin", "erin@example.org"), "Muted arrival", "No popup please.");
            wait_for("muted message listed", || subjects(&w).iter().any(|s| s == "Muted arrival")).await?;
            sleep(1000).await;
            ensure(
                !crate::platform::NOTIFIED.with(|n| n.borrow().iter().any(|(_, body)| body.contains("Muted arrival"))),
                "notified while muted",
            )?;

            // Wrong server settings are rejected and nothing is saved.
            click(&w, "Edit")?;
            ensure(w.get_needs_setup() && w.get_setup().email == user.as_str(), "first account not in the form")?;
            type_in(&w, "IMAP port", "1")?;
            click(&w, "Save")?;
            wait_for("login error", || !w.get_setup_error().is_empty()).await?;
            ensure(config::load_accounts().unwrap()[0].imap_port == 3993, "bad port saved")?;
            // A new name with the password left empty keeps the stored one and restarts the engine.
            type_in(&w, "IMAP port", "3993")?;
            type_in(&w, "Your name", "Renamed Story")?;
            click(&w, "Save")?;
            wait_for("edit saved", || !w.get_needs_setup()).await?;
            ensure(config::load_accounts().unwrap()[0].name == "Renamed Story", "name not saved")?;
            ensure(w.get_accounts().row_data(0).is_some_and(|a| a.name == "Renamed Story"), "settings list not updated")?;
            deliver(&account, ("Frank", "frank@example.org"), "After the edit", "Still syncing?");
            wait_for("sync after restart", || subjects(&w).iter().any(|s| s == "After the edit")).await?;

            // Removing the first account forgets it on this device only.
            click(&w, "Remove")?;
            click(&w, "Remove account")?;
            let left: Vec<String> = config::load_accounts().unwrap().into_iter().map(|a| a.email).collect();
            ensure(left == [user2.clone()], format!("accounts left: {left:?}"))?;
            ensure(!db(&user).exists(), "cache not deleted")?;
            ensure(on_server(&account, "INBOX", "After the edit").is_some(), "server mail touched")?;
            wait_for("only the second account listed", || {
                !subjects(&w).is_empty() && !subjects(&w).iter().any(|s| s == "After the edit")
            })
            .await?;
            ensure(w.get_has_account() && !w.get_needs_setup(), "still one account")?;

            // Removing the last one goes back to setup.
            click(&w, "Remove")?;
            click(&w, "Remove account")?;
            ensure(config::load_accounts().unwrap().is_empty(), "last account not removed")?;
            ensure(w.get_needs_setup() && !w.get_has_account(), "setup not shown after removing the last account")?;
            ensure(subjects(&w).is_empty(), "messages still listed")
        }
        .await;
        record("Settings: preferences, edit and remove accounts", r);

        let _ = slint::quit_event_loop();
    })
    .unwrap();
    slint::run_event_loop().unwrap();

    let failed: Vec<String> =
        results.borrow().iter().filter_map(|(id, r)| r.as_ref().err().map(|e| format!("{id}: {e}"))).collect();
    assert!(failed.is_empty(), "failed stories:\n{}", failed.join("\n"));
}
