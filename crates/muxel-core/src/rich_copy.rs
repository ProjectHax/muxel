//! "Copy as rich text" for a rendered Markdown pane: the whole document as HTML
//! that survives a paste into a mail client (Outlook's Word engine, Gmail).
//!
//! Parsing matches the rendered view: GFM (tables, task lists, strikethrough,
//! autolinks, footnotes), raw HTML included, minus the tags GFM disallows
//! (`<script>`, `<style>`, …). Every tag of the compiled HTML is then parsed
//! and written out again for mail. Mail clients drop stylesheets, classes and
//! form controls, and a relative path points at nothing once pasted, so
//! presentation goes inline and the rest gets a text stand-in.
//!
//! That rewrite is also what makes raw HTML safe to hand to another program.
//! Only listed elements and attributes are written back, link and image URLs
//! must be on the web or a mail address, and any other element loses its tags
//! but keeps its text, as it does in the rendered view. A `<` that doesn't open
//! a whole tag is written as text.

use markdown::{CompileOptions, Options, ParseOptions};
use std::fmt::Write as _;

/// One font stack for code, inline and in blocks.
const MONO: &str = "font-family:Consolas,Menlo,'Courier New',monospace;font-size:90%";
/// Outlook's Word engine honours the presentational `border`/`cellpadding`
/// attributes more reliably than CSS, so tables carry both.
const TABLE: &str = r#"<table border="1" cellspacing="0" cellpadding="6" style="border-collapse:collapse;border:1px solid #d0d7de">"#;
const CELL: &str = "border:1px solid #d0d7de;padding:4px 8px";
const HEADER_CELL_SHADE: &str = ";background-color:#f6f8fa";
const BLOCKQUOTE: &str = r#"<blockquote style="margin:0 0 0 0.8ex;border-left:3px solid #d0d7de;padding-left:1ex;color:#57606a">"#;

/// Elements written back as themselves, keeping only an `id` (a fragment
/// link's target) and, on the block ones HTML gave it to, `align`.
const KEPT: &[&str] = &[
    "b", "br", "caption", "dd", "div", "dl", "dt", "em", "h1", "h2", "h3", "h4", "h5", "h6", "hr",
    "i", "li", "p", "strong", "sub", "sup", "tbody", "tfoot", "thead", "tr", "u", "ul",
];
const ALIGNED: &[&str] = &["div", "h1", "h2", "h3", "h4", "h5", "h6", "p"];
/// Elements the rewrite restyles, besides the kept ones.
const RESTYLED: &[&str] = &["blockquote", "code", "mark", "ol", "table", "td", "th"];
/// Block containers a mail client may not know, written as `<div>` so their
/// content still starts on its own line.
const AS_DIV: &[&str] = &[
    "address",
    "article",
    "aside",
    "center",
    "details",
    "dialog",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hgroup",
    "main",
    "nav",
    "section",
    "summary",
];
/// Elements with no end tag.
const VOID: &[&str] = &["br", "hr", "img", "input"];
/// URL schemes a link keeps. An image keeps only the web ones.
const LINK_SCHEMES: &[&str] = &["http", "https", "mailto"];
const IMAGE_SCHEMES: &[&str] = &["http", "https"];

/// The Markdown document as mail-safe HTML (a fragment, no `<html>` wrapper).
///
/// `image_label` names a local image's text stand-in (`[Image: alt]`); the
/// caller passes it translated.
pub fn markdown_to_html(markdown: &str, image_label: &str) -> String {
    let options = Options {
        parse: ParseOptions::gfm(),
        compile: CompileOptions {
            allow_dangerous_html: true,
            ..CompileOptions::gfm()
        },
    };
    // markdown-rs only fails on MDX, which GFM leaves off.
    let html = markdown::to_html_with_options(markdown, &options)
        .unwrap_or_else(|_| format!("<pre>{}</pre>", escape_text(markdown)));
    for_mail(&html, image_label)
}

/// Rebuild every tag for mail; the text between tags passes through.
///
/// markdown-rs encodes `&`, `"`, `<` and `>` in text and attribute values, so
/// every `<` that starts a tag is markup it generated or raw HTML the author
/// wrote, never document text.
fn for_mail(html: &str, image_label: &str) -> String {
    let mut mail = Mail {
        out: String::with_capacity(html.len() + html.len() / 4),
        image_label,
        in_pre: false,
        links: Vec::new(),
    };
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        mail.out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if let Some(body) = rest.strip_prefix("<!--") {
            // Comments never show in the rendered view. Dropping them also keeps
            // an author's comment from posing as a CF_HTML fragment marker.
            let end = if body.starts_with('>') {
                Some(1)
            } else if body.starts_with("->") {
                Some(2)
            } else {
                body.find("-->").map(|end| end + 3)
            };
            rest = end.map_or("", |end| &body[end..]);
            continue;
        }
        match Tag::scan(rest) {
            Some((tag, len)) => {
                if let Some(tag) = tag {
                    mail.tag(&tag);
                }
                rest = &rest[len..];
            }
            None => {
                mail.out.push_str("&lt;");
                rest = &rest[1..];
            }
        }
    }
    mail.out.push_str(rest);
    mail.out
}

struct Mail<'a> {
    out: String,
    image_label: &'a str,
    /// Inside `<pre>`, whose style covers its `<code>`.
    in_pre: bool,
    /// For each open `<a>`, whether its start tag was written.
    links: Vec<bool>,
}

impl Mail<'_> {
    fn tag(&mut self, tag: &Tag) {
        let name = tag.name.as_str();
        if tag.end {
            self.end(name);
            return;
        }
        match name {
            "a" => self.link(tag),
            "img" => self.image(tag),
            // Gmail strips form controls, which would lose a task's state.
            "input"
                if tag
                    .attr("type")
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("checkbox")) =>
            {
                self.out.push(if tag.attr("checked").is_some() {
                    '☑'
                } else {
                    '☐'
                });
            }
            "table" => self.out.push_str(TABLE),
            "th" | "td" => self.cell(tag),
            "pre" => {
                self.in_pre = true;
                let _ = write!(
                    self.out,
                    r#"<pre style="{MONO};background-color:#f6f8fa;padding:8px 12px;white-space:pre-wrap">"#
                );
            }
            // A fenced block's language class means nothing without a
            // stylesheet; its `<pre>` carries the styling.
            "code" if self.in_pre => self.out.push_str("<code>"),
            "code" => {
                let _ = write!(
                    self.out,
                    r#"<code style="{MONO};background-color:#f6f8fa;padding:1px 4px">"#
                );
            }
            "blockquote" => self.out.push_str(BLOCKQUOTE),
            // `<s>` is plain strikethrough; `<del>` marks an edit, which a word
            // processor's HTML import may take as a tracked change instead.
            "del" | "s" => self.out.push_str("<s>"),
            "mark" => self
                .out
                .push_str(r#"<mark style="background-color:#fff8c5">"#),
            "ol" => {
                self.out.push_str("<ol");
                let start = tag
                    .attr("start")
                    .filter(|n| is_number(n.strip_prefix('-').unwrap_or(n), 9));
                if let Some(start) = start {
                    let _ = write!(self.out, r#" start="{start}""#);
                }
                self.out.push('>');
            }
            _ if KEPT.contains(&name) => {
                let _ = write!(self.out, "<{name}");
                if let Some(id) = tag.attr("id") {
                    push_attr(&mut self.out, "id", id);
                }
                if ALIGNED.contains(&name)
                    && let Some(align) = tag.attr("align").and_then(alignment)
                {
                    let _ = write!(self.out, r#" align="{align}""#);
                }
                self.out
                    .push_str(if VOID.contains(&name) { " />" } else { ">" });
            }
            _ if AS_DIV.contains(&name) => self.out.push_str("<div>"),
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "a" => {
                if self.links.pop() == Some(true) {
                    self.out.push_str("</a>");
                }
            }
            "pre" => {
                self.in_pre = false;
                self.out.push_str("</pre>");
            }
            "del" | "s" => self.out.push_str("</s>"),
            _ if VOID.contains(&name) => {}
            _ if KEPT.contains(&name) || RESTYLED.contains(&name) => {
                let _ = write!(self.out, "</{name}>");
            }
            _ if AS_DIV.contains(&name) => self.out.push_str("</div>"),
            _ => {}
        }
    }

    /// A table cell with borders, keeping its alignment and span.
    fn cell(&mut self, tag: &Tag) {
        let _ = write!(self.out, "<{}", tag.name);
        if let Some(align) = tag.attr("align").and_then(alignment) {
            let _ = write!(self.out, r#" align="{align}""#);
        }
        for span in ["colspan", "rowspan"] {
            if let Some(n) = tag.attr(span).filter(|n| is_number(n, 4)) {
                let _ = write!(self.out, r#" {span}="{n}""#);
            }
        }
        let shade = if tag.name == "th" {
            HEADER_CELL_SHADE
        } else {
            ""
        };
        let _ = write!(self.out, r#" style="{CELL}{shade}">"#);
    }

    /// A link keeps its tag when the reader can follow it: a web or mail URL,
    /// or a fragment of this document. Anything else (a path that resolves
    /// only beside the Markdown file, a script, a `data:` URI) leaves its text,
    /// the way a local image leaves its alt text. An `id` keeps the tag as a
    /// fragment's target.
    fn link(&mut self, tag: &Tag) {
        let href = tag
            .attr("href")
            .map(clean_url)
            .filter(|url| url.starts_with('#') || has_scheme(url, LINK_SCHEMES));
        let id = tag.attr("id");
        self.links.push(href.is_some() || id.is_some());
        if href.is_none() && id.is_none() {
            return;
        }
        self.out.push_str("<a");
        if let Some(href) = &href {
            let _ = write!(self.out, r#" href="{}""#, escape_text(href));
            if let Some(title) = tag.attr("title") {
                push_attr(&mut self.out, "title", title);
            }
        }
        if let Some(id) = id {
            push_attr(&mut self.out, "id", id);
        }
        self.out.push('>');
    }

    /// A web image stays: Outlook embeds it on paste and Gmail proxies it.
    /// Anything else (a relative or absolute local path, a `data:` URI) becomes
    /// its alt text, since neither client can show it and classic Outlook drops
    /// `data:` images.
    fn image(&mut self, tag: &Tag) {
        let src = tag.attr("src").map(clean_url).unwrap_or_default();
        if has_scheme(&src, IMAGE_SCHEMES) {
            let _ = write!(self.out, r#"<img src="{}""#, escape_text(&src));
            for attr in ["alt", "title"] {
                if let Some(value) = tag.attr(attr) {
                    push_attr(&mut self.out, attr, value);
                }
            }
            for attr in ["width", "height"] {
                let size = tag
                    .attr(attr)
                    .filter(|v| is_number(v.strip_suffix('%').unwrap_or(v), 5));
                if let Some(size) = size {
                    let _ = write!(self.out, r#" {attr}="{size}""#);
                }
            }
            self.out.push_str(" />");
            return;
        }
        let name = match tag.attr("alt").filter(|alt| !alt.trim().is_empty()) {
            Some(alt) => Some(as_written(alt)),
            None if scheme(&src).as_deref() == Some("data") => None,
            None => src
                .rsplit(['/', '\\'])
                .next()
                .filter(|file| !file.is_empty())
                .map(escape_text),
        };
        let label = escape_text(self.image_label);
        let _ = match name {
            Some(name) => write!(self.out, "<em>[{label}: {name}]</em>"),
            None => write!(self.out, "<em>[{label}]</em>"),
        };
    }
}

/// One tag as written: its lowercased name and its attributes in order, with
/// quotes stripped and character references left alone.
struct Tag<'a> {
    name: String,
    end: bool,
    attrs: Vec<(&'a str, &'a str)>,
}

impl<'a> Tag<'a> {
    /// The tag at the start of `s` (which starts with `<`) and its length.
    /// `None` when the `<` is text: not followed by a name, or not closed by
    /// `>` before the next `<`. The tag itself is `None` for a declaration or
    /// processing instruction (`<!DOCTYPE …>`, `<?…?>`), which a browser hides.
    ///
    /// Stopping at the next `<` keeps the rewrite linear. Otherwise a run of
    /// unfinished tags would have each one scan the rest of the document.
    /// HTML allows a `<` inside a quoted attribute value; markdown-rs never
    /// writes one, and an author who does gets the tag back as text.
    fn scan(s: &'a str) -> Option<(Option<Self>, usize)> {
        let b = s.as_bytes();
        let end = b.get(1) == Some(&b'/');
        let name_start = 1 + usize::from(end);
        if !b.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            if !matches!(b.get(1), Some(b'!' | b'?' | b'/')) {
                return None;
            }
            let close = 1 + s[1..].find(['<', '>'])?;
            return (b[close] == b'>').then_some((None, close + 1));
        }
        let stops = |c: u8| c.is_ascii_whitespace() || matches!(c, b'/' | b'>' | b'<');
        let mut i = name_start;
        while i < b.len() && !stops(b[i]) {
            i += 1;
        }
        let name = s[name_start..i].to_ascii_lowercase();
        let mut attrs = Vec::new();
        loop {
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
                i += 1;
            }
            match b.get(i) {
                None | Some(b'<') => return None,
                Some(b'>') => break,
                Some(_) => {}
            }
            let attr_start = i;
            while i < b.len() && !stops(b[i]) && b[i] != b'=' {
                i += 1;
            }
            let attr = &s[attr_start..i];
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let mut value = "";
            if b.get(i) == Some(&b'=') {
                i += 1;
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                if let Some(&quote @ (b'"' | b'\'')) = b.get(i) {
                    let len = s[i + 1..].find([char::from(quote), '<'])?;
                    if b[i + 1 + len] == b'<' {
                        return None;
                    }
                    value = &s[i + 1..i + 1 + len];
                    i += len + 2;
                } else {
                    let start = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'>' | b'<')
                    {
                        i += 1;
                    }
                    value = &s[start..i];
                }
            }
            if !attr.is_empty() {
                attrs.push((attr, value));
            }
        }
        Some((Some(Tag { name, end, attrs }), i + 1))
    }

    /// An attribute's value as written. The first one counts, as in a browser.
    fn attr(&self, want: &str) -> Option<&'a str> {
        self.attrs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(want))
            .map(|&(_, value)| value)
    }
}

/// ` name="value"` for a text value (`id`, `title`, `alt`) as written. Its
/// character references stay for the reader to decode; in text they can't
/// become markup.
fn push_attr(out: &mut String, name: &str, value: &str) {
    let _ = write!(out, r#" {name}="{}""#, as_written(value));
}

/// Text as written, safe inside a double-quoted attribute or between tags.
fn as_written(value: &str) -> String {
    value
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A URL attribute as a browser's URL parser reads it: character references
/// decoded, surrounding spaces and control characters trimmed, tabs and line
/// breaks taken out. The rewrite checks this form and writes it back escaped,
/// so the reader follows exactly the URL that was checked.
fn clean_url(value: &str) -> String {
    decode_references(value)
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// `url`'s scheme, lowercased; `None` for a relative URL.
fn scheme(url: &str) -> Option<String> {
    let end = url.find([':', '/', '?', '#'])?;
    url[end..]
        .starts_with(':')
        .then(|| url[..end].to_ascii_lowercase())
}

fn has_scheme(url: &str, schemes: &[&str]) -> bool {
    scheme(url).is_some_and(|scheme| schemes.contains(&scheme.as_str()))
}

/// `value` with the character references decoded that a browser decodes in an
/// attribute and a script URL can hide behind: numeric ones (`&#106;`,
/// `&#x6A;`, with or without the `;`) and the five XML ones. Any other named
/// reference (`&colon;`) stays literal, since the value is written back with
/// its `&` escaped.
fn decode_references(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let (decoded, len) = reference(rest).unwrap_or(('&', 1));
        out.push(decoded);
        rest = &rest[len..];
    }
    out.push_str(rest);
    out
}

/// The character the reference at the start of `s` stands for, and the
/// reference's length.
fn reference(s: &str) -> Option<(char, usize)> {
    const NAMED: [(&str, char); 5] = [
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&apos;", '\''),
    ];
    if let Some(&(name, decoded)) = NAMED.iter().find(|(name, _)| s.starts_with(name)) {
        return Some((decoded, name.len()));
    }
    let (start, radix) = if s.starts_with("&#x") || s.starts_with("&#X") {
        (3, 16)
    } else if s.starts_with("&#") {
        (2, 10)
    } else {
        return None;
    };
    let digits = s[start..]
        .bytes()
        .take_while(|&b| char::from(b).is_digit(radix))
        .count();
    if digits == 0 {
        return None;
    }
    let end = start + digits;
    let code = s[start..end].bytes().fold(0u32, |code, b| {
        code.saturating_mul(radix)
            .saturating_add(char::from(b).to_digit(radix).unwrap_or(0))
    });
    let decoded = char::from_u32(code)
        .filter(|&c| c != '\0')
        .unwrap_or(char::REPLACEMENT_CHARACTER);
    Some((decoded, end + usize::from(s[end..].starts_with(';'))))
}

fn alignment(value: &str) -> Option<&'static str> {
    ["left", "center", "right", "justify"]
        .into_iter()
        .find(|align| value.eq_ignore_ascii_case(align))
}

/// One to `max` ASCII digits.
fn is_number(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::markdown_to_html;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The document as mail HTML, held to the clipboard's safety rule.
    fn html(markdown: &str) -> String {
        let out = markdown_to_html(markdown, "Image");
        assert_safe(&out);
        out
    }

    /// The safety rule, read off the output alone: every `<` opens a tag
    /// written `</name>` or `<name( attr="value")*( /)?>`, with no event
    /// handler, no `<` or `>` inside a value, and every link or image URL on
    /// the web, a mail address or a fragment of the document. A browser parses
    /// markup of that shape one way only.
    fn assert_safe(html: &str) {
        for (at, _) in html.match_indices('<') {
            let len = html[at..]
                .find('>')
                .unwrap_or_else(|| panic!("unclosed `<` at {at} in:\n{html}"));
            let tag = &html[at..=at + len];
            let inner = &tag[1..tag.len() - 1];
            let inner = inner.strip_prefix('/').unwrap_or(inner);
            let inner = inner.strip_suffix(" /").unwrap_or(inner);
            let (name, mut attrs) = inner.split_once(' ').unwrap_or((inner, ""));
            assert!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "`<` that opens no tag the export writes: {tag} in:\n{html}"
            );
            while !attrs.is_empty() {
                let (attr, rest) = attrs
                    .split_once("=\"")
                    .unwrap_or_else(|| panic!("unquoted attribute in {tag}"));
                let (value, rest) = rest.split_once('"').unwrap();
                assert!(
                    attr.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                    "attribute {attr:?} in {tag}"
                );
                assert!(!attr.starts_with("on"), "event handler in {tag}");
                assert!(!value.contains(['<', '>']), "markup inside {tag}");
                if attr == "href" || attr == "src" {
                    let url = value
                        .replace("&quot;", "\"")
                        .replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&amp;", "&")
                        .to_ascii_lowercase();
                    assert!(
                        ["http://", "https://", "mailto:", "#"]
                            .iter()
                            .any(|scheme| url.starts_with(scheme)),
                        "URL {url:?} in {tag}"
                    );
                }
                attrs = if rest.is_empty() {
                    rest
                } else {
                    rest.strip_prefix(' ')
                        .unwrap_or_else(|| panic!("attributes run together in {tag}"))
                };
            }
        }
    }

    /// Every opening `<name …>` tag in `html`.
    fn open_tags<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
        html.match_indices('<')
            .map(|(i, _)| &html[i..])
            .filter(|s| {
                s[1..].starts_with(name) && s[1 + name.len()..].starts_with([' ', '>', '/'])
            })
            .map(|s| &s[..=s.find('>').unwrap()])
            .collect()
    }

    // Expected markup is the GFM spec's output for these constructs
    // (https://github.github.com/gfm/), except where mail needs a change.
    #[test]
    fn keeps_the_elements_a_mail_client_understands() {
        let out = html(
            "# Title\n\nSome **bold**, *italic*, ~~gone~~ and a [link](https://example.com).\n\n\
             - one\n- two\n\n1. first\n2. second\n\n> quoted\n\n---\n\nSee https://auto.example.org\n",
        );
        for expected in [
            "<h1>Title</h1>",
            "<strong>bold</strong>",
            "<em>italic</em>",
            r#"<a href="https://example.com">link</a>"#,
            "<ul>\n<li>one</li>\n<li>two</li>\n</ul>",
            "<ol>\n<li>first</li>\n<li>second</li>\n</ol>",
            "<p>quoted</p>",
            "<hr />",
            r#"<a href="https://auto.example.org">https://auto.example.org</a>"#,
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        // Strikethrough as `<s>`, never `<del>`.
        assert!(out.contains("<s>gone</s>"), "{out}");
        assert!(!out.contains("<del"), "{out}");
        assert_eq!(open_tags(&out, "blockquote").len(), 1);
        assert!(open_tags(&out, "blockquote")[0].contains("border-left:"));
    }

    #[test]
    fn tables_keep_alignment_and_get_borders_outlook_shows() {
        // GFM spec examples 198 and 199.
        let out = html(
            "| foo | bar |\n| --- | --- |\n| baz | bim |\n\n| abc | defghi |\n:-: | -----------:\nbar | baz\n",
        );
        let tables = open_tags(&out, "table");
        let cells: Vec<&str> = [open_tags(&out, "th"), open_tags(&out, "td")].concat();
        assert_eq!(tables.len(), 2, "{out}");
        assert_eq!(cells.len(), 8, "{out}");
        for tag in tables.iter().chain(&cells) {
            assert!(tag.contains("border:1px solid"), "no border on {tag}");
        }
        assert!(tables.iter().all(|t| t.contains(r#"border="1""#)));
        assert!(out.contains(r#"align="center""#) && out.contains(r#"align="right""#));
        for text in [">foo</th>", ">baz</td>", ">abc</th>", ">defghi</th>"] {
            assert!(out.contains(text), "missing {text:?} in:\n{out}");
        }
    }

    #[test]
    fn code_is_monospace_inline_and_in_blocks() {
        let out = html("Run `cargo test` now.\n\n```rust\nfn main() { let a = 1 < 2; }\n```\n");
        let pre = open_tags(&out, "pre");
        let code = open_tags(&out, "code");
        assert_eq!((pre.len(), code.len()), (1, 2), "{out}");
        assert!(pre[0].contains("font-family:Consolas"));
        assert!(
            code[0].contains("font-family:Consolas"),
            "inline code: {}",
            code[0]
        );
        assert_eq!(code[1], "<code>", "the block's own <pre> carries the style");
        assert!(!out.contains("class="), "{out}");
        assert!(out.contains(">cargo test</code>"));
        assert!(out.contains("fn main() { let a = 1 &lt; 2; }\n</code></pre>"));
    }

    #[test]
    fn task_list_state_survives_without_form_controls() {
        // GFM spec example 279.
        let out = html("- [ ] foo\n- [x] bar\n");
        assert!(!out.contains("<input"), "{out}");
        assert!(out.contains("<li>☐ foo</li>"), "{out}");
        assert!(out.contains("<li>☑ bar</li>"), "{out}");
    }

    #[test]
    fn local_images_become_their_alt_text_and_web_images_stay() {
        let out = html(
            "![Architecture diagram](./img/arch.png)\n\n![](images/flow.png)\n\n\
             ![Logo](https://example.com/logo.png)\n\n![Inline](data:image/png;base64,iVBORw0KGgo=)\n\n\
             <img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"Raw > html\">\n\n<img src='C:\\shots\\a.png'>\n",
        );
        assert!(
            out.contains("<em>[Image: Architecture diagram]</em>"),
            "{out}"
        );
        assert!(out.contains("<em>[Image: flow.png]</em>"), "{out}");
        assert!(out.contains("<em>[Image: Inline]</em>"), "{out}");
        assert!(out.contains("<em>[Image: Raw &gt; html]</em>"), "{out}");
        assert!(out.contains("<em>[Image: a.png]</em>"), "{out}");
        assert!(!out.contains("data:"), "{out}");
        assert_eq!(
            open_tags(&out, "img"),
            [r#"<img src="https://example.com/logo.png" alt="Logo" />"#],
            "{out}"
        );
    }

    #[test]
    fn raw_html_passes_through_except_what_gfm_disallows() {
        // GFM spec example 652 (the tag filter) plus HTML the rendered view shows.
        let out = html(
            "<strong> <title> <style> <em>\n\nline one<br>line two\n\n<!-- private note -->\n\n<script>alert(1)</script>\n",
        );
        assert!(out.contains("<strong> &lt;title> &lt;style> <em>"), "{out}");
        assert!(out.contains("line one<br />line two"), "{out}");
        assert!(out.contains("&lt;script>"), "{out}");
        assert!(!out.contains("private note"), "{out}");
    }

    #[test]
    fn a_lone_angle_bracket_does_not_hide_the_tag_after_it() {
        let out = html(
            "<div>\nx < y <img src=\"local.png\" alt=\"pic\">\n</div>\n\n`<table>` and 1 < 2\n",
        );
        assert!(out.contains("x &lt; y <em>[Image: pic]</em>"), "{out}");
        // Markup written as text stays text.
        assert!(out.contains("&lt;table&gt;</code> and 1 &lt; 2"), "{out}");
        assert!(!out.contains("<table"), "{out}");
    }

    #[test]
    fn raw_html_keeps_no_event_handlers_styles_or_unknown_attributes() {
        // The `<img/…>` form isn't a CommonMark tag, so it rides in a `<div>`
        // block, which passes its lines through raw.
        let out = html(
            "<p onclick=\"steal()\" ONMOUSEOVER=steal() class=\"x\" style=\"color:red\">para</p>\n\n\
             <img src=\"https://example.com/x.png\" onerror=\"alert(1)\" alt=\"pic\">\n\n\
             <div>\n<img/src=\"https://example.com/y.png\"/onload=alert(1)>\n</div>\n\n\
             <div style=\"background:url(javascript:alert(1))\" data-x=\"1\">block</div>\n",
        );
        let lower = out.to_ascii_lowercase();
        for gone in [
            "onclick",
            "onmouseover",
            "onerror",
            "onload",
            "steal",
            "alert",
            "color:red",
            "data-x",
            "class=",
        ] {
            assert!(!lower.contains(gone), "{gone} survived in:\n{out}");
        }
        assert!(out.contains("<p>para</p>"), "{out}");
        assert!(
            out.contains(r#"<img src="https://example.com/x.png" alt="pic" />"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<img src="https://example.com/y.png" />"#),
            "{out}"
        );
        assert!(out.contains("<div>block</div>"), "{out}");
    }

    #[test]
    fn script_urls_lose_their_link_however_they_are_spelled() {
        let hrefs = [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            " javascript:alert(1)",
            "java&#x09;script:alert(1)",
            "java&#10;script:alert(1)",
            "&#106;avascript:alert(1)",
            "&#x6A;avascript:alert(1)",
            "&#106avascript:alert(1)",
            "&#0000106avascript:alert(1)",
            "javascript&colon;alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
        ];
        let mut links: Vec<String> = hrefs
            .iter()
            .flat_map(|href| {
                [
                    format!("<a href=\"{href}\">x</a>"),
                    format!("<a href='{href}'>x</a>"),
                ]
            })
            .collect();
        links.push("<a href=javascript:alert(1)>x</a>".into());
        links.push("[x](javascript:alert(1))".into());
        for link in &links {
            let out = html(&format!("Click {link} now.\n"));
            assert!(out.contains("<p>Click x now.</p>"), "{link} gave:\n{out}");
        }
        let out = html("<img src=\"javascript:alert(1)\" alt=\"x\">\n");
        assert!(
            out.contains("<em>[Image: x]</em>") && !out.contains("<img"),
            "{out}"
        );
    }

    #[test]
    fn link_and_image_attributes_keep_their_quotes_and_ampersands() {
        let out = html(
            "[docs](https://example.com/a?b=1&c=2 \"The \\\"best\\\" docs\"), \
             ![A \"quoted\" alt](https://example.com/x.png 'Single \\'quoted\\' title'), \
             <a href=\"https://example.com/?q=a&amp;b\" title='Say \"hi\"'>raw</a> \
             and <someone@example.com>.\n",
        );
        for expected in [
            r#"<a href="https://example.com/a?b=1&amp;c=2" title="The &quot;best&quot; docs">docs</a>"#,
            r#"<img src="https://example.com/x.png" alt="A &quot;quoted&quot; alt" title="Single 'quoted' title" />"#,
            r#"<a href="https://example.com/?q=a&amp;b" title="Say &quot;hi&quot;">raw</a>"#,
            r#"<a href="mailto:someone@example.com">someone@example.com</a>"#,
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
    }

    #[test]
    fn links_that_only_work_beside_the_file_become_their_text() {
        let out = html(
            "See [the spec](./spec.md), [notes](notes/today.md#plan), [root](/docs/a.md), \
             [up](../README.md) and <a href=\"file:///C:/notes/x.md\">a file</a>.\n\n\
             [Usage](#usage) and a note[^1] stay links.\n\n[^1]: The note.\n",
        );
        assert!(
            out.contains("See the spec, notes, root, up and a file."),
            "{out}"
        );
        for kept in [
            r##"<a href="#usage">Usage</a>"##,
            r##"<a href="#user-content-fn-1" id="user-content-fnref-1">1</a>"##,
            r##"<li id="user-content-fn-1">"##,
            r##"<a href="#user-content-fnref-1">↩</a>"##,
        ] {
            assert!(out.contains(kept), "missing {kept:?} in:\n{out}");
        }
    }

    #[test]
    fn tags_mail_does_not_know_keep_their_text() {
        // As in the rendered view: unknown inline tags go and their text stays;
        // block containers keep their own lines.
        let out = html(
            "Press <kbd>Ctrl</kbd>, <span class=\"x\">see</span> <object data=\"x.swf\">this</object>.\n\n\
             <details><summary>More</summary>Hidden part</details>\n",
        );
        assert!(out.contains("<p>Press Ctrl, see this.</p>"), "{out}");
        assert!(
            out.contains("<div><div>More</div>Hidden part</div>"),
            "{out}"
        );
    }

    #[test]
    fn unfinished_tags_take_linear_time() {
        // Raw HTML whose tags never close: GFM keeps the whole run as one HTML
        // block, and every `<` starts a tag with no `>` before the next `<`.
        let markdown = format!("<div>\n{}", "<a ".repeat(100_000));
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || done.send(markdown_to_html(&markdown, "Image")));
        // Linear work takes milliseconds, so 10 s leaves room for a loaded
        // machine. Rescanning the rest of the document at each `<` is about
        // 1.5e10 byte steps, which ran past the bound.
        let out = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("300 KB of unfinished tags took over 10 s");
        assert_eq!(out.matches("&lt;a ").count(), 100_000);
    }
}
