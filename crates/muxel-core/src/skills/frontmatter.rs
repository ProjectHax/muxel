//! A `SKILL.md`'s YAML frontmatter.
//!
//! Only the handful of keys skills use are read, and only the YAML shapes they
//! use: plain, single- and double-quoted scalars, `|`/`>` block scalars, plain
//! scalars continued on indented lines, and a one-level `metadata:` map. That is
//! all every published skill needs, so a full YAML parser isn't pulled in.

use crate::library::sanitize::sanitize;

/// The fields muxel uses from a `SKILL.md`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    /// `metadata.internal: true` — the `skills` CLI hides these by default.
    pub internal: bool,
}

/// Why a `SKILL.md` isn't a usable skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontmatterError {
    /// No `---` block at the top of the file.
    Missing,
    /// The block has no `name`.
    NoName,
    /// The block has no `description`.
    NoDescription,
}

/// Longest `name` the Agent Skills spec allows.
pub const MAX_NAME_LEN: usize = 64;
/// Longest `description` the spec allows.
pub const MAX_DESCRIPTION_LEN: usize = 1024;

/// Parse the frontmatter at the top of a `SKILL.md`. Text is sanitized (control
/// and invisible formatting characters removed) since it is shown on screen.
pub fn parse(text: &str) -> Result<Frontmatter, FrontmatterError> {
    let block = block(text).ok_or(FrontmatterError::Missing)?;
    let lines: Vec<&str> = block.lines().collect();
    let mut fm = Frontmatter::default();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') || indent(line) > 0 {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let rest = rest.trim();
        // The indented lines that belong to this key.
        let start = i;
        while i < lines.len() && (lines[i].trim().is_empty() || indent(lines[i]) > 0) {
            i += 1;
        }
        let body = &lines[start..i];
        match key {
            "name" => fm.name = one_line(&scalar(rest, body)),
            "description" => fm.description = one_line(&scalar(rest, body)),
            "license" => {
                let v = one_line(&scalar(rest, body));
                fm.license = (!v.is_empty()).then_some(v);
            }
            "metadata" => {
                fm.internal = body.iter().any(|l| {
                    l.split_once(':').is_some_and(|(k, v)| {
                        k.trim() == "internal" && unquote(v.trim()).eq_ignore_ascii_case("true")
                    })
                });
            }
            _ => {}
        }
    }
    if fm.name.is_empty() {
        return Err(FrontmatterError::NoName);
    }
    if fm.description.is_empty() {
        return Err(FrontmatterError::NoDescription);
    }
    Ok(fm)
}

/// The text between the opening `---` line and the closing one.
pub fn block(text: &str) -> Option<&str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let start = first.len();
    let mut pos = start;
    for line in lines {
        if line.trim_end() == "---" {
            return Some(&text[start..pos]);
        }
        pos += line.len();
    }
    None
}

/// Whether `name` meets the spec: lowercase letters, digits and single inner
/// hyphens, at most [`MAX_NAME_LEN`] characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// The folder name a skill installs under, from its frontmatter `name`, as the
/// `skills` CLI derives it: lowercase, each run of characters outside
/// `[a-z0-9._]` becomes `-`, leading/trailing `.` and `-` trimmed, at most 255
/// characters. `None` when nothing is left.
pub fn install_name(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let trimmed: String = out
        .trim_matches(|c| c == '.' || c == '-')
        .chars()
        .take(255)
        .collect();
    let trimmed = trimmed.trim_matches(|c| c == '.' || c == '-').to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// A key's value: the text after the colon plus the indented lines below it.
fn scalar(rest: &str, body: &[&str]) -> String {
    let header = rest.split(" #").next().unwrap_or(rest).trim();
    if header.starts_with('|') || header.starts_with('>') {
        let literal = header.starts_with('|');
        let lines = dedent(body);
        let text = if literal {
            lines.join("\n")
        } else {
            fold(&lines)
        };
        return sanitize(text.trim_end());
    }
    if rest.starts_with('"') || rest.starts_with('\'') {
        // A quoted scalar may span lines; join them and unquote.
        let mut joined = rest.to_string();
        for l in body {
            joined.push(' ');
            joined.push_str(l.trim());
        }
        return sanitize(&unquote(joined.trim()));
    }
    // A plain scalar, continued on indented lines.
    let mut parts = vec![strip_comment(rest)];
    parts.extend(body.iter().map(|l| l.trim()).filter(|l| !l.is_empty()));
    sanitize(&parts.join(" "))
}

fn strip_comment(s: &str) -> &str {
    match s.find(" #") {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

fn dedent<'a>(body: &[&'a str]) -> Vec<&'a str> {
    let min = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent(l))
        .min()
        .unwrap_or(0);
    body.iter()
        .map(|l| if l.len() >= min { &l[min..] } else { l.trim() })
        .collect()
}

/// `>` folding: lines join with spaces; blank lines become newlines.
fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    for l in lines {
        if l.trim().is_empty() {
            out.push('\n');
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(l.trim_end());
        }
    }
    out
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        return out;
    }
    s.to_string()
}

/// Folded newlines and runs of whitespace become single spaces, for one-line
/// fields shown in lists.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{FrontmatterError, block, install_name, parse, valid_name};

    #[test]
    fn reads_plain_fields() {
        let fm = parse(
            "---\nname: security-audit\ndescription: Audit code. Use when asked.\nlicense: MIT\n---\n# Body\n",
        )
        .unwrap();
        assert_eq!(fm.name, "security-audit");
        assert_eq!(fm.description, "Audit code. Use when asked.");
        assert_eq!(fm.license.as_deref(), Some("MIT"));
        assert!(!fm.internal);
    }

    #[test]
    fn reads_quoted_and_block_scalars() {
        let fm = parse(
            "---\nname: \"pdf\"\ndescription: >\n  Work with PDFs:\n  fill forms.\nlicense: 'Apache-2.0 ''x'''\n---\n",
        )
        .unwrap();
        assert_eq!(fm.name, "pdf");
        assert_eq!(fm.description, "Work with PDFs: fill forms.");
        assert_eq!(fm.license.as_deref(), Some("Apache-2.0 'x'"));

        let fm = parse("---\nname: a\ndescription: |\n  line one\n  line two\n---\n").unwrap();
        assert_eq!(fm.description, "line one line two");
    }

    #[test]
    fn plain_scalars_continue_on_indented_lines() {
        let fm = parse("---\nname: a\ndescription: Starts here\n  and continues.\n---\n").unwrap();
        assert_eq!(fm.description, "Starts here and continues.");
    }

    #[test]
    fn reads_internal_metadata_and_skips_other_keys() {
        let fm = parse(
            "---\nname: a\ndescription: d\nallowed-tools: Read, Grep\nmetadata:\n  internal: true\n  version: 2\n---\n",
        )
        .unwrap();
        assert!(fm.internal);
    }

    #[test]
    fn missing_fields_are_errors() {
        assert_eq!(parse("# no frontmatter"), Err(FrontmatterError::Missing));
        assert_eq!(parse("---\nname: a\n"), Err(FrontmatterError::Missing));
        assert_eq!(
            parse("---\ndescription: d\n---\n"),
            Err(FrontmatterError::NoName)
        );
        assert_eq!(
            parse("---\nname: a\n---\n"),
            Err(FrontmatterError::NoDescription)
        );
    }

    #[test]
    fn strips_control_characters_and_handles_crlf_and_bom() {
        let fm = parse("\u{feff}---\r\nname: a\r\ndescription: x\u{1b}[31my\u{202e}z\r\n---\r\n")
            .unwrap();
        assert_eq!(fm.name, "a");
        assert_eq!(fm.description, "x[31myz");
        assert_eq!(block("---\nk: v\n---"), Some("k: v\n"));
    }

    #[test]
    fn validates_names_by_the_spec() {
        assert!(valid_name("security-audit"));
        assert!(valid_name("pdf2"));
        for bad in ["", "-a", "a-", "a--b", "Upper", "a_b", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn install_names_follow_the_skills_cli() {
        assert_eq!(
            install_name("Vercel React Best Practices").as_deref(),
            Some("vercel-react-best-practices")
        );
        assert_eq!(install_name("..a__b..").as_deref(), Some("a__b"));
        assert_eq!(install_name("../../etc").as_deref(), Some("etc"));
        assert_eq!(install_name("---"), None);
    }
}
