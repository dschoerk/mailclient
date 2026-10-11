//! A message as a printable PDF: subject, headers, text and attachment names on A4 pages, set
//! in a system font that is embedded (as a subset), so the text stays selectable and searchable.
//!
//! shortcut: the text rendition only: HTML layout, colors and images are not reproduced. Lines are
//! laid out without shaping or font fallback, so scripts that need either (Arabic, Indic) or
//! characters missing from the font print poorly.

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, UnicodeCmap};
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str, TextStr};

/// What goes on paper, already formatted for reading.
#[derive(Debug, Clone, Default)]
pub struct Printable {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub date: String,
    pub text: String,
    /// Names (and sizes) of the attachments, listed at the end.
    pub attachments: Vec<String>,
}

/// A TrueType font: the file's bytes and the face's index in it (collections hold several).
pub struct Font {
    pub data: Vec<u8>,
    pub index: u32,
}

const PAGE_WIDTH: f32 = 595.0;
const PAGE_HEIGHT: f32 = 842.0;
const MARGIN: f32 = 56.0;
/// Room under the text for the page number.
const FOOTER: f32 = 24.0;
const BODY_SIZE: f32 = 10.5;
const HEADER_SIZE: f32 = 9.5;
const SUBJECT_SIZE: f32 = 16.0;
const LINE_HEIGHT: f32 = 1.35;
const FONT_NAME: Name = Name(b"F1");
const IDENTITY: SystemInfo = SystemInfo { registry: Str(b"Adobe"), ordering: Str(b"Identity"), supplement: 0 };

/// A sans-serif TrueType font from the system, looked up once.
pub fn system_font() -> Result<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();
    FONT.get_or_init(find_system_font).as_ref().ok_or_else(|| anyhow!("no usable TrueType font found on this system"))
}

fn find_system_font() -> Option<Font> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    // fontdb has no Android font directory of its own.
    #[cfg(target_os = "android")]
    db.load_fonts_dir("/system/fonts");
    let usable = |id| {
        db.with_face_data(id, |data, index| {
            let face = ttf_parser::Face::parse(data, index).ok()?;
            // CFF-flavored fonts would need another embedding; the common system fonts are TrueType.
            let truetype = face.tables().glyf.is_some();
            let latin = "Aaäß€".chars().all(|c| face.glyph_index(c).is_some());
            (truetype && latin).then(|| Font { data: data.to_vec(), index })
        })
        .flatten()
    };
    let names = ["DejaVu Sans", "Noto Sans", "Roboto", "Liberation Sans", "Arimo", "Arial", "Segoe UI", "FreeSans"];
    let families = names.iter().map(|n| fontdb::Family::Name(n)).chain([fontdb::Family::SansSerif]);
    for family in families {
        let query = fontdb::Query { families: &[family], ..Default::default() };
        if let Some(font) = db.query(&query).and_then(usable) {
            return Some(font);
        }
    }
    db.faces().find_map(|f| usable(f.id))
}

/// One line on the page.
enum Item {
    Text { text: String, size: f32, gray: f32 },
    Gap(f32),
    Rule,
}

/// The PDF of `message`, set in `font`.
pub fn message_pdf(message: &Printable, font: &Font) -> Result<Vec<u8>> {
    let face = ttf_parser::Face::parse(&font.data, font.index).context("reading the font")?;
    let units = f32::from(face.units_per_em());
    let measure = |text: &str, size: f32| -> f32 {
        text.chars()
            .map(|c| face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g)).map_or(0.0, f32::from))
            .sum::<f32>()
            * size
            / units
    };
    let width = PAGE_WIDTH - 2.0 * MARGIN;
    let mut items = Vec::new();
    let add = |items: &mut Vec<Item>, text: &str, size: f32, gray: f32| {
        for line in wrap(text, width, &|t| measure(t, size)) {
            items.push(Item::Text { text: line, size, gray });
        }
    };
    let subject = if message.subject.trim().is_empty() { "(no subject)" } else { message.subject.as_str() };
    add(&mut items, subject, SUBJECT_SIZE, 0.0);
    items.push(Item::Gap(4.0));
    for (label, value) in [("From", &message.from), ("To", &message.to), ("Cc", &message.cc), ("Date", &message.date)] {
        if !value.is_empty() {
            add(&mut items, &format!("{label}: {value}"), HEADER_SIZE, 0.3);
        }
    }
    items.push(Item::Rule);
    add(&mut items, message.text.trim_end(), BODY_SIZE, 0.0);
    if !message.attachments.is_empty() {
        items.push(Item::Rule);
        add(&mut items, "Attachments:", HEADER_SIZE, 0.3);
        for name in &message.attachments {
            add(&mut items, &format!("  {name}"), HEADER_SIZE, 0.3);
        }
    }

    // Lay out pages first: the footer says how many there are.
    let mut pages: Vec<Vec<(f32, &Item)>> = vec![Vec::new()];
    let top = PAGE_HEIGHT - MARGIN;
    let bottom = MARGIN + FOOTER;
    let mut y = top;
    for item in &items {
        let height = match item {
            Item::Text { size, .. } => size * LINE_HEIGHT,
            Item::Gap(h) => *h,
            Item::Rule => 14.0,
        };
        if y - height < bottom && !pages.last().is_some_and(Vec::is_empty) {
            pages.push(Vec::new());
            y = top;
            // A gap or rule at the top of a page is noise.
            if !matches!(item, Item::Text { .. }) {
                continue;
            }
        }
        y -= height;
        pages.last_mut().expect("one page at least").push((y, item));
    }

    let mut glyphs = Glyphs::new(&face);
    let mut pdf = Pdf::new();
    let catalog_id = Ref::new(1);
    let tree_id = Ref::new(2);
    let font_id = Ref::new(3);
    let cid_id = Ref::new(4);
    let descriptor_id = Ref::new(5);
    let file_id = Ref::new(6);
    let cmap_id = Ref::new(7);
    let info_id = Ref::new(8);
    let first_page = 9;
    let page_ids: Vec<Ref> = (0..pages.len()).map(|i| Ref::new(first_page + 2 * i as i32)).collect();
    pdf.catalog(catalog_id).pages(tree_id);
    pdf.pages(tree_id).kids(page_ids.iter().copied()).count(pages.len() as i32);
    pdf.document_info(info_id).title(TextStr(subject)).creator(TextStr("mailclient"));
    for (n, (lines, &page_id)) in pages.iter().zip(&page_ids).enumerate() {
        let content_id = Ref::new(page_id.get() + 1);
        let mut content = Content::new();
        for (y, item) in lines {
            match item {
                Item::Text { text, size, gray } => {
                    let bytes = glyphs.encode(text);
                    content.begin_text().set_font(FONT_NAME, *size).set_fill_gray(*gray).next_line(MARGIN, *y);
                    content.show(Str(&bytes)).end_text();
                }
                Item::Rule => {
                    content.set_stroke_gray(0.75).set_line_width(0.5);
                    content.move_to(MARGIN, y + 6.0).line_to(PAGE_WIDTH - MARGIN, y + 6.0).stroke();
                }
                Item::Gap(_) => {}
            }
        }
        let number = format!("{} / {}", n + 1, pages.len());
        let bytes = glyphs.encode(&number);
        let x = PAGE_WIDTH - MARGIN - measure(&number, 8.0);
        content.begin_text().set_font(FONT_NAME, 8.0).set_fill_gray(0.45).next_line(x, MARGIN - 4.0);
        content.show(Str(&bytes)).end_text();
        let content = content.finish();
        pdf.stream(content_id, &content);
        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, PAGE_WIDTH, PAGE_HEIGHT)).parent(tree_id).contents(content_id);
        page.resources().fonts().pair(FONT_NAME, font_id);
        page.finish();
    }

    // The font, cut down to the glyphs used. Glyph ids are renumbered by the subset; the text
    // above already uses the new numbers, which are also the CIDs (Identity mapping).
    let subset = subsetter::subset(&font.data, font.index, &glyphs.remapper).map_err(|e| anyhow!("subsetting the font: {e:?}"))?;
    let scale = |v: i16| f32::from(v) * 1000.0 / units;
    let postscript = face
        .names()
        .into_iter()
        .find(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .and_then(|n| n.to_string())
        .unwrap_or_else(|| "Font".into());
    let postscript: String = postscript.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    // A subset's name carries a six-letter tag.
    let base_font = format!("MAILCL+{postscript}");
    let base_font = Name(base_font.as_bytes());
    pdf.type0_font(font_id)
        .base_font(base_font)
        .encoding_predefined(Name(b"Identity-H"))
        .descendant_font(cid_id)
        .to_unicode(cmap_id);
    let mut cid = pdf.cid_font(cid_id);
    cid.subtype(CidFontType::Type2)
        .base_font(base_font)
        .system_info(IDENTITY)
        .font_descriptor(descriptor_id)
        .default_width(0.0)
        .cid_to_gid_map_predefined(Name(b"Identity"));
    cid.widths().consecutive(0, glyphs.widths.iter().map(|w| w * 1000.0 / units));
    cid.finish();
    let bbox = face.global_bounding_box();
    pdf.font_descriptor(descriptor_id)
        .name(base_font)
        .flags(FontFlags::NON_SYMBOLIC)
        .bbox(Rect::new(scale(bbox.x_min), scale(bbox.y_min), scale(bbox.x_max), scale(bbox.y_max)))
        .italic_angle(0.0)
        .ascent(scale(face.ascender()))
        .descent(scale(face.descender()))
        .cap_height(scale(face.capital_height().unwrap_or(face.ascender())))
        .stem_v(80.0)
        .font_file2(file_id);
    pdf.stream(file_id, &subset).pair(Name(b"Length1"), subset.len() as i32);
    let cmap = glyphs.cmap.finish();
    pdf.cmap(cmap_id, &cmap).name(Name(b"Custom")).system_info(IDENTITY);
    Ok(pdf.finish())
}

/// The glyphs used so far, numbered as in the subset font.
struct Glyphs<'f> {
    face: &'f ttf_parser::Face<'f>,
    remapper: subsetter::GlyphRemapper,
    /// Advance of each new glyph id, in font units.
    widths: Vec<f32>,
    cmap: UnicodeCmap<u16>,
}

impl<'f> Glyphs<'f> {
    fn new(face: &'f ttf_parser::Face<'f>) -> Self {
        let mut glyphs = Glyphs {
            face,
            remapper: subsetter::GlyphRemapper::new(),
            widths: Vec::new(),
            cmap: UnicodeCmap::new(Name(b"Custom"), IDENTITY),
        };
        // Glyph 0 (.notdef) keeps its number: fonts need it first.
        glyphs.glyph(ttf_parser::GlyphId(0), None);
        glyphs
    }

    fn glyph(&mut self, old: ttf_parser::GlyphId, c: Option<char>) -> u16 {
        let new = self.remapper.remap(old.0);
        if usize::from(new) == self.widths.len() {
            self.widths.push(self.face.glyph_hor_advance(old).map_or(0.0, f32::from));
            if let Some(c) = c {
                self.cmap.pair(new, c);
            }
        }
        new
    }

    /// `text` as two-byte glyph ids (Identity-H).
    fn encode(&mut self, text: &str) -> Vec<u8> {
        text.chars()
            .flat_map(|c| {
                let old = self.face.glyph_index(c).unwrap_or(ttf_parser::GlyphId(0));
                let c = (old.0 != 0).then_some(c);
                self.glyph(old, c).to_be_bytes()
            })
            .collect()
    }
}

/// Breaks `text` into lines no wider than `width`: at spaces where possible, inside words that do
/// not fit a line on their own. Tabs become spaces, other control characters are dropped.
fn wrap(text: &str, width: f32, measure: &dyn Fn(&str) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let paragraph: String =
            paragraph.replace('\t', "    ").chars().filter(|c| !c.is_control()).collect();
        let mut line = String::new();
        for word in paragraph.split_inclusive(' ') {
            if measure(format!("{line}{word}").trim_end()) <= width {
                line.push_str(word);
                continue;
            }
            if !line.trim().is_empty() {
                lines.push(std::mem::take(&mut line).trim_end().to_string());
            }
            line.clear();
            // A word longer than a line: as many characters per line as fit.
            for c in word.chars() {
                if !line.is_empty() && measure(format!("{line}{c}").trim_end()) > width {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_spaces_and_inside_long_words() {
        // Every character one unit wide.
        let measure = |t: &str| t.chars().count() as f32;
        assert_eq!(wrap("aa bb cc", 5.0, &measure), ["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh x", 3.0, &measure), ["abc", "def", "gh", "x"]);
        assert_eq!(wrap("a\n\n  b\tc", 10.0, &measure), ["a", "", "  b    c"]);
        assert_eq!(wrap("", 10.0, &measure), Vec::<String>::new());
    }

    #[test]
    fn writes_a_pdf_with_the_text() {
        let Ok(font) = system_font() else {
            eprintln!("no system font, skipped");
            return;
        };
        let long: String = (0..400).map(|i| format!("Line {i} of a long message, Grüße.\n")).collect();
        let message = Printable {
            subject: "Quarterly report".into(),
            from: "Alice <alice@example.org>".into(),
            to: "bob@example.org".into(),
            date: "Thu, 8 Oct 2026 10:00".into(),
            text: long,
            attachments: vec!["report.pdf  12 KB".into()],
            ..Default::default()
        };
        let pdf = message_pdf(&message, font).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(pdf.starts_with(b"%PDF-"));
        assert!(text.contains("/FontFile2"), "font embedded");
        let pages: usize = text.split("/Count ").nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).unwrap();
        assert!((8..=10).contains(&pages), "{pages} pages");
        assert!(text.contains("beginbfchar") || text.contains("beginbfrange"), "text is extractable");
        if let Some(path) = std::env::var_os("MAILCLIENT_PDF_SAMPLE") {
            std::fs::write(path, &pdf).unwrap();
        }
        // The subset is much smaller than the whole font.
        assert!(pdf.len() < font.data.len().max(200_000), "{} bytes", pdf.len());
    }
}
