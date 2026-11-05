//! "Copy as rich text" for a rendered Markdown pane: the whole document as HTML
//! that survives a paste into a mail client (Outlook's Word engine, Gmail).
//!
//! Parsing matches the rendered view: GFM (tables, task lists, strikethrough,
//! autolinks, footnotes), with raw HTML passed through the way the view renders
//! it, minus the tags GFM disallows (`<script>`, `<style>`, …). The compiled
//! HTML is then adjusted for mail. Mail clients drop stylesheets, classes and
//! form controls, and a relative image path points at nothing once pasted, so
//! presentation goes inline and the rest gets a text stand-in.

use markdown::{CompileOptions, Options, ParseOptions};
use std::borrow::Cow;

/// One font stack for code, inline and in blocks.
const MONO: &str = "font-family:Consolas,Menlo,'Courier New',monospace;font-size:90%";
/// Outlook's Word engine honours the presentational `border`/`cellpadding`
/// attributes more reliably than CSS, so tables carry both.
const TABLE: &str = r#"<table border="1" cellspacing="0" cellpadding="6" style="border-collapse:collapse;border:1px solid #d0d7de">"#;
const CELL: &str = "border:1px solid #d0d7de;padding:4px 8px";
const HEADER_CELL_SHADE: &str = ";background-color:#f6f8fa";
const BLOCKQUOTE: &str = r#"<blockquote style="margin:0 0 0 0.8ex;border-left:3px solid #d0d7de;padding-left:1ex;color:#57606a">"#;
/// markdown-rs's exact task-list checkboxes (it never emits other `<input>`s).
const UNCHECKED_BOX: &str = r#"<input type="checkbox" disabled="" />"#;
const CHECKED_BOX: &str = r#"<input type="checkbox" disabled="" checked="" />"#;

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

/// Rewrite the tags mail clients mishandle; pass everything else through.
///
/// markdown-rs encodes `&`, `"`, `<` and `>` in text and attribute values, so
/// every `<` that starts a tag is markup it generated or raw HTML the author
/// wrote, never document text.
fn for_mail(html: &str, image_label: &str) -> String {
    let mut out = String::with_capacity(html.len() + html.len() / 4);
    let mut in_pre = false;
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if let Some(body) = rest.strip_prefix("<!--") {
            // Comments never show in the rendered view. Dropping them also keeps
            // an author's comment from posing as a CF_HTML fragment marker.
            rest = body.find("-->").map_or("", |end| &body[end + 3..]);
            continue;
        }
        let Some(len) = tag_len(rest) else {
            out.push('<');
            rest = &rest[1..];
            continue;
        };
        let (tag, tail) = rest.split_at(len);
        rest = tail;
        out.push_str(&rewrite_tag(tag, &mut in_pre, image_label));
    }
    out.push_str(rest);
    out
}

/// Length of the tag at the start of `s` (which starts with `<`), honouring
/// quoted attribute values that contain `>`. `None` when the `<` is text.
fn tag_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let starts_tag = bytes
        .get(1)
        .is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'/' | b'!' | b'?'));
    if !starts_tag {
        return None;
    }
    let mut quote = None;
    let mut after_eq = false;
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'>' => return Some(i + 1),
            b'=' => after_eq = true,
            b'"' | b'\'' if after_eq => {
                quote = Some(b);
                after_eq = false;
            }
            b if b.is_ascii_whitespace() => {}
            _ => after_eq = false,
        }
    }
    None
}

fn rewrite_tag<'a>(tag: &'a str, in_pre: &mut bool, image_label: &str) -> Cow<'a, str> {
    let closing = tag.starts_with("</");
    let name = tag
        .trim_start_matches(['<', '/'])
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match (closing, name.as_str()) {
        (false, "table") if tag == "<table>" => TABLE.into(),
        (false, "th" | "td") => cell(tag, &name).map_or(tag.into(), Cow::Owned),
        (false, "pre") if tag == "<pre>" => {
            *in_pre = true;
            format!(r#"<pre style="{MONO};background-color:#f6f8fa;padding:8px 12px;white-space:pre-wrap">"#).into()
        }
        (true, "pre") => {
            *in_pre = false;
            tag.into()
        }
        // A fenced block's language class means nothing without a stylesheet;
        // its `<pre>` carries the styling.
        (false, "code") if tag == "<code>" || tag.starts_with(r#"<code class="language-"#) => {
            if *in_pre {
                "<code>".into()
            } else {
                format!(r#"<code style="{MONO};background-color:#f6f8fa;padding:1px 4px">"#).into()
            }
        }
        (false, "blockquote") if tag == "<blockquote>" => BLOCKQUOTE.into(),
        // `<s>` is plain strikethrough; `<del>` marks an edit, which a word
        // processor's HTML import may take as a tracked change instead.
        (false, "del") if tag == "<del>" => "<s>".into(),
        (true, "del") if tag == "</del>" => "</s>".into(),
        // Gmail strips form controls, which would lose the task's state.
        (false, "input") if tag == UNCHECKED_BOX => "☐".into(),
        (false, "input") if tag == CHECKED_BOX => "☑".into(),
        (false, "img") => image(tag, image_label),
        _ => tag.into(),
    }
}

/// A bare or aligned table cell with borders; `None` leaves a cell the author
/// wrote with other attributes alone.
fn cell(tag: &str, name: &str) -> Option<String> {
    let attrs = tag
        .strip_prefix('<')?
        .strip_prefix(name)?
        .strip_suffix('>')?;
    if !matches!(
        attrs,
        "" | r#" align="left""# | r#" align="center""# | r#" align="right""#
    ) {
        return None;
    }
    let shade = if name == "th" { HEADER_CELL_SHADE } else { "" };
    Some(format!(r#"<{name}{attrs} style="{CELL}{shade}">"#))
}

/// A web image stays: Outlook embeds it on paste and Gmail proxies it. Anything
/// else (a relative or absolute local path, a `data:` URI) becomes its alt text,
/// since neither client can show it and classic Outlook drops `data:` images.
fn image<'a>(tag: &'a str, label: &str) -> Cow<'a, str> {
    let src = attr(tag, "src").unwrap_or("");
    let lower = src.trim_start().to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return tag.into();
    }
    let name = attr(tag, "alt")
        .filter(|alt| !alt.trim().is_empty())
        .or_else(|| {
            src.rsplit(['/', '\\'])
                .next()
                .filter(|file| !file.is_empty() && !lower.starts_with("data:"))
        });
    let label = escape_text(label);
    match name {
        // Already attribute-encoded; only a raw-HTML value can still hold `<`/`>`.
        Some(name) => {
            let name = name.replace('<', "&lt;").replace('>', "&gt;");
            format!("<em>[{label}: {name}]</em>").into()
        }
        None => format!("<em>[{label}]</em>").into(),
    }
}

/// The value of attribute `want` in `tag` (quoted or not), as written.
fn attr<'a>(tag: &'a str, want: &str) -> Option<&'a str> {
    let inner = tag
        .strip_prefix('<')?
        .trim_end_matches('>')
        .trim_end_matches('/');
    let mut rest = inner.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if rest.is_empty() {
            return None;
        }
        let name_end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        rest = rest[name_end..].trim_start();
        let mut value = "";
        if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let body = &after[1..];
                    let end = body.find(q).unwrap_or(body.len());
                    value = &body[..end];
                    rest = body.get(end + 1..).unwrap_or("");
                }
                _ => {
                    let end = after
                        .find(|c: char| c.is_ascii_whitespace())
                        .unwrap_or(after.len());
                    value = &after[..end];
                    rest = &after[end..];
                }
            }
        }
        if name.eq_ignore_ascii_case(want) {
            return Some(value);
        }
    }
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::markdown_to_html;

    fn html(markdown: &str) -> String {
        markdown_to_html(markdown, "Image")
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
        assert!(out.contains("line one<br>line two"), "{out}");
        assert!(out.contains("&lt;script>"), "{out}");
        assert!(!out.contains("private note"), "{out}");
    }

    #[test]
    fn a_lone_angle_bracket_does_not_hide_the_tag_after_it() {
        let out = html(
            "<div>\nx < y <img src=\"local.png\" alt=\"pic\">\n</div>\n\n`<table>` and 1 < 2\n",
        );
        assert!(out.contains("x < y <em>[Image: pic]</em>"), "{out}");
        // Markup written as text stays text.
        assert!(out.contains("&lt;table&gt;</code> and 1 &lt; 2"), "{out}");
        assert!(!out.contains("<table"), "{out}");
    }
}
