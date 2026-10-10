//! User stories (docs/user-stories.md) against the real app. Needs scripts/test-mailserver.sh and
//! an app built with the MCP server; see README, "End-to-end tests".

use e2e::{App, Notifications, android, create_folders, deliver, init_server, server_subjects, test_account, test_ca};

#[test]
#[ignore = "needs scripts/test-mailserver.sh and an app built with --features slint/mcp"]
fn setup_and_browse_inbox() {
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive"]);
    deliver(&account, ("Bob", "bob@example.org"), "Quarterly invoice", "Please find the invoice details below.");
    deliver(&account, ("Alice", "alice@example.org"), "Lunch on Friday?", "Pizza place at noon?");

    let app = App::launch();

    // US1: setup verifies the login before saving.
    app.set_up(&account);

    // US2: the inbox lists both messages.
    app.wait_for("Alice: Lunch on Friday?", "ListItem");
    app.wait_for("Bob: Quarterly invoice", "ListItem");

    // US3: open a message. Its body is rendered by Servo, outside the element tree: screenshot only.
    app.click("Alice: Lunch on Friday?", "ListItem");
    app.screenshot("setup_and_browse_inbox");
    assert!(!app.exists("Save and connect", "Button"), "still on the setup screen");
}

/// US18: leaving the composer keeps the message in Drafts; it can be reopened and sent from there.
#[test]
#[ignore = "needs scripts/test-mailserver.sh and an app built with --features slint/mcp"]
fn drafts() {
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive", "Drafts"]);
    let app = App::launch();
    app.set_up(&account);
    app.click_in_drawer("New message", "Button");
    app.type_in("To", &account.email);
    app.type_in("Subject", "E2E draft");
    app.type_in("Body", "Written on the go");
    // Phones leave with the system Back key; the desktop with Close.
    if android() { app.back() } else { app.click("Close", "Button") }
    app.wait_until("draft in Drafts on the server", || server_subjects(&account, "Drafts") == ["E2E draft"]);
    app.wait_until("composer closed", || !app.exists("Send", "Button"));

    app.click_in_drawer("Folder Drafts", "ListItem");
    app.click(&format!("To: {}: E2E draft", account.email), "ListItem");
    app.click("Edit draft", "Button");
    app.wait_for("Send", "Button");
    app.screenshot("drafts_reopened");
    app.click("Send", "Button");
    app.wait_until("draft removed after sending", || server_subjects(&account, "Drafts").is_empty());
    app.wait_until("copy in Sent", || server_subjects(&account, "Sent") == ["E2E draft"]);
}

/// US19: mail arriving while the app runs is announced with a system notification.
#[test]
#[ignore = "needs scripts/test-mailserver.sh and an app built with --features slint/mcp"]
fn new_mail_notification() {
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive"]);
    deliver(&account, ("Bob", "bob@example.org"), "Old news", "Already here before setup.");
    let app = App::launch();
    let notifications = Notifications::watch();
    app.set_up(&account);
    app.wait_for("Bob: Old news", "ListItem");

    let subject = format!("Fresh mail {}", account.email);
    deliver(&account, ("Carol", "carol@example.org"), &subject, "Arrived while the app was open.");
    app.wait_for(&format!("Carol: {subject}"), "ListItem");
    app.wait_until("notification", || notifications.shown(&subject));
    assert!(!notifications.shown("Old news"), "mail from before setup was announced");
}

/// US20: several conversations ticked, then read and archived at once.
#[test]
#[ignore = "needs scripts/test-mailserver.sh and an app built with --features slint/mcp"]
fn bulk_actions() {
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive"]);
    for subject in ["Bulk one", "Bulk two", "Bulk three"] {
        deliver(&account, ("Erin", "erin@example.org"), subject, "One of several.");
    }
    let app = App::launch();
    app.set_up(&account);
    app.wait_for("Erin: Bulk three", "ListItem");

    // Phones tick with a long press, then a tap; the desktop with the tick box.
    if android() {
        app.long_press("Erin: Bulk one", "ListItem");
        app.wait_for("1 selected", "Text");
        app.click("Erin: Bulk two", "ListItem");
    } else {
        app.click("Select Erin: Bulk one", "Checkbox");
        app.click("Select Erin: Bulk two", "Checkbox");
    }
    app.wait_for("2 selected", "Text");
    app.screenshot("bulk_selected");
    app.click("Mark selected read", "Button");
    app.wait_until("read on the server", || {
        let read = e2e::server_messages(&account, "INBOX");
        ["Bulk one", "Bulk two"].iter().all(|s| read.iter().any(|m| m.subject == *s && m.seen))
            && read.iter().any(|m| m.subject == "Bulk three" && !m.seen)
    });

    app.click("Select Erin: Bulk one", "Checkbox");
    app.click("Select Erin: Bulk two", "Checkbox");
    app.click("Archive selected", "Button");
    app.wait_until("archived on the server", || server_subjects(&account, "INBOX") == ["Bulk three"]);
    assert!(!app.exists("Erin: Bulk one", "ListItem"), "archived row still listed");
    let mut archived = server_subjects(&account, "Archive");
    archived.sort();
    assert_eq!(archived, ["Bulk one", "Bulk two"]);
}
