//! Runs public test mail through the parser and sanitizer. Fetch it first:
//!   scripts/fetch-testdata.sh && cargo test -p mailcore --test corpus -- --ignored
//! Only file names are ever printed, never message content.

use std::path::{Path, PathBuf};

use mailcore::mime;

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/testdata")
}

/// (name, raw RFC 822 message) for every test message.
fn corpus() -> Vec<(String, Vec<u8>)> {
    let root = testdata();
    assert!(root.exists(), "run scripts/fetch-testdata.sh first");
    let mut out = Vec::new();

    for folder in std::fs::read_dir(root.join("spamassassin")).unwrap() {
        for file in std::fs::read_dir(folder.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            // Each folder has a `cmds` file listing deletions; it is not a message.
            if path.file_name().unwrap() != "cmds" {
                out.push((rel(&root, &path), std::fs::read(&path).unwrap()));
            }
        }
    }

    let templates = root.join("templates");
    let mut html_files: Vec<PathBuf> = ["Cerberus", "responsive-html-email-template"]
        .iter()
        .flat_map(|repo| std::fs::read_dir(templates.join(repo)).unwrap())
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "html"))
        .collect();
    for kind in std::fs::read_dir(templates.join("postmark-templates/templates-inlined")).unwrap() {
        for template in std::fs::read_dir(kind.unwrap().path()).unwrap() {
            html_files.push(template.unwrap().path().join("content.html"));
        }
    }
    for path in html_files {
        let html = std::fs::read_to_string(&path).unwrap();
        let text = std::fs::read_to_string(path.with_extension("txt"))
            .unwrap_or_else(|_| mime::html_to_text(&html));
        let name = rel(&root, &path);
        let raw = mail_builder::MessageBuilder::new()
            .from(("Template Sender", "templates@example.org"))
            .to("you@example.org")
            .subject(format!("Template: {name}"))
            .html_body(html)
            .text_body(text)
            .write_to_vec()
            .unwrap();
        out.push((name, raw));
    }
    out.sort();
    out
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap().display().to_string()
}

/// Returns why sanitized HTML is unsafe, if it is.
fn unsafe_markup(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    for bad in [
        "<script",
        "<iframe",
        "<object",
        "<embed",
        "<form",
        "<base",
        "javascript:",
    ] {
        if lower.contains(bad) {
            return Some(bad.into());
        }
    }
    // Event handler attributes, looked for inside tags only.
    for tag in lower
        .split('<')
        .skip(1)
        .map(|t| t.split('>').next().unwrap_or(""))
    {
        if let Some(attr) = tag
            .split_whitespace()
            .skip(1)
            .find(|a| a.starts_with("on") && a.contains('='))
        {
            return Some(format!("handler {}", attr.split('=').next().unwrap()));
        }
    }
    None
}

#[test]
fn unsafe_markup_detector_works() {
    assert!(unsafe_markup("<p onclick=\"x\">").is_some());
    assert!(unsafe_markup("<a href=\"JavaScript:x\">").is_some());
    assert!(unsafe_markup("<p class=\"a\">online=1</p>").is_none());
}

#[test]
#[ignore = "needs scripts/fetch-testdata.sh"]
fn corpus_parses_and_sanitizes() {
    let corpus = corpus();
    let (mut html_count, mut failures) = (0, Vec::new());
    for (name, raw) in &corpus {
        let body = mime::body_from_raw(raw);
        if let Some(html) = &body.html {
            html_count += 1;
            if let Some(why) = unsafe_markup(html) {
                failures.push(format!("{name}: {why}"));
            }
        }
        if body.text.trim().is_empty() && body.html.is_none() && body.attachments.is_empty() {
            failures.push(format!("{name}: no readable content"));
        }
        let summary = mime::summary_from_headers("INBOX", 1, raw, None);
        if summary.from_addr.is_empty() && summary.from_name.is_empty() {
            failures.push(format!("{name}: no sender"));
        }
    }
    println!(
        "{} messages, {html_count} with HTML, {} failures",
        corpus.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Fills the local GreenMail server (scripts/test-mailserver.sh) with the corpus,
/// for browsing in the app as `corpus@localhost` (any password).
#[test]
#[ignore = "needs scripts/fetch-testdata.sh and scripts/test-mailserver.sh"]
fn load_into_greenmail() {
    // SAFETY: set before any other thread starts.
    unsafe {
        std::env::set_var(
            "MAILCLIENT_EXTRA_CA",
            testdata().join("../test-mailserver/ca.pem"),
        )
    };
    mailcore::init_tls();
    let account = mailcore::AccountConfig {
        imap_host: "localhost".into(),
        imap_port: 3993,
        smtp_host: "localhost".into(),
        smtp_port: 3465,
        ..mailcore::AccountConfig::guess("corpus@localhost")
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut imap = mailcore::imap::Imap::connect(&account, "any")
            .await
            .unwrap();
        if imap.select("INBOX").await.unwrap().exists > 0 {
            println!("corpus@localhost already has mail; restart the server to reload");
            return;
        }
        let corpus = corpus();
        for (_, raw) in &corpus {
            imap.append("INBOX", "()", raw).await.unwrap();
        }
        println!("appended {} messages to corpus@localhost", corpus.len());
        imap.logout().await;
    });
}
