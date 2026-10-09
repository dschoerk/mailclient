//! Parsing received messages, making their HTML safe to display, and building
//! outgoing messages.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use mail_parser::{Address, HeaderValue, Message, MessageParser, MimeHeaders, PartType};

use crate::store::MessageSummary;

/// Origin under which message content is served to the HTML renderer. `.invalid`
/// is reserved (RFC 2606), so this can never collide with a real host.
pub const CONTENT_ORIGIN: &str = "https://message.invalid";

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub index: usize,
    pub name: String,
    pub mime_type: String,
    pub size: usize,
}

/// A message body prepared for display.
#[derive(Debug, Clone, Default)]
pub struct Body {
    /// Sanitized HTML fragment, if the message has an HTML part.
    pub html: Option<String>,
    /// Plain-text rendition, used for the text view, quoting and search.
    pub text: String,
    pub snippet: String,
    pub attachments: Vec<Attachment>,
    /// Inline parts referenced from HTML via `cid:`, keyed by Content-ID.
    pub inline_parts: HashMap<String, (String, Vec<u8>)>,
    /// True if the HTML references remote resources (which are blocked by default).
    pub has_remote_content: bool,
}

fn first_addr(addr: Option<&Address>) -> (String, String) {
    addr.and_then(|a| a.first())
        .map(|a| (a.name().unwrap_or_default().to_string(), a.address().unwrap_or_default().to_string()))
        .unwrap_or_default()
}

/// "Name <addr>, other@example.org"
pub fn format_addresses(addr: Option<&Address>) -> String {
    addr.map(|a| {
        a.iter()
            .map(|a| match (a.name(), a.address()) {
                (Some(n), Some(ad)) if !n.is_empty() => format!("{n} <{ad}>"),
                (_, Some(ad)) => ad.to_string(),
                (Some(n), None) => n.to_string(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

fn header_ids(value: &HeaderValue) -> String {
    match value {
        HeaderValue::Text(t) => format!("<{t}>"),
        HeaderValue::TextList(list) => list.iter().map(|t| format!("<{t}>")).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

/// Builds a list entry from a header-only fetch.
pub fn summary_from_headers(folder: &str, uid: u32, headers: &[u8], internal_date: Option<i64>) -> MessageSummary {
    let parsed = match fix_ut_dates(headers) {
        Some(fixed) => MessageParser::default().parse_headers(&fixed).map(|m| m.into_owned()),
        None => MessageParser::default().parse_headers(headers),
    };
    let Some(msg) = parsed else {
        return MessageSummary { folder: folder.into(), uid, date: internal_date.unwrap_or(0), ..Default::default() };
    };
    let (from_name, from_addr) = first_addr(msg.from());
    let has_attachments = msg
        .header_raw("Content-Type")
        .map(|ct| ct.to_ascii_lowercase().contains("multipart/mixed"))
        .unwrap_or(false);
    MessageSummary {
        folder: folder.into(),
        uid,
        message_id: msg.message_id().map(|id| format!("<{id}>")).unwrap_or_default(),
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name,
        from_addr,
        to: format_addresses(msg.to()),
        cc: format_addresses(msg.cc()),
        date: msg.date().map(|d| d.to_timestamp()).or(internal_date).unwrap_or(0),
        has_attachments,
        in_reply_to: header_ids(msg.in_reply_to()),
        references: header_ids(msg.references()),
        ..Default::default()
    }
}

pub fn parse(raw: &[u8]) -> Option<Message<'_>> {
    match fix_ut_dates(raw) {
        Some(fixed) => MessageParser::default().parse(&fixed).map(|m| m.into_owned()),
        None => MessageParser::default().parse(raw),
    }
}

/// shortcut: mail-parser 0.11.9 lets a `Date: … UT` header (obsolete RFC 822 zone) swallow
/// the next header line, losing e.g. From or the whole message; we rewrite it to the
/// equivalent `GMT` first. Remove once fixed upstream. Returns None if nothing needed fixing.
fn fix_ut_dates(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len() + 8);
    let mut changed = false;
    let mut consumed = 0;
    for line in raw.split_inclusive(|&b| b == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content.is_empty() {
            break; // end of headers
        }
        consumed += line.len();
        if content.len() > 5 && content[..5].eq_ignore_ascii_case(b"date:") && content.ends_with(b" UT") {
            out.extend_from_slice(&content[..content.len() - 2]);
            out.extend_from_slice(b"GMT");
            out.extend_from_slice(&line[content.len()..]);
            changed = true;
        } else {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(&raw[consumed..]);
    changed.then_some(out)
}

pub fn body_from_raw(raw: &[u8]) -> Body {
    let Some(msg) = parse(raw) else {
        let text = String::from_utf8_lossy(raw).into_owned();
        return Body { snippet: snippet(&text), text, ..Default::default() };
    };

    let html_source = msg.html_bodies().find_map(|p| match &p.body {
        PartType::Html(h) => Some(h.as_ref()),
        _ => None,
    });
    let text = match msg.text_bodies().find_map(|p| match &p.body {
        PartType::Text(t) => Some(t.to_string()),
        _ => None,
    }) {
        Some(t) => t,
        None => html_source.map(html_to_text).unwrap_or_default(),
    };

    let mut attachments = Vec::new();
    let mut inline_parts = HashMap::new();
    for (index, part) in msg.attachments().enumerate() {
        let mime_type = part
            .content_type()
            .map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")))
            .unwrap_or_else(|| "application/octet-stream".into());
        if let Some(cid) = part.content_id() {
            inline_parts.insert(cid.trim_matches(['<', '>']).to_string(), (mime_type.clone(), part.contents().to_vec()));
        }
        let name = part.attachment_name().map(str::to_string).or_else(|| {
            part.message().and_then(|m| m.subject()).map(|s| format!("{s}.eml"))
        });
        // Inline images without a filename are part of the HTML, not something to save.
        if name.is_none() && part.content_id().is_some() {
            continue;
        }
        attachments.push(Attachment {
            index,
            name: name.unwrap_or_else(|| format!("attachment-{}", index + 1)),
            mime_type,
            size: part.len(),
        });
    }

    let (html, has_remote_content) = match html_source {
        Some(h) => {
            let (clean, remote) = sanitize_html(h);
            (Some(clean), remote)
        }
        None => (None, false),
    };

    Body { snippet: snippet(&text), text, html, attachments, inline_parts, has_remote_content }
}

/// Raw bytes of the attachment with the given index, for saving to disk.
pub fn attachment_data(raw: &[u8], index: usize) -> Option<Vec<u8>> {
    let msg = parse(raw)?;
    let part = msg.attachments().nth(index)?;
    Some(match &part.body {
        PartType::Message(m) => m.raw_message().to_vec(),
        _ => part.contents().to_vec(),
    })
}

pub fn html_to_text(html: &str) -> String {
    // Trivial decorator: no markdown-style `#`/`*` markers in snippets and quotes.
    html2text::config::with_decorator(html2text::render::TrivialDecorator::new())
        .string_from_read(html.as_bytes(), 100)
        .unwrap_or_default()
}

pub fn snippet(text: &str) -> String {
    let collapsed = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .flat_map(str::split_whitespace)
        // html2text draws table borders with box-drawing characters.
        .filter(|w| !w.chars().all(|c| ('\u{2500}'..='\u{257F}').contains(&c) || c == '.'))
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(160).collect()
}

/// Sanitizes untrusted HTML. Removes scripts, event handlers, forms, frames and
/// anything else that could execute or navigate; rewrites `cid:` references to
/// [`CONTENT_ORIGIN`]. Remote URLs are left in place but blocked by the renderer
/// unless the user opts in. Returns the HTML and whether it references remote content.
pub fn sanitize_html(html: &str) -> (String, bool) {
    let remote = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = remote.clone();
    let cleaned = {
        let mut builder = ammonia::Builder::default();
        builder
            .add_tags(["style", "center", "font"])
            .rm_clean_content_tags(["style"])
            .add_generic_attributes([
                "style", "class", "id", "align", "valign", "width", "height", "bgcolor", "border", "dir", "lang",
                "title", "color",
            ])
            .add_tag_attributes("table", ["cellpadding", "cellspacing", "background"])
            .add_tag_attributes("td", ["background", "nowrap"])
            .add_tag_attributes("font", ["face", "size"])
            .url_schemes(HashSet::from(["http", "https", "mailto", "cid", "data", "tel"]))
            .url_relative(ammonia::UrlRelative::Deny)
            .link_rel(Some("noopener noreferrer"))
            .attribute_filter(move |_element, attribute, value| {
                let lower = value.trim_start().to_ascii_lowercase();
                if lower.starts_with("cid:") {
                    let cid = &value.trim_start()[4..];
                    return Some(Cow::Owned(format!("{CONTENT_ORIGIN}/cid/{}", percent_encode(cid))));
                }
                if lower.starts_with("data:") && !lower.starts_with("data:image/") {
                    return None;
                }
                if matches!(attribute, "src" | "background") && (lower.starts_with("http:") || lower.starts_with("https:")) {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if attribute == "style" && lower.contains("url(") {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Some(Cow::Borrowed(value))
            });
        builder.clean(html).to_string()
    };
    let remote = remote.load(std::sync::atomic::Ordering::Relaxed) || cleaned.to_ascii_lowercase().contains("url(http");
    (cleaned, remote)
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escapes plain text and turns URLs into links.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let quoted = line.starts_with('>');
        if quoted {
            out.push_str("<span class=\"quote\">");
        }
        for (j, word) in line.split(' ').enumerate() {
            if j > 0 {
                out.push(' ');
            }
            let escaped = html_escape::encode_text(word);
            if word.starts_with("https://") || word.starts_with("http://") {
                let href = html_escape::encode_double_quoted_attribute(word);
                out.push_str(&format!("<a href=\"{href}\">{escaped}</a>"));
            } else {
                out.push_str(&escaped);
            }
        }
        if quoted {
            out.push_str("</span>");
        }
    }
    format!("<pre class=\"plain\">{out}</pre>")
}

/// Wraps a sanitized fragment into the document shown in the message view.
pub fn render_document(body: &Body) -> String {
    let content = match &body.html {
        Some(html) => html.clone(),
        None => text_to_html(&body.text),
    };
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\
         :root {{ color-scheme: light; }}\
         body {{ margin: 16px; font-family: sans-serif; font-size: 14px; color: #1d1d1f; background: #fff; overflow-wrap: anywhere; }}\
         img {{ max-width: 100%; height: auto; }}\
         pre.plain {{ white-space: pre-wrap; font-family: monospace; font-size: 13px; }}\
         .quote {{ color: #6e6e73; }}\
         </style></head><body>{content}</body></html>"
    )
}

/// Content-Security-Policy for message documents. `sandbox` without
/// `allow-scripts` disables scripting entirely; the fetch directives are a second
/// layer behind the renderer's own request blocking.
pub fn content_security_policy(allow_remote_images: bool) -> String {
    let img = if allow_remote_images { "https: http: data:" } else { "data:" };
    format!(
        "sandbox; default-src 'none'; style-src 'unsafe-inline'; \
         img-src {CONTENT_ORIGIN} {img}; font-src {CONTENT_ORIGIN} data:; form-action 'none'"
    )
}

// ---- composing ----

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: String,
    pub references: String,
    pub attachments: Vec<std::path::PathBuf>,
}

/// Splits "A <a@x>, b@y" into (name, address) pairs.
pub fn parse_address_list(list: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut depth = 0;
    for c in list.chars().chain(std::iter::once(',')) {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            '<' => {
                depth += 1;
                current.push(c);
            }
            '>' => {
                depth -= 1;
                current.push(c);
            }
            ',' | ';' if !in_quotes && depth == 0 => {
                let entry = current.trim();
                if !entry.is_empty() {
                    if let (Some(lt), Some(gt)) = (entry.rfind('<'), entry.rfind('>')) {
                        let name = entry[..lt].trim().trim_matches('"').trim().to_string();
                        out.push((name, entry[lt + 1..gt].trim().to_string()));
                    } else {
                        out.push((String::new(), entry.to_string()));
                    }
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    out
}

pub struct BuiltMessage {
    pub raw: Vec<u8>,
    pub recipients: Vec<String>,
}

pub fn build_message(draft: &Draft) -> Result<BuiltMessage> {
    use mail_builder::MessageBuilder;
    use mail_builder::headers::address::Address as MbAddress;

    let to = parse_address_list(&draft.to);
    let cc = parse_address_list(&draft.cc);
    let bcc = parse_address_list(&draft.bcc);
    let recipients: Vec<String> = to.iter().chain(&cc).chain(&bcc).map(|(_, a)| a.clone()).collect();
    if recipients.is_empty() {
        return Err(anyhow!("add at least one recipient"));
    }
    for r in &recipients {
        if !r.contains('@') || r.contains(char::is_whitespace) {
            return Err(anyhow!("invalid address: {r}"));
        }
    }
    let to_mb = |list: &[(String, String)]| {
        MbAddress::new_list(
            list.iter().map(|(n, a)| MbAddress::new_address(if n.is_empty() { None } else { Some(n.clone()) }, a.clone())).collect(),
        )
    };

    let mut builder = MessageBuilder::new()
        .from((draft.from_name.clone(), draft.from_addr.clone()))
        .to(to_mb(&to))
        .subject(draft.subject.clone())
        .text_body(draft.body.clone());
    if !cc.is_empty() {
        builder = builder.cc(to_mb(&cc));
    }
    let strip = |s: &str| s.split_whitespace().map(|id| id.trim_matches(['<', '>']).to_string()).collect::<Vec<_>>();
    if !draft.in_reply_to.is_empty() {
        builder = builder.in_reply_to(strip(&draft.in_reply_to));
    }
    if !draft.references.is_empty() {
        builder = builder.references(strip(&draft.references));
    }
    for path in &draft.attachments {
        let data = std::fs::read(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
        builder = builder.attachment(guess_mime(&name), name, data);
    }
    let raw = builder.write_to_vec()?;
    Ok(BuiltMessage { raw, recipients })
}

fn guess_mime(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("txt") => "text/plain",
        Some("html" | "htm") => "text/html",
        Some("zip") => "application/zip",
        Some("ics") => "text/calendar",
        _ => "application/octet-stream",
    }
}

pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let lower = subject.to_ascii_lowercase();
    if lower.starts_with(&prefix.to_ascii_lowercase()) {
        subject.to_string()
    } else {
        format!("{prefix} {subject}")
    }
}

fn format_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%a, %d %b %Y at %H:%M").to_string())
        .unwrap_or_default()
}

/// Prefills a draft answering or forwarding `original`.
pub fn reply_draft(kind: ReplyKind, original: &MessageSummary, body_text: &str, me: &str) -> Draft {
    let sender = if original.from_name.is_empty() {
        original.from_addr.clone()
    } else {
        format!("{} <{}>", original.from_name, original.from_addr)
    };
    match kind {
        ReplyKind::Forward => Draft {
            subject: prefixed("Fwd:", &original.subject),
            body: format!(
                "\n\n---------- Forwarded message ----------\nFrom: {sender}\nDate: {}\nSubject: {}\nTo: {}\n\n{body_text}",
                format_date(original.date),
                original.subject,
                original.to
            ),
            ..Default::default()
        },
        ReplyKind::Reply | ReplyKind::ReplyAll => {
            let quoted: String = body_text.lines().map(|l| format!("> {l}\n")).collect();
            let cc = if matches!(kind, ReplyKind::ReplyAll) {
                parse_address_list(&format!("{}, {}", original.to, original.cc))
                    .into_iter()
                    .filter(|(_, a)| !a.eq_ignore_ascii_case(me) && !a.eq_ignore_ascii_case(&original.from_addr))
                    .map(|(n, a)| if n.is_empty() { a } else { format!("{n} <{a}>") })
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                String::new()
            };
            let references = [original.references.as_str(), original.message_id.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            Draft {
                to: sender.clone(),
                cc,
                subject: prefixed("Re:", &original.subject),
                body: format!("\n\nOn {}, {sender} wrote:\n{quoted}", format_date(original.date)),
                in_reply_to: original.message_id.clone(),
                references,
                ..Default::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML_MAIL: &[u8] = b"From: Alice <alice@example.org>\r\n\
To: Bob <bob@example.org>, carol@example.org\r\n\
Subject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?=\r\n\
Message-ID: <abc@example.org>\r\n\
Date: Thu, 08 Oct 2026 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/related; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><head><style>p{color:red}</style><script>alert(1)</script></head>\
<body onload=\"evil()\"><p>Hi <a href=\"javascript:evil()\">x</a> <a href=\"https://example.org\" target=\"_blank\">y</a></p>\
<img src=\"cid:logo@x\"><img src=\"https://tracker.example/p.gif\"><form action=\"https://x\"><input></form>\
<iframe src=\"https://x\"></iframe></body></html>\r\n\
--b\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@x>\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--b--\r\n";

    #[test]
    fn sanitizes_active_content() {
        let body = body_from_raw(HTML_MAIL);
        let html = body.html.unwrap();
        for bad in ["<script", "alert(1)", "onload", "javascript:", "<form", "<input", "<iframe", "target="] {
            assert!(!html.contains(bad), "{bad} survived: {html}");
        }
        assert!(html.contains("p{color:red}"), "style should be kept: {html}");
        assert!(html.contains("https://message.invalid/cid/logo@x"), "{html}");
        assert!(html.contains("noopener"));
        assert!(body.has_remote_content);
        assert_eq!(body.inline_parts.get("logo@x").map(|p| p.0.as_str()), Some("image/png"));
        assert!(body.attachments.is_empty(), "inline image is not an attachment");
        assert!(body.text.contains("Hi"));
    }

    #[test]
    fn summary_from_headers_decodes_fields() {
        let s = summary_from_headers("INBOX", 5, HTML_MAIL, None);
        assert_eq!(s.subject, "Grüße");
        assert_eq!(s.from_name, "Alice");
        assert_eq!(s.from_addr, "alice@example.org");
        assert_eq!(s.to, "Bob <bob@example.org>, carol@example.org");
        assert_eq!(s.message_id, "<abc@example.org>");
        assert_eq!(s.date, 1791453600);
    }

    #[test]
    fn plain_text_is_escaped_and_linkified() {
        let html = text_to_html("a <b> & https://example.org/?a=1&b=2\n> quoted");
        assert!(html.contains("a &lt;b&gt; &amp;"));
        assert!(html.contains("<a href=\"https://example.org/?a=1&amp;b=2\">"));
        assert!(html.contains("<span class=\"quote\">&gt; quoted</span>"));
    }

    #[test]
    fn csp_disables_scripts() {
        let csp = content_security_policy(false);
        assert!(csp.starts_with("sandbox;"));
        assert!(!csp.contains("allow-scripts"));
        assert!(!csp.contains(" https: "));
        assert!(content_security_policy(true).contains(" https: "));
    }

    #[test]
    fn address_lists() {
        assert_eq!(
            parse_address_list("\"Doe, John\" <john@x.org>, jane@y.org; Bob <bob@z.org>"),
            vec![
                ("Doe, John".into(), "john@x.org".into()),
                (String::new(), "jane@y.org".into()),
                ("Bob".into(), "bob@z.org".into())
            ]
        );
    }

    #[test]
    fn reply_all_excludes_self_and_threads() {
        let original = summary_from_headers("INBOX", 1, HTML_MAIL, None);
        let d = reply_draft(ReplyKind::ReplyAll, &original, "line1\nline2", "bob@example.org");
        assert_eq!(d.to, "Alice <alice@example.org>");
        assert_eq!(d.cc, "carol@example.org");
        assert_eq!(d.subject, "Re: Grüße");
        assert_eq!(d.in_reply_to, "<abc@example.org>");
        assert!(d.body.contains("> line1\n> line2"));
        assert_eq!(reply_draft(ReplyKind::Reply, &original, "", "").subject, "Re: Grüße");
        assert_eq!(prefixed("Re:", "RE: x"), "RE: x");
    }

    #[test]
    fn builds_parseable_message() {
        let draft = Draft {
            from_name: "Me".into(),
            from_addr: "me@example.org".into(),
            to: "You <you@example.org>".into(),
            bcc: "hidden@example.org".into(),
            subject: "Hello".into(),
            body: "Body text".into(),
            in_reply_to: "<abc@example.org>".into(),
            ..Default::default()
        };
        let built = build_message(&draft).unwrap();
        assert_eq!(built.recipients, vec!["you@example.org", "hidden@example.org"]);
        let raw = String::from_utf8(built.raw.clone()).unwrap();
        assert!(!raw.contains("hidden@example.org"), "Bcc must not appear in headers");
        let parsed = parse(&built.raw).unwrap();
        assert_eq!(parsed.subject(), Some("Hello"));
        assert_eq!(parsed.in_reply_to().as_text(), Some("abc@example.org"));
        assert!(build_message(&Draft::default()).is_err());
    }

    #[test]
    fn parses_obsolete_ut_zone() {
        let raw = b"Date: Sat, 14 Sep 2002 06:00:48 UT\nFrom: A <a@b.c>\nSubject: x\n\nbody\n";
        let s = summary_from_headers("INBOX", 1, raw, None);
        assert_eq!((s.from_addr.as_str(), s.subject.as_str(), s.date), ("a@b.c", "x", 1031983248));
        assert!(body_from_raw(raw).text.contains("body"));
    }

    #[test]
    fn snippet_skips_table_borders() {
        assert_eq!(snippet("───┬───\nHello ─── world"), "Hello world");
    }

    #[test]
    fn html_to_text_has_no_markup() {
        assert_eq!(html_to_text("<h1>Hi</h1><p><b>there</b></p>").split_whitespace().collect::<Vec<_>>(), ["Hi", "there"]);
    }

    #[test]
    fn percent_roundtrip() {
        assert_eq!(percent_decode(&percent_encode("a b/c@d")), "a b/c@d");
    }
}
