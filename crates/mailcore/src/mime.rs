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
        .map(|a| {
            (
                a.name().unwrap_or_default().to_string(),
                a.address().unwrap_or_default().to_string(),
            )
        })
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
        HeaderValue::TextList(list) => list
            .iter()
            .map(|t| format!("<{t}>"))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Builds a list entry from a header-only fetch.
pub fn summary_from_headers(
    folder: &str,
    uid: u32,
    headers: &[u8],
    internal_date: Option<i64>,
) -> MessageSummary {
    let parsed = match fix_ut_dates(headers) {
        Some(fixed) => MessageParser::default()
            .parse_headers(&fixed)
            .map(|m| m.into_owned()),
        None => MessageParser::default().parse_headers(headers),
    };
    let Some(msg) = parsed else {
        return MessageSummary {
            folder: folder.into(),
            uid,
            date: internal_date.unwrap_or(0),
            ..Default::default()
        };
    };
    let (from_name, from_addr) = first_addr(msg.from());
    let has_attachments = msg
        .header_raw("Content-Type")
        .map(|ct| ct.to_ascii_lowercase().contains("multipart/mixed"))
        .unwrap_or(false);
    MessageSummary {
        folder: folder.into(),
        uid,
        message_id: msg
            .message_id()
            .map(|id| format!("<{id}>"))
            .unwrap_or_default(),
        subject: msg.subject().unwrap_or_default().to_string(),
        from_name,
        from_addr,
        to: format_addresses(msg.to()),
        cc: format_addresses(msg.cc()),
        date: msg
            .date()
            .map(|d| d.to_timestamp())
            .or(internal_date)
            .unwrap_or(0),
        has_attachments,
        in_reply_to: header_ids(msg.in_reply_to()),
        references: header_ids(msg.references()),
        ..Default::default()
    }
}

pub fn parse(raw: &[u8]) -> Option<Message<'_>> {
    match fix_ut_dates(raw) {
        Some(fixed) => MessageParser::default()
            .parse(&fixed)
            .map(|m| m.into_owned()),
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
        if content.len() > 5
            && content[..5].eq_ignore_ascii_case(b"date:")
            && content.ends_with(b" UT")
        {
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
        return Body {
            snippet: snippet(&text),
            text,
            ..Default::default()
        };
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
            inline_parts.insert(
                cid.trim_matches(['<', '>']).to_string(),
                (mime_type.clone(), part.contents().to_vec()),
            );
        }
        let name = part.attachment_name().map(str::to_string).or_else(|| {
            part.message()
                .and_then(|m| m.subject())
                .map(|s| format!("{s}.eml"))
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

    Body {
        snippet: snippet(&text),
        text,
        html,
        attachments,
        inline_parts,
        has_remote_content,
    }
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
        .filter(|w| {
            !w.chars()
                .all(|c| ('\u{2500}'..='\u{257F}').contains(&c) || c == '.')
        })
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
                "style", "class", "id", "align", "valign", "width", "height", "bgcolor", "border",
                "dir", "lang", "title", "color",
            ])
            .add_tag_attributes("table", ["cellpadding", "cellspacing", "background"])
            .add_tag_attributes("td", ["background", "nowrap"])
            .add_tag_attributes("font", ["face", "size"])
            .url_schemes(HashSet::from([
                "http", "https", "mailto", "cid", "data", "tel",
            ]))
            .url_relative(ammonia::UrlRelative::Deny)
            .link_rel(Some("noopener noreferrer"))
            .attribute_filter(move |_element, attribute, value| {
                let lower = value.trim_start().to_ascii_lowercase();
                if lower.starts_with("cid:") {
                    let cid = &value.trim_start()[4..];
                    return Some(Cow::Owned(format!(
                        "{CONTENT_ORIGIN}/cid/{}",
                        percent_encode(cid)
                    )));
                }
                if lower.starts_with("data:") && !lower.starts_with("data:image/") {
                    return None;
                }
                if matches!(attribute, "src" | "background")
                    && (lower.starts_with("http:") || lower.starts_with("https:"))
                {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if attribute == "style" && lower.contains("url(") {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Some(Cow::Borrowed(value))
            });
        builder.clean(html).to_string()
    };
    let remote = remote.load(std::sync::atomic::Ordering::Relaxed)
        || cleaned.to_ascii_lowercase().contains("url(http");
    (cleaned, remote)
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
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

fn body_html(body: &Body) -> String {
    match &body.html {
        Some(html) => html.clone(),
        None => text_to_html(&body.text),
    }
}

/// Wraps a sanitized fragment into the document shown in the message view.
pub fn render_document(body: &Body) -> String {
    document(&body_html(body))
}

/// A conversation in one document, each message under its `heading` and anchored `#m<index>`;
/// `current` is marked. A `None` body is still downloading.
///
/// shortcut: messages share one document, so a message's own `<style>` can restyle the others;
/// give each its own frame if that shows up in real mail.
pub fn render_conversation(messages: &[(String, Option<&Body>)], current: usize) -> String {
    let mut content = String::new();
    for (i, (heading, body)) in messages.iter().enumerate() {
        let class = if i == current {
            "message current"
        } else {
            "message"
        };
        let inner = body
            .map(body_html)
            .unwrap_or_else(|| "<p class=\"loading\">Loading…</p>".into());
        content.push_str(&format!(
            "<section id=\"m{i}\" class=\"{class}\"><h2>{}</h2>{inner}</section>",
            html_escape::encode_text(heading)
        ));
    }
    document(&content)
}

/// Inline style of a link-target badge. Inline and `!important`, so the message's own CSS cannot
/// restyle or hide it with a class rule.
///
/// shortcut: hostile CSS can still cover the badge (e.g. a positioned element on top); a
/// tamper-proof display would need the badge drawn outside the document.
const LINK_TARGET_STYLE: &str = "display:inline !important; visibility:visible !important; opacity:1 !important; \
    position:static !important; font:11px/1.4 sans-serif !important; color:#1d1d1f !important; \
    background:#eef1f6 !important; border:1px solid #c9d2e3 !important; border-radius:3px; \
    padding:0 4px !important; margin:0 0 0 4px !important; white-space:nowrap; text-decoration:none !important;";
const LINK_MISMATCH_STYLE: &str =
    "color:#a01818 !important; background:#fde8e8 !important; border-color:#e5a0a0 !important;";

/// Shows where each web link in a sanitized HTML fragment really goes: a badge with the target's
/// host after the link, red with ⚠ when the link text names another site (`paypal.com` linking to
/// `paypal.com.evil.io`). Links whose text already shows their target get none. Hosts are shown
/// as the URL parser normalizes them, so look-alike Unicode domains appear as `xn--…`.
///
/// Expects [`sanitize_html`] output: html5ever's serialization (lowercase tags, attribute values
/// double-quoted with `"` escaped), no nested links.
pub fn mark_link_targets(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 256);
    let mut rest = html;
    loop {
        let next_link = rest.find("<a ");
        let next_style = rest.find("<style");
        let (start, is_style) = match (next_link, next_style) {
            (Some(l), Some(s)) if s < l => (s, true),
            (Some(l), _) => (l, false),
            (None, Some(s)) => (s, true),
            (None, None) => break,
        };
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // Style sheets are raw text: something looking like a tag in there is none.
        if is_style {
            let end = rest.find("</style>").map_or(rest.len(), |e| e + "</style>".len());
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let Some((tag_len, href)) = parse_start_tag(rest) else {
            out.push_str(rest);
            return out;
        };
        let (inner_end, close) = match rest[tag_len..].find("</a>") {
            Some(c) => (tag_len + c, tag_len + c + "</a>".len()),
            None => (rest.len(), rest.len()),
        };
        let inner = &rest[tag_len..inner_end];
        out.push_str(&rest[..close]);
        if let Some(badge) = href.and_then(|href| link_target_badge(&href, inner)) {
            out.push_str(&badge);
        }
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
}

/// Length of the `<a …>` start tag at the beginning of `html` and its decoded `href`.
fn parse_start_tag(html: &str) -> Option<(usize, Option<String>)> {
    let bytes = html.as_bytes();
    let mut i = "<a".len();
    let mut href = None;
    loop {
        while bytes.get(i)?.is_ascii_whitespace() {
            i += 1;
        }
        match bytes[i] {
            b'>' => return Some((i + 1, href)),
            b'/' => i += 1,
            _ => {
                let name_start = i;
                while !matches!(bytes.get(i)?, b'=' | b'>' | b'/') && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                let name = &html[name_start..i];
                if bytes[i] != b'=' {
                    continue;
                }
                i += 1;
                let value = if bytes.get(i) == Some(&b'"') {
                    let len = html[i + 1..].find('"')?;
                    i += len + 2;
                    &html[i - len - 1..i - 1]
                } else {
                    let start = i;
                    while !matches!(bytes.get(i)?, b'>') && !bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    &html[start..i]
                };
                if name.eq_ignore_ascii_case("href") {
                    href = Some(html_escape::decode_html_entities(value).into_owned());
                }
            }
        }
    }
}

fn link_target_badge(href: &str, inner_html: &str) -> Option<String> {
    let url = url::Url::parse(href.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let text = visible_text(inner_html);
    let mismatch = match host_named_by(&text) {
        Some(named) if same_site(&named, host) => return None,
        Some(_) => true,
        None => false,
    };
    let style = if mismatch { format!("{LINK_TARGET_STYLE} {LINK_MISMATCH_STYLE}") } else { LINK_TARGET_STYLE.into() };
    Some(format!(
        "<span class=\"link-target{}\" style=\"{style}\" title=\"{}\">{}{}</span>",
        if mismatch { " mismatch" } else { "" },
        html_escape::encode_double_quoted_attribute(url.as_str()),
        if mismatch { "⚠ " } else { "" },
        html_escape::encode_text(host),
    ))
}

/// The text a link shows: its HTML without tags, entities decoded, whitespace collapsed.
fn visible_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    html_escape::decode_html_entities(&text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The host a link text names, if it reads like a web address (`https://…`, `www.…`, `example.org/x`).
fn host_named_by(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches(['.', ',', ';', ':', ')']);
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let lower = text.to_lowercase();
    let candidate = if lower.starts_with("http://") || lower.starts_with("https://") {
        text.to_string()
    } else {
        // A bare domain needs a dot and an alphabetic top-level label: not "1.5" or "e.g".
        let domain = lower.split(['/', '?', '#']).next().unwrap_or_default();
        let tld = domain.rsplit('.').next().unwrap_or_default();
        if !domain.contains('.') || tld.chars().count() < 2 || !tld.chars().all(char::is_alphabetic) || domain.contains('@') {
            return None;
        }
        format!("http://{text}")
    };
    url::Url::parse(&candidate).ok()?.host_str().map(str::to_string)
}

/// `actual` is the site the text `named`, or one of its subdomains; a leading `www.` doesn't count.
fn same_site(named: &str, actual: &str) -> bool {
    let named = named.strip_prefix("www.").unwrap_or(named);
    let actual = actual.strip_prefix("www.").unwrap_or(actual);
    actual == named || actual.ends_with(&format!(".{named}"))
}

/// The message as text for a source view, cut after `limit` bytes (at a line end where possible)
/// with a note saying how much is missing.
pub fn source_text(raw: &[u8], limit: usize) -> String {
    if raw.len() <= limit {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let cut = raw[..limit].iter().rposition(|&b| b == b'\n').map_or(limit, |i| i + 1);
    format!(
        "{}\n[… {} more bytes not shown. Save the message as .eml to see all of it.]",
        String::from_utf8_lossy(&raw[..cut]),
        raw.len() - cut
    )
}

/// A file name for saving a message: its subject without characters file systems reject.
pub fn eml_file_name(subject: &str) -> String {
    let name: String = subject
        .chars()
        .map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    let name = name.trim().trim_matches('.');
    let name: String = name.chars().take(100).collect();
    if name.is_empty() { "message.eml".into() } else { format!("{}.eml", name.trim_end()) }
}

/// Run by the app (never by the page, whose scripts the CSP sandbox blocks) after the user scrolled a
/// conversation: marks the message under the reading line as current and returns its index, or -1 when
/// there is nothing to follow. The line moves from the viewport's top to its bottom as the page scrolls
/// from start to end, so it passes every message, however short.
pub const SCROLLED_SCRIPT: &str = "(() => {\
    const s = document.querySelectorAll('section.message'), e = document.scrollingElement;\
    if (s.length < 2 || e.scrollHeight <= innerHeight) return -1;\
    const line = e.scrollTop / (e.scrollHeight - innerHeight) * innerHeight;\
    let n = 0;\
    s.forEach((m, i) => { if (m.getBoundingClientRect().top <= line) n = i; });\
    s.forEach((m, i) => m.classList.toggle('current', i === n));\
    return n;\
})()";

fn document(content: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><style>\
         :root {{ color-scheme: light; }}\
         body {{ margin: 16px; font-family: sans-serif; font-size: 14px; color: #1d1d1f; background: #fff; overflow-wrap: anywhere; }}\
         img {{ max-width: 100%; height: auto; }}\
         pre.plain {{ white-space: pre-wrap; font-family: monospace; font-size: 13px; }}\
         .quote {{ color: #6e6e73; }}\
         section.message {{ border-top: 1px solid #e0e0e3; padding: 12px 0 12px 15px; }}\
         section.message:first-child {{ border-top: none; }}\
         section.current {{ border-left: 3px solid #2f6fde; padding-left: 12px; }}\
         section h2 {{ font-size: 13px; font-weight: 600; color: #6e6e73; margin: 0 0 10px; }}\
         .loading {{ color: #6e6e73; }}\
         </style></head><body>{content}</body></html>"
    )
}

/// Content-Security-Policy for message documents. `sandbox` without
/// `allow-scripts` disables scripting entirely; the fetch directives are a second
/// layer behind the renderer's own request blocking.
pub fn content_security_policy(allow_remote_images: bool) -> String {
    let img = if allow_remote_images {
        "https: http: data:"
    } else {
        "data:"
    };
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
    /// Kept across saves, so a newer copy of a draft can replace the older one in Drafts.
    pub message_id: String,
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
    build(draft, false)
}

/// A draft for the Drafts folder: recipients may be missing, Bcc is kept.
pub fn build_draft(draft: &Draft) -> Result<BuiltMessage> {
    build(draft, true)
}

fn build(draft: &Draft, is_draft: bool) -> Result<BuiltMessage> {
    use mail_builder::MessageBuilder;
    use mail_builder::headers::address::Address as MbAddress;

    let to = parse_address_list(&draft.to);
    let cc = parse_address_list(&draft.cc);
    let bcc = parse_address_list(&draft.bcc);
    let recipients: Vec<String> = to
        .iter()
        .chain(&cc)
        .chain(&bcc)
        .map(|(_, a)| a.clone())
        .collect();
    if recipients.is_empty() && !is_draft {
        return Err(anyhow!("add at least one recipient"));
    }
    for r in &recipients {
        if !r.contains('@') || r.contains(char::is_whitespace) {
            return Err(anyhow!("invalid address: {r}"));
        }
    }
    let to_mb = |list: &[(String, String)]| {
        MbAddress::new_list(
            list.iter()
                .map(|(n, a)| {
                    MbAddress::new_address(
                        if n.is_empty() { None } else { Some(n.clone()) },
                        a.clone(),
                    )
                })
                .collect(),
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
    if is_draft && !bcc.is_empty() {
        builder = builder.bcc(to_mb(&bcc));
    }
    let strip = |s: &str| {
        s.split_whitespace()
            .map(|id| id.trim_matches(['<', '>']).to_string())
            .collect::<Vec<_>>()
    };
    if !draft.in_reply_to.is_empty() {
        builder = builder.in_reply_to(strip(&draft.in_reply_to));
    }
    if !draft.references.is_empty() {
        builder = builder.references(strip(&draft.references));
    }
    if let Some(id) = strip(&draft.message_id).pop() {
        builder = builder.message_id(id);
    }
    for path in &draft.attachments {
        let data = std::fs::read(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        builder = builder.attachment(guess_mime(&name), name, data);
    }
    let raw = builder.write_to_vec()?;
    Ok(BuiltMessage { raw, recipients })
}

fn guess_mime(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
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

/// Reads a saved draft back for editing. Attachments are left to the caller.
pub fn draft_from_raw(raw: &[u8]) -> Draft {
    let summary = summary_from_headers("", 0, raw, None);
    let bcc = MessageParser::default().parse_headers(raw).map(|m| format_addresses(m.bcc())).unwrap_or_default();
    Draft {
        to: summary.to,
        cc: summary.cc,
        bcc,
        subject: summary.subject,
        body: body_from_raw(raw).text.replace("\r\n", "\n"),
        in_reply_to: summary.in_reply_to,
        references: summary.references,
        message_id: summary.message_id,
        ..Default::default()
    }
}

/// A fresh Message-ID for a new draft, in the sender's domain.
pub fn new_message_id(from_addr: &str) -> String {
    let domain = from_addr.rsplit_once('@').map_or("localhost", |(_, d)| d);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("<{nanos:x}.{:x}@{domain}>", std::process::id())
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
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%a, %d %b %Y at %H:%M")
                .to_string()
        })
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
                    .filter(|(_, a)| {
                        !a.eq_ignore_ascii_case(me) && !a.eq_ignore_ascii_case(&original.from_addr)
                    })
                    .map(|(n, a)| {
                        if n.is_empty() {
                            a
                        } else {
                            format!("{n} <{a}>")
                        }
                    })
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
                body: format!(
                    "\n\nOn {}, {sender} wrote:\n{quoted}",
                    format_date(original.date)
                ),
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
        for bad in [
            "<script",
            "alert(1)",
            "onload",
            "javascript:",
            "<form",
            "<input",
            "<iframe",
            "target=",
        ] {
            assert!(!html.contains(bad), "{bad} survived: {html}");
        }
        assert!(
            html.contains("p{color:red}"),
            "style should be kept: {html}"
        );
        assert!(
            html.contains("https://message.invalid/cid/logo@x"),
            "{html}"
        );
        assert!(html.contains("noopener"));
        assert!(body.has_remote_content);
        assert_eq!(
            body.inline_parts.get("logo@x").map(|p| p.0.as_str()),
            Some("image/png")
        );
        assert!(
            body.attachments.is_empty(),
            "inline image is not an attachment"
        );
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
        let d = reply_draft(
            ReplyKind::ReplyAll,
            &original,
            "line1\nline2",
            "bob@example.org",
        );
        assert_eq!(d.to, "Alice <alice@example.org>");
        assert_eq!(d.cc, "carol@example.org");
        assert_eq!(d.subject, "Re: Grüße");
        assert_eq!(d.in_reply_to, "<abc@example.org>");
        assert!(d.body.contains("> line1\n> line2"));
        assert_eq!(
            reply_draft(ReplyKind::Reply, &original, "", "").subject,
            "Re: Grüße"
        );
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
        assert_eq!(
            built.recipients,
            vec!["you@example.org", "hidden@example.org"]
        );
        let raw = String::from_utf8(built.raw.clone()).unwrap();
        assert!(
            !raw.contains("hidden@example.org"),
            "Bcc must not appear in headers"
        );
        let parsed = parse(&built.raw).unwrap();
        assert_eq!(parsed.subject(), Some("Hello"));
        assert_eq!(parsed.in_reply_to().as_text(), Some("abc@example.org"));
        assert!(build_message(&Draft::default()).is_err());
    }

    #[test]
    fn parses_obsolete_ut_zone() {
        let raw = b"Date: Sat, 14 Sep 2002 06:00:48 UT\nFrom: A <a@b.c>\nSubject: x\n\nbody\n";
        let s = summary_from_headers("INBOX", 1, raw, None);
        assert_eq!(
            (s.from_addr.as_str(), s.subject.as_str(), s.date),
            ("a@b.c", "x", 1031983248)
        );
        assert!(body_from_raw(raw).text.contains("body"));
    }

    #[test]
    fn snippet_skips_table_borders() {
        assert_eq!(snippet("───┬───\nHello ─── world"), "Hello world");
    }

    #[test]
    fn html_to_text_has_no_markup() {
        assert_eq!(
            html_to_text("<h1>Hi</h1><p><b>there</b></p>")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["Hi", "there"]
        );
    }

    #[test]
    fn draft_roundtrip() {
        let draft = Draft {
            from_addr: "me@example.org".into(),
            bcc: "Hidden <hidden@example.org>".into(),
            subject: "Unfinished".into(),
            body: "First line\nsecond".into(),
            in_reply_to: "<a@x>".into(),
            references: "<r@x> <a@x>".into(),
            message_id: "<draft1@example.org>".into(),
            ..Default::default()
        };
        assert!(build_message(&Draft { bcc: String::new(), ..draft.clone() }).is_err(), "sending needs a recipient");
        let back = draft_from_raw(&build_draft(&draft).unwrap().raw);
        assert_eq!(back.to, "");
        assert_eq!(back.bcc, draft.bcc);
        assert_eq!(back.subject, draft.subject);
        assert_eq!(back.body.trim_end(), draft.body);
        assert_eq!((back.in_reply_to, back.references), (draft.in_reply_to, draft.references));
        assert_eq!(back.message_id, draft.message_id);
        assert!(new_message_id("me@example.org").ends_with("@example.org>"));
    }

    #[test]
    fn percent_roundtrip() {
        assert_eq!(percent_decode(&percent_encode("a b/c@d")), "a b/c@d");
    }

    #[test]
    fn renders_conversation_with_anchors() {
        let body = body_from_raw(b"Subject: x\r\n\r\nHello <there>\r\n");
        let doc = render_conversation(&[("Bob <b@x>".into(), Some(&body)), ("Me".into(), None)], 1);
        assert!(
            doc.contains("<section id=\"m0\" class=\"message\"><h2>Bob &lt;b@x&gt;</h2>"),
            "{doc}"
        );
        assert!(doc.contains("Hello &lt;there&gt;"));
        assert!(doc.contains(
            "<section id=\"m1\" class=\"message current\"><h2>Me</h2><p class=\"loading\">"
        ));
    }

    #[test]
    fn marks_link_targets() {
        let (html, _) = sanitize_html(
            "<style>a{color:red} /* <a href=\"https://css.example\">x</a> */</style>\
             <p><a href=\"https://tracker.example/c?u=1&amp;v=2\">Click <b>here</b></a> \
             <a href=\"https://paypal.com.evil.io/login\">www.paypal.com</a> \
             <a href=\"https://www.example.org/path\">example.org</a> \
             <a href=\"https://news.example.org/x\">https://example.org</a> \
             <a href=\"https://xn--pypal-4ve.com/\">see offer</a> \
             <a href=\"mailto:a@example.org\">mail us</a> <a>no href</a></p>",
        );
        let marked = mark_link_targets(&html);
        let badges: Vec<&str> = marked
            .split("<span class=\"link-target")
            .skip(1)
            .map(|b| &b[b.find('>').unwrap() + 1..b.find("</span>").unwrap()])
            .collect();
        assert_eq!(
            badges,
            ["tracker.example", "⚠ paypal.com.evil.io", "xn--pypal-4ve.com"],
            "{marked}"
        );
        assert!(marked.contains("link-target mismatch"), "{marked}");
        assert!(marked.contains("title=\"https://tracker.example/c?u=1&amp;v=2\""), "{marked}");
        // The style sheet is left alone; nothing else is changed but the badges.
        assert!(marked.contains("/* <a href=\"https://css.example\">x</a> */"), "{marked}");
        let without: String = marked
            .split("<span class=\"link-target")
            .enumerate()
            .map(|(i, part)| if i == 0 { part } else { &part[part.find("</span>").unwrap() + 7..] })
            .collect();
        assert_eq!(without, html);
    }

    #[test]
    fn link_target_helpers() {
        assert_eq!(host_named_by("WWW.PayPal.com/login").as_deref(), Some("www.paypal.com"));
        assert_eq!(host_named_by("https://a.example/x").as_deref(), Some("a.example"));
        assert_eq!(host_named_by("version 1.5"), None);
        assert_eq!(host_named_by("1.5"), None);
        assert_eq!(host_named_by("e.g."), None);
        assert_eq!(host_named_by("me@example.org"), None);
        assert!(same_site("example.org", "www.example.org"));
        assert!(same_site("www.example.org", "mail.example.org"));
        assert!(!same_site("example.org", "example.org.evil.io"));
        assert!(!same_site("example.org", "badexample.org"));
        // Unterminated markup never loops or panics.
        for broken in ["<a href=\"https://x.example\">ü", "<a href=", "<a href=x", "<a  ", "<a x=y>"] {
            mark_link_targets(broken);
        }
        assert!(mark_link_targets("<a href=https://x.example>t</a>").contains(">x.example</span>"));
    }

    #[test]
    fn source_text_and_file_names() {
        assert_eq!(source_text(b"a\r\nb", 100), "a\r\nb");
        let cut = source_text(b"line one\nline two\nline three", 12);
        assert!(cut.starts_with("line one\n\n[…"), "{cut}");
        assert!(cut.contains("19 more bytes"), "{cut}");
        assert_eq!(eml_file_name("Re: a/b? <c>"), "Re_ a_b_ _c_.eml");
        assert_eq!(eml_file_name("  ..  "), "message.eml");
        assert_eq!(eml_file_name(""), "message.eml");
        assert_eq!(eml_file_name(&"x".repeat(300)).len(), 104);
    }
}
