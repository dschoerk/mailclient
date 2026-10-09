//! User stories (docs/user-stories.md) against the real app. Needs scripts/test-mailserver.sh and
//! an app built with the MCP server; see README, "End-to-end tests".

use e2e::{App, create_folders, deliver, init_server, test_account, test_ca};

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
    app.type_in("Email", &account.email);
    app.type_in("Password", "x");
    app.type_in("IMAP server", "localhost");
    app.type_in("IMAP port", "3993");
    app.type_in("SMTP server", "localhost");
    app.type_in("SMTP port", "3465");
    app.click("Save and connect", "Button");

    // US2: the inbox lists both messages.
    app.wait_for("Alice: Lunch on Friday?", "ListItem");
    app.wait_for("Bob: Quarterly invoice", "ListItem");

    // US3: open a message. Its body is rendered by Servo, outside the element tree: screenshot only.
    app.click("Alice: Lunch on Friday?", "ListItem");
    app.screenshot("setup_and_browse_inbox");
    assert!(!app.exists("Save and connect", "Button"), "still on the setup screen");
}
