//! "Copy as rich text" for a rendered Markdown pane: the whole document as HTML
//! that survives a paste into a mail client (Outlook's Word engine, Gmail).
//!
//! Parsing matches the rendered view: GFM (tables, task lists, strikethrough,
//! autolinks, footnotes) with raw HTML. ammonia then parses the compiled HTML
//! as a browser would (html5ever) and writes back only the listed elements and
//! attributes, URLs on the web or to a mail address, and links to a fragment of
//! the document; any other element leaves its text. Mail clients drop
//! stylesheets, classes and form controls, and a relative path points at
//! nothing once pasted, so presentation goes inline and the rest gets a text
//! stand-in.
//!
//! The rendered view parses each piece of raw HTML on its own, but this parses
//! the document whole, so a tag that makes what follows it text (`<title>`,
//! `<script>`, `<select>`, …) would take the rest of the document with it.
//! Those tags are written as text, as GFM's tag filter does on GitHub.

use ammonia::{Builder, UrlRelative};
use markdown::{CompileOptions, Options, ParseOptions};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Inline styles mail keeps where it drops a stylesheet. Code in a block takes
/// its size from the `<code>` inside the `<pre>`.
const PRE_STYLE: &str = "font-family:Consolas,Menlo,'Courier New',monospace;\
                         background-color:#f6f8fa;padding:8px 12px;white-space:pre-wrap";
const CODE_STYLE: &str =
    "font-family:Consolas,Menlo,'Courier New',monospace;font-size:90%;background-color:#f6f8fa";
const TABLE_STYLE: &str = "border-collapse:collapse;border:1px solid #d0d7de";
const TD_STYLE: &str = "border:1px solid #d0d7de;padding:4px 8px";
const TH_STYLE: &str = "border:1px solid #d0d7de;padding:4px 8px;background-color:#f6f8fa";
const BLOCKQUOTE_STYLE: &str =
    "margin:0 0 0 0.8ex;border-left:3px solid #d0d7de;padding-left:1ex;color:#57606a";
const MARK_STYLE: &str = "background-color:#fff8c5";

/// Elements written back as themselves (`del` and the containers below are
/// renamed on the way out, and `<span>` is written only to carry an `id`).
const ELEMENTS: &[&str] = &[
    "a",
    "b",
    "blockquote",
    "br",
    "caption",
    "code",
    "dd",
    "del",
    "div",
    "dl",
    "dt",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "s",
    "span",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];
/// HTML's other text-level elements that hold text (all but ruby's parts), and
/// `<ins>`. Mail may not know them, so each leaves its text, inside a `<span>`
/// when it carries an `id`.
const AS_SPAN: &[&str] = &[
    "abbr", "bdi", "bdo", "cite", "data", "dfn", "ins", "kbd", "q", "samp", "small", "time", "var",
];
/// Block containers a mail client may not know, written as `<div>` so their
/// content still starts on its own line and an `id` still marks a target.
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
/// Tags that change how the rest of the document parses and that GFM's tag
/// filter leaves alone: `<noscript>` content is text when scripting is on,
/// `<template>` content is never shown, and `<select>` ignores the markup
/// inside it.
const AS_TEXT: &[&str] = &["noscript", "select", "template"];
/// What markdown-rs writes for a task's checkbox. Gmail strips form controls,
/// which would lose the task's state.
const UNCHECKED: &str = r#"<input type="checkbox" disabled="" />"#;
const CHECKED: &str = r#"<input type="checkbox" disabled="" checked="" />"#;

static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut attributes: HashMap<&str, HashSet<&str>> = HashMap::from([
        ("a", HashSet::from(["href", "name", "title"])),
        (
            "img",
            HashSet::from(["src", "alt", "title", "width", "height"]),
        ),
        ("td", HashSet::from(["align", "colspan", "rowspan"])),
        ("th", HashSet::from(["align", "colspan", "rowspan"])),
        ("ol", HashSet::from(["start"])),
    ]);
    for aligned in ["div", "h1", "h2", "h3", "h4", "h5", "h6", "p"] {
        attributes.insert(aligned, HashSet::from(["align"]));
    }
    let mut builder = Builder::empty();
    builder
        .tags(
            ELEMENTS
                .iter()
                .chain(AS_SPAN)
                .chain(AS_DIV)
                .copied()
                .collect(),
        )
        // An `id` is a fragment link's target, `<a name>` the older spelling.
        .generic_attributes(HashSet::from(["id"]))
        .tag_attributes(attributes)
        .url_schemes(HashSet::from(["http", "https", "mailto"]))
        .url_relative(UrlRelative::Custom(Box::new(fragment_only)))
        .attribute_filter(checked_value)
        .link_rel(None)
        .strip_comments(true)
        // Outlook's Word engine honours the presentational `border` and
        // `cellpadding` attributes more reliably than CSS, so tables carry both.
        .set_tag_attribute_value("table", "border", "1")
        .set_tag_attribute_value("table", "cellspacing", "0")
        .set_tag_attribute_value("table", "cellpadding", "6")
        .set_tag_attribute_value("table", "style", TABLE_STYLE)
        .set_tag_attribute_value("td", "style", TD_STYLE)
        .set_tag_attribute_value("th", "style", TH_STYLE)
        .set_tag_attribute_value("pre", "style", PRE_STYLE)
        .set_tag_attribute_value("code", "style", CODE_STYLE)
        .set_tag_attribute_value("blockquote", "style", BLOCKQUOTE_STYLE)
        .set_tag_attribute_value("mark", "style", MARK_STYLE);
    builder
});

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
    let html = as_text(&html).replace(CHECKED, "☑").replace(UNCHECKED, "☐");
    for_mail(&SANITIZER.clean(&html).to_string(), image_label)
}

/// `html` with the `<` of each [`AS_TEXT`] start or end tag written as `&lt;`,
/// the way GFM's tag filter treats its own list. markdown-rs escapes `<` in
/// document text, so these come only from raw HTML.
fn as_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt + 1..];
        let name = rest.strip_prefix('/').unwrap_or(rest);
        let filtered = AS_TEXT.iter().any(|tag| {
            name.get(..tag.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(tag))
                && name[tag.len()..]
                    .starts_with(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
        });
        out.push_str(if filtered { "&lt;" } else { "<" });
    }
    out.push_str(rest);
    out
}

/// A relative link is kept only as a fragment of this document. Any other
/// path resolves only beside the Markdown file, so the link leaves its text.
fn fragment_only(url: &str) -> Option<Cow<'_, str>> {
    url.starts_with('#').then_some(Cow::Borrowed(url))
}

/// The attribute values worth keeping. URLs arrive decoded and with an allowed
/// scheme, or relative; an image keeps only a web URL, since neither Outlook
/// nor Gmail can show a local file or a `data:` URI. Sizes and alignment keep
/// only the values HTML gave them.
fn checked_value<'u>(element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    let keep = match attribute {
        "href" | "src" => return url(element, value),
        "align" => {
            return ["left", "center", "right", "justify"]
                .into_iter()
                .find(|align| align.eq_ignore_ascii_case(value))
                .map(Cow::Borrowed);
        }
        "start" => is_number(value.strip_prefix('-').unwrap_or(value), 9),
        "colspan" | "rowspan" => is_number(value, 4),
        "width" | "height" => is_number(value.strip_suffix('%').unwrap_or(value), 5),
        _ => true,
    };
    keep.then_some(Cow::Borrowed(value))
}

/// A URL as a browser reads it: no surrounding spaces or controls, and no tab
/// or line break inside.
fn url<'u>(element: &str, value: &'u str) -> Option<Cow<'u, str>> {
    let trimmed = value.trim_matches(|c: char| c <= ' ');
    let url = if trimmed.contains(['\t', '\n', '\r']) {
        Cow::Owned(trimmed.replace(['\t', '\n', '\r'], ""))
    } else {
        Cow::Borrowed(trimmed)
    };
    let web = ["http:", "https:"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    });
    (element != "img" || web).then_some(url)
}

/// The changes ammonia doesn't make: a local image becomes its alt text,
/// `<del>` and the unfamiliar elements get names mail knows, and a link with
/// no URL or target, or a span with no target, leaves only its text. A target
/// (`id`) stays wherever the text does.
///
/// The input is html5ever's serialization of the cleaned tree. Text escapes
/// `<`, and every attribute value is double-quoted with `"` escaped, so each
/// `<` opens a tag that ends at the first `>` outside quotes. Elements nest
/// properly, so an end tag closes the latest element still open.
fn for_mail(html: &str, image_label: &str) -> String {
    let mut out = String::with_capacity(html.len());
    // What each open `<a>`, `<span>` or AS_SPAN element was written as, if
    // anything, so its end tag matches.
    let mut inline: Vec<Option<&str>> = Vec::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        let Some(len) = tag_len(rest) else {
            out.push_str(&escape_text(rest));
            return out;
        };
        let tag = &rest[..len];
        rest = &rest[len..];
        let end = tag.starts_with("</");
        let body = &tag[if end { 2 } else { 1 }..len - 1];
        let name = body.split(' ').next().unwrap_or(body);
        let attrs = &body[name.len()..];
        let inline_name = name == "a" || name == "span" || AS_SPAN.contains(&name);
        match name {
            _ if end && inline_name => {
                if let Some(written) = inline.pop().flatten() {
                    rename(&mut out, true, written, "");
                }
            }
            "a" => {
                let keep = ["href", "id", "name"]
                    .iter()
                    .any(|attr| attr_value(body, attr).is_some());
                inline.push(keep.then_some("a"));
                if keep {
                    out.push_str(tag);
                }
            }
            // ammonia leaves these no attribute but `id`.
            _ if inline_name => {
                let keep = attr_value(body, "id").is_some();
                inline.push(keep.then_some("span"));
                if keep {
                    rename(&mut out, false, "span", attrs);
                }
            }
            "img" if attr_value(body, "src").is_none() => {
                let label = escape_text(image_label);
                // Already escaped as an attribute value, as is the alt text,
                // which leaves `<` and `>` alone.
                let id = attr_value(body, "id")
                    .map(|id| format!(r#" id="{id}""#))
                    .unwrap_or_default();
                match attr_value(body, "alt")
                    .map(str::trim)
                    .filter(|alt| !alt.is_empty())
                {
                    Some(alt) => {
                        let alt = alt.replace('<', "&lt;").replace('>', "&gt;");
                        out.push_str(&format!("<em{id}>[{label}: {alt}]</em>"));
                    }
                    None => out.push_str(&format!("<em{id}>[{label}]</em>")),
                }
            }
            // `<s>` is plain strikethrough; `<del>` marks an edit, which a
            // word processor's HTML import may take as a tracked change.
            "del" => rename(&mut out, end, "s", attrs),
            _ if AS_DIV.contains(&name) => rename(&mut out, end, "div", attrs),
            _ => out.push_str(tag),
        }
    }
    out.push_str(rest);
    out
}

/// Length of the tag at the start of `s`, up to the first `>` outside double
/// quotes.
fn tag_len(s: &str) -> Option<usize> {
    let mut quoted = false;
    s.bytes()
        .position(|b| {
            if b == b'"' {
                quoted = !quoted;
            }
            b == b'>' && !quoted
        })
        .map(|end| end + 1)
}

/// The value of `attr` in a serialized tag's `name attr="value" …`, still
/// escaped.
fn attr_value<'a>(body: &'a str, attr: &str) -> Option<&'a str> {
    let mut rest = body.split_once(' ')?.1;
    while let Some((name, after)) = rest.split_once("=\"") {
        let (value, after) = after.split_once('"')?;
        if name == attr {
            return Some(value);
        }
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    None
}

fn rename(out: &mut String, end: bool, name: &str, attrs: &str) {
    out.push_str(if end { "</" } else { "<" });
    out.push_str(name);
    out.push_str(attrs);
    out.push('>');
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn is_number(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::markdown_to_html;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The elements the export may write.
    const WRITTEN: &[&str] = &[
        "a",
        "b",
        "blockquote",
        "br",
        "caption",
        "code",
        "dd",
        "div",
        "dl",
        "dt",
        "em",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "i",
        "img",
        "li",
        "mark",
        "ol",
        "p",
        "pre",
        "s",
        "span",
        "strong",
        "sub",
        "sup",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "tr",
        "u",
        "ul",
    ];

    /// The document as mail HTML, held to the clipboard's safety rule.
    fn html(markdown: &str) -> String {
        let out = markdown_to_html(markdown, "Image");
        assert_safe(&out);
        out
    }

    /// Length of the tag at the start of `s`: up to the first `>` outside
    /// double quotes.
    fn tag_len(s: &str) -> Option<usize> {
        let mut quoted = false;
        s.bytes()
            .position(|b| {
                if b == b'"' {
                    quoted = !quoted;
                }
                b == b'>' && !quoted
            })
            .map(|end| end + 1)
    }

    /// Every tag in `html`, in order: each `<` outside a tag opens one.
    fn tags(html: &str) -> Vec<&str> {
        let mut tags = Vec::new();
        let mut rest = html;
        while let Some(lt) = rest.find('<') {
            rest = &rest[lt..];
            let len = tag_len(rest).unwrap_or_else(|| panic!("unclosed `<` in:\n{html}"));
            tags.push(&rest[..len]);
            rest = &rest[len..];
        }
        tags
    }

    /// The safety rule, read off the output alone: every `<` outside a tag
    /// opens one written `</name>` or `<name( attr="value")*>` for an element
    /// the export may write, with no event handler, every link on the web, to
    /// a mail address or to a fragment of the document, and every image on the
    /// web. A browser parses markup of that shape one way only.
    fn assert_safe(html: &str) {
        for tag in tags(html) {
            let len = tag.len();
            let inner = &tag[1..len - 1];
            let inner = inner.strip_prefix('/').unwrap_or(inner);
            let inner = inner.strip_suffix(" /").unwrap_or(inner);
            let (name, mut attrs) = inner.split_once(' ').unwrap_or((inner, ""));
            assert!(
                WRITTEN.contains(&name),
                "element {name:?} the export doesn't write: {tag} in:\n{html}"
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
                let schemes: &[&str] = match attr {
                    "href" => &["http://", "https://", "mailto:", "#"],
                    "src" => &["http://", "https://"],
                    _ => &[],
                };
                if !schemes.is_empty() {
                    let url = value
                        .replace("&quot;", "\"")
                        .replace("&amp;", "&")
                        .to_ascii_lowercase();
                    assert!(
                        schemes.iter().any(|scheme| url.starts_with(scheme)),
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
        tags(html)
            .into_iter()
            .filter(|t| {
                t[1..].starts_with(name) && t[1 + name.len()..].starts_with([' ', '>', '/'])
            })
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
            r#"<a href="https://auto.example.org">https://auto.example.org</a>"#,
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        assert_eq!(open_tags(&out, "hr").len(), 1, "{out}");
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
            code.iter().all(|tag| tag.contains("font-family:Consolas")),
            "{out}"
        );
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
        for expected in [
            "<em>[Image: Architecture diagram]</em>",
            "<em>[Image: Inline]</em>",
            "<em>[Image: Raw &gt; html]</em>",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        // No alt text: the stand-in says only that an image was there.
        assert_eq!(out.matches("<em>[Image]</em>").count(), 2, "{out}");
        assert!(!out.contains("data:"), "{out}");
        let images = open_tags(&out, "img");
        assert_eq!(images.len(), 1, "{out}");
        assert!(
            images[0].contains(r#"src="https://example.com/logo.png""#),
            "{out}"
        );
        assert!(images[0].contains(r#"alt="Logo""#), "{out}");
    }

    #[test]
    fn raw_html_the_view_renders_survives_and_comments_go() {
        let out = html(
            "<strong>bold</strong>, <u>under</u> and H<sub>2</sub>O\n\nline one<br>line two\n\n\
             <!-- private note -->\n",
        );
        for expected in [
            "<strong>bold</strong>",
            "<u>under</u>",
            "H<sub>2</sub>O",
            "line one<br",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        assert!(!out.contains("private note"), "{out}");
    }

    #[test]
    fn scripts_styles_and_frames_never_become_elements() {
        // Their tags show as text, as on GitHub (see the module doc).
        let out = html(
            "Before.\n\n<script>alert(1)</script>\n\n<style>p { color: red }</style>\n\n\
             <iframe src=\"https://evil.example/\">frame</iframe>\n\n\
             Inline <SCRIPT >alert(2)</script> and <svg><style>x</style></svg>.\n\nAfter.\n",
        );
        for name in ["script", "SCRIPT", "style", "iframe", "svg"] {
            assert!(open_tags(&out, name).is_empty(), "<{name}> in:\n{out}");
        }
        assert!(
            out.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{out}"
        );
        assert!(
            out.contains("<p>Before.</p>") && out.contains("<p>After.</p>"),
            "{out}"
        );
    }

    #[test]
    fn a_tag_whose_content_is_text_takes_only_itself() {
        // In the rendered view each piece of raw HTML parses alone, so one of
        // these in prose can't hide the rest of the document.
        let out = html(
            "Set the <title> and <textarea> tags.\n\n## Next\n\n\
             Pick with <select>, <noscript> or <template>.\n\n## End\n",
        );
        for kept in ["<h2>Next</h2>", "<h2>End</h2>"] {
            assert!(out.contains(kept), "missing {kept:?} in:\n{out}");
        }
        for tag in ["title", "textarea", "select", "noscript", "template"] {
            assert!(
                out.contains(&format!("&lt;{tag}&gt;")),
                "<{tag}> isn't text in:\n{out}"
            );
        }
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
    fn a_quoted_angle_bracket_stays_inside_its_attribute() {
        // HTML allows `<` in a quoted attribute value, single or double quoted.
        let out = html(
            "<a href=\"https://example.com/a\" title=\"1 < 2\">Docs</a> and \
             <a href='https://example.com/b' title='a < b'>More</a>.\n\n\
             <img src=\"https://example.com/x.png\" alt=\"x < y\">\n",
        );
        assert!(!out.contains("&lt;a") && !out.contains("&lt;img"), "{out}");
        let links = open_tags(&out, "a");
        assert!(
            links
                .iter()
                .any(|t| t.contains(r#"href="https://example.com/a""#) && t.contains("1 < 2")),
            "{out}"
        );
        assert!(
            links
                .iter()
                .any(|t| t.contains(r#"href="https://example.com/b""#) && t.contains("a < b")),
            "{out}"
        );
        assert!(
            out.contains(">Docs</a>") && out.contains(">More</a>"),
            "{out}"
        );
        assert!(
            open_tags(&out, "img").iter().any(|t| t.contains("x < y")),
            "{out}"
        );
    }

    #[test]
    fn fragment_targets_keep_their_ids() {
        // Elements the export restyles or renames, a legacy named anchor, and
        // elements that would otherwise leave only their text: spans, unfamiliar
        // inline tags and local images.
        let out = html(
            "<div><a href=\"#sample\">Sample</a><pre id=\"sample\">x</pre></div>\n\n\
             <table id=\"t\"><tr><td>c</td></tr></table>\n\n\
             <blockquote id=\"q\">quoted</blockquote>\n\n\
             <section id=\"s\">part</section>\n\n\
             <a name=\"legacy\"></a>Legacy target, <a href=\"#legacy\">back</a>.\n\n\
             <a href=\"#here\">Here</a> is <span id=\"here\" class=\"x\">there</span>\
             <span id=\"anchor\"></span>, <span class=\"outer\"><span id=\"inner\">in</span> out</span>, \
             <kbd id=\"key\">Ctrl</kbd> and <a href=\"#plot\">Plot</a>\
             <img id=\"plot\" src=\"./plot.png\" alt=\"Plot\">\n",
        );
        for (name, target) in [
            ("pre", r#"id="sample""#),
            ("table", r#"id="t""#),
            ("blockquote", r#"id="q""#),
            ("div", r#"id="s""#),
            ("a", r#"name="legacy""#),
        ] {
            assert!(
                open_tags(&out, name).iter().any(|tag| tag.contains(target)),
                "no <{name} {target}> in:\n{out}"
            );
        }
        for expected in [
            r##"<a href="#sample">Sample</a>"##,
            r##"<a href="#here">Here</a> is <span id="here">there</span><span id="anchor"></span>, "##,
            r#"<span id="inner">in</span> out, <span id="key">Ctrl</span> and "#,
            r##"<a href="#plot">Plot</a><em id="plot">[Image: Plot]</em>"##,
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
    }

    #[test]
    fn urls_decode_every_named_reference() {
        // `&sol;` and `&colon;` are HTML character references like `&amp;`.
        let out = html(
            "<img src=\"https://example.com/a&sol;b.png\" alt=\"pic\">\n\n\
             <a href=\"https&colon;//example.com/x\">colon</a> and \
             <a href=\"https://example.com/?a=1&AMP;b=2\">amp</a>\n",
        );
        for expected in [
            r#"src="https://example.com/a/b.png""#,
            r#"href="https://example.com/x""#,
            r#"href="https://example.com/?a=1&amp;b=2""#,
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
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
        for src in ["https://example.com/x.png", "https://example.com/y.png"] {
            assert!(
                open_tags(&out, "img")
                    .iter()
                    .any(|t| t.contains(&format!(r#"src="{src}""#))),
                "no image {src} in:\n{out}"
            );
        }
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
            r#"href="https://example.com/a?b=1&amp;c=2""#,
            r#"title="The &quot;best&quot; docs""#,
            r#"alt="A &quot;quoted&quot; alt""#,
            r#"title="Single 'quoted' title""#,
            r#"href="https://example.com/?q=a&amp;b""#,
            r#"title="Say &quot;hi&quot;""#,
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
        // block, and every `<` starts a tag that never ends, bare or inside a
        // quote left open.
        let inputs = [
            format!("<div>\n{}", "<a ".repeat(100_000)),
            format!("<div>\n<a title=\"{}", "<b ".repeat(100_000)),
        ];
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            for markdown in inputs {
                let _ = done.send(markdown_to_html(&markdown, "Image"));
            }
        });
        // Linear work takes milliseconds, so 10 s leaves room for a loaded
        // machine. The first rewriter rescanned the rest of the document at
        // each `<`, about 1.5e10 byte steps here, and ran past the bound.
        for _ in 0..2 {
            let out = finished
                .recv_timeout(Duration::from_secs(10))
                .expect("300 KB of unfinished tags took over 10 s");
            assert!(!out.contains("<a") && !out.contains("<b"), "{out}");
        }
    }
}
