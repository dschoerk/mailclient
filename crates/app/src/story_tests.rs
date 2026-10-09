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

use crate::AppWindow;
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
            let messages = imap.fetch_new(&folder, 0).await.unwrap();
            imap.logout().await;
            messages
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
    let account = AccountConfig {
        name: "Story".into(),
        email: user.clone(),
        username: user.clone(),
        imap_host: "localhost".into(),
        imap_port: 3993,
        imap_security: Security::Tls,
        smtp_host: "localhost".into(),
        smtp_port: 3465,
        smtp_security: Security::Tls,
        auth: mailcore::Auth::Password,
    };
    // A realistic mailbox: special folders and two received messages.
    {
        let account = account.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let mut imap = Imap::connect(&account, "x").await.unwrap();
                for f in ["Sent", "Trash", "Archive"] {
                    imap.create(f).await.unwrap();
                }
                imap.logout().await;
            })
        })
        .join()
        .unwrap();
    }
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
            ensure(mailcore::config::load_account().unwrap().is_none(), "config saved despite failed login")?;
            type_in(&w, "IMAP port", "3993")?;
            click(&w, "Save and connect")?;
            wait_for("setup to finish", || !w.get_needs_setup()).await?;
            ensure(mailcore::config::load_account().unwrap().is_some(), "config not saved")
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
            // Set on the desktop layout: phones hide the folder pane with the checkbox.
            let checkbox = element(&w, "Left-handed", &[AccessibleRole::Checkbox])?;
            checkbox.invoke_accessible_default_action();
            ensure(w.get_left_handed(), "setting not toggled")?;
            ensure(config::load_settings().is_ok_and(|s| s.left_handed), "setting not saved")?;
            ensure(config::load_account().ok().flatten().is_some(), "account lost when saving the setting")?;
            // The floating variant has its own Next.
            element(&w, "Floating buttons", &[AccessibleRole::Checkbox])?.invoke_accessible_default_action();
            ensure(config::load_settings().is_ok_and(|s| s.floating_buttons), "floating setting not saved")?;
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

        // US10: mail from elsewhere shows up without a manual sync (IDLE).
        let r = async {
            deliver(&account, ("Carol", "carol@example.org"), "Pushed via IDLE", "Arrived while open.");
            wait_for("pushed message listed", || subjects(&w).iter().any(|s| s == "Pushed via IDLE")).await
        }
        .await;
        record("US10 receive new mail", r);

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

        let _ = slint::quit_event_loop();
    })
    .unwrap();
    slint::run_event_loop().unwrap();

    let failed: Vec<String> =
        results.borrow().iter().filter_map(|(id, r)| r.as_ref().err().map(|e| format!("{id}: {e}"))).collect();
    assert!(failed.is_empty(), "failed stories:\n{}", failed.join("\n"));
}
