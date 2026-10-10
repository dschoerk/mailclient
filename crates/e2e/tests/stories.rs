//! User stories (docs/user-stories.md) against the real app. Needs scripts/test-mailserver.sh and
//! an app built with the MCP server; see README, "End-to-end tests".

use e2e::{App, Notifications, PixelServer, android, deliver_raw, create_folders, deliver, init_server, server_subjects, test_account, test_ca};

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

/// US4 and HTML mail: rendered (Servo on the desktop, WebView on Android) with remote
/// content blocked until "Load images"; a counting server stands in for the tracker.
#[test]
#[ignore = "needs scripts/test-mailserver.sh and an app built with --features slint/mcp"]
fn html_mail() {
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive"]);
    let pixel = PixelServer::start();
    let raw = format!(
        "From: Newsletter <news@example.org>\r\nTo: {}\r\nSubject: HTML news\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/html; charset=utf-8\r\n\r\n\
         <html><body><h1>Big headline</h1><p>Rendered <b>as HTML</b>.</p>\
         <script>new Image().src = 'http://localhost:{port}/script';</script>\
         <img src=\"http://localhost:{port}/pixel.gif\" width=1 height=1></body></html>\r\n",
        account.email,
        port = pixel.port
    );
    deliver_raw(&account, &raw);
    let app = App::launch();
    app.set_up(&account);
    app.click("Newsletter: HTML news", "ListItem");
    app.wait_for("Load images", "Button");
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(pixel.requests(), Vec::<String>::new(), "remote content loaded before the user allowed it");
    // The WebView is a window of its own, outside Slint's tree and the MCP screenshot.
    if android() { e2e::device_screenshot("html_mail") } else { app.screenshot("html_mail"); }
    app.click("Load images", "Button");
    // Also proves the page was rendered: the image request comes from the parsed message.
    app.wait_until("remote image requested after Load images", || pixel.requests().iter().any(|p| p == "/pixel.gif"));
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!pixel.requests().iter().any(|p| p == "/script"), "a script in the message ran");
}

/// Touch in the Android WebView: it scrolls under a finger, a mailto: link opens the composer,
/// and the view follows the reader area (hidden under the composer, moved on rotation).
#[test]
#[ignore = "needs scripts/test-mailserver.sh and an app built with --features slint/mcp"]
fn html_touch() {
    if !android() {
        eprintln!("skipped: touches in the native WebView are Android only");
        return;
    }
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive"]);
    let paragraphs: String = (1..=60).map(|i| format!("<p style=\"background:hsl({},60%,80%)\">Paragraph {i}</p>", i * 37 % 360)).collect();
    let raw = format!(
        "From: Shop <shop@example.org>\r\nTo: {}\r\nSubject: Long offer\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/html; charset=utf-8\r\n\r\n\
         <html><body><a href=\"mailto:sales@example.org?subject=Order%20question\" \
         style=\"display:block;height:300px;background:#2f6fde;color:#fff;font-size:32px\">Write to sales</a>\
         {paragraphs}</body></html>\r\n",
        account.email
    );
    deliver_raw(&account, &raw);
    let app = App::launch();
    app.set_up(&account);
    app.click("Shop: Long offer", "ListItem");
    let mut frame = None;
    app.wait_until("HTML view shown", || {
        frame = e2e::html_frame();
        frame.is_some()
    });
    let [x0, y0, x1, y1] = frame.unwrap();
    let screen = e2e::Screen::capture();
    assert!(y0 > 0 && y1 <= screen.height && x1 <= screen.width, "frame {:?} off screen", frame);
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Scrolling: the page moves, the native header above it does not.
    let before = e2e::Screen::capture();
    let mid = (x0 + x1) / 2;
    e2e::swipe((mid, y0 + (y1 - y0) * 4 / 5), (mid, y0 + (y1 - y0) / 5), 300);
    std::thread::sleep(std::time::Duration::from_secs(1));
    let after = e2e::Screen::capture();
    assert!(before.differs(&after, [x0, y0, x1, y1]), "the page did not scroll");
    assert!(!before.differs(&after, [0, y0.saturating_sub(250), screen.width, y0]), "the header moved with the swipe");
    e2e::device_screenshot("html_touch_scrolled");

    // Back to the top, then tap the link block.
    for _ in 0..3 {
        e2e::swipe((mid, y0 + (y1 - y0) / 5), (mid, y0 + (y1 - y0) * 4 / 5), 200);
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    e2e::tap(mid, y0 + 150);
    app.wait_for("Send", "Button");
    let value = |label: &str| app.property(&app.wait_for(label, "TextInput"), "accessibleValue");
    assert_eq!(value("To"), "sales@example.org");
    assert_eq!(value("Subject"), "Order question");
    app.wait_until("HTML view hidden under the composer", || e2e::html_frame().is_none());
    app.back();
    app.wait_until("HTML view back after the composer", || e2e::html_frame().is_some());

    // Landscape: the wide layout puts the reader right of the list.
    let landscape = e2e::Landscape::turn();
    app.wait_until("HTML view moved to the landscape reader", || {
        e2e::html_frame().is_some_and(|[lx0, _, lx1, _]| lx0 > 300 && lx1 > x1 + 300)
    });
    e2e::device_screenshot("html_touch_landscape");
    drop(landscape);

    // Leaving the message hides it.
    app.wait_until("portrait again", || e2e::html_frame().is_some_and(|f| f[0] == x0));
    app.back();
    app.wait_until("HTML view hidden with the message closed", || e2e::html_frame().is_none());
}

/// Real newsletter layouts (corpus templates, target/testdata/templates): rendered on both
/// platforms, with every remote image pointed at a counting server that must stay silent.
#[test]
#[ignore = "needs scripts/test-mailserver.sh, the corpus and an app built with --features slint/mcp"]
fn newsletter_templates() {
    let root = e2e::workspace_dir().join("target/testdata/templates");
    let templates = [
        ("Cerberus", "Cerberus/cerberus-responsive.html"),
        ("Responsive", "responsive-html-email-template/email-inlined.html"),
        ("Receipt", "postmark-templates/templates-inlined/basic-full/receipt/content.html"),
    ];
    if !root.exists() {
        eprintln!("skipped: corpus not downloaded (see README)");
        return;
    }
    init_server(&test_ca());
    let account = test_account();
    create_folders(&account, &["Sent", "Trash", "Archive"]);
    let pixel = PixelServer::start();
    for (name, path) in templates {
        let html = std::fs::read_to_string(root.join(path)).unwrap();
        let local = format!("http://localhost:{}/", pixel.port);
        let html = html.replace("src=\"https://", &format!("src=\"{local}")).replace("src=\"http://", &format!("src=\"{local}"));
        e2e::deliver_html(&account, (name, "news@example.org"), &format!("{name} template"), &html);
    }
    let app = App::launch();
    app.set_up(&account);
    for (name, _) in templates {
        app.click(&format!("{name}: {name} template"), "ListItem");
        std::thread::sleep(std::time::Duration::from_secs(3));
        let shot = format!("template_{}", name.to_lowercase());
        if android() { e2e::device_screenshot(&shot) } else { app.screenshot(&shot); }
        assert_eq!(pixel.requests(), Vec::<String>::new(), "{name}: remote content loaded without asking");
        if android() {
            app.back();
        }
    }
    app.click("Cerberus: Cerberus template", "ListItem");
    app.click("Load images", "Button");
    app.wait_until("images requested after Load images", || !pixel.requests().is_empty());
}
