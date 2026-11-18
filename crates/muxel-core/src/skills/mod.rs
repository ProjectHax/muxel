//! Agent Skills: folders holding a `SKILL.md` (YAML frontmatter `name` +
//! `description`) that coding agents load on demand.
//!
//! Every agent reads them from its own set of folders (see [`agents`]). muxel's
//! skill manager lists what each installed agent can see, installs skills from a
//! git repository, changes which agents a skill is available to, updates and
//! removes them — on this computer or an SSH host, for the user or one project.
//!
//! This module is the pure half: the agent table, frontmatter and source
//! parsing, discovery inside a cloned repository, the placement planner, the
//! inventory that merges a scan into one row per skill, the `skills` CLI lock
//! format, the curated catalog and the remote command strings. The app crate
//! does the I/O.
//!
//! Paths are `String`s with `/` separators throughout, because the same code
//! serves local folders and remote (possibly Windows) hosts; Windows accepts `/`.

pub mod agents;
pub mod curated;
pub mod discover;
pub mod frontmatter;
pub mod inventory;
pub mod lock;
pub mod placement;
pub mod remote;
pub mod source;
pub mod tar;

pub use agents::{Scope, SkillAgent, SkillHome};

/// The file that makes a folder a skill.
pub const SKILL_FILE: &str = "SKILL.md";
/// muxel's record of where a skill it installed came from, kept inside the
/// skill's canonical folder so it travels with the skill (projects, remotes).
pub const SIDECAR_FILE: &str = ".muxel-skill.json";
/// Largest skill folder muxel copies, in bytes (the `skills` CLI's limit).
pub const MAX_SKILL_BYTES: u64 = 25 * 1024 * 1024;
/// Most files muxel copies from one skill folder.
pub const MAX_SKILL_FILES: usize = 1000;
/// Largest `SKILL.md` muxel reads, in bytes.
pub const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
/// How deep below a skills folder a scan looks for `SKILL.md`. Three levels
/// reaches Hermes's `<category>/<skill>` layout and Claude's
/// `synced/<bucket>/<skill>`.
pub const SCAN_DEPTH: usize = 3;
/// Names never copied out of a source repository into an installed skill.
pub const COPY_EXCLUDES: &[&str] = &[".git", "metadata.json", "__pycache__", "__pypackages__"];

/// Join two `/`-separated path parts.
pub fn join(base: &str, rel: &str) -> String {
    let base = base.trim_end_matches(['/', '\\']);
    let rel = rel.trim_start_matches('/');
    if base.is_empty() {
        return rel.to_string();
    }
    if rel.is_empty() {
        return base.to_string();
    }
    format!("{base}/{rel}")
}

/// `\` → `/` and no trailing separator, for comparing paths.
pub fn norm(path: &str) -> String {
    let p = path.replace('\\', "/");
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() && p.starts_with('/') {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The last component of a path.
pub fn file_name(path: &str) -> &str {
    let p = path.trim_end_matches(['/', '\\']);
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// The path with its last component removed.
pub fn parent(path: &str) -> &str {
    let p = path.trim_end_matches(['/', '\\']);
    match p.rfind(['/', '\\']) {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "",
    }
}

/// Whether `path` is `dir` or below it (both compared normalized).
pub fn is_within(path: &str, dir: &str) -> bool {
    let p = norm(path);
    let d = norm(dir);
    p == d || p.starts_with(&format!("{}/", d.trim_end_matches('/')))
}

/// The relative path from directory `from_dir` to `to`, for a symlink placed in
/// `from_dir`. Both must be absolute in the same tree.
pub fn relative_path(from_dir: &str, to: &str) -> String {
    let from = norm(from_dir);
    let to = norm(to);
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; a.len() - common];
    parts.extend(&b[common..]);
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Resolve a symlink target read from a link that lives in `link_dir`: absolute
/// targets are kept, relative ones are joined on and `.`/`..` collapsed.
pub fn resolve_link(link_dir: &str, target: &str) -> String {
    let t = target.replace('\\', "/");
    let absolute = t.starts_with('/') || t.as_bytes().get(1) == Some(&b':');
    let joined = if absolute { t } else { join(link_dir, &t) };
    collapse(&joined)
}

/// Collapse `.` and `..` components without touching the filesystem.
pub fn collapse(path: &str) -> String {
    let p = path.replace('\\', "/");
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let body = out.join("/");
    if rooted { format!("/{body}") } else { body }
}

#[cfg(test)]
mod tests {
    use super::{collapse, file_name, is_within, join, norm, parent, relative_path, resolve_link};

    #[test]
    fn joins_and_normalizes() {
        assert_eq!(join("/h/", "/.claude/skills"), "/h/.claude/skills");
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", ""), "a");
        assert_eq!(norm("C:\\Users\\me\\"), "C:/Users/me");
        assert_eq!(norm("/"), "/");
        assert_eq!(file_name("/h/.agents/skills/x/"), "x");
        assert_eq!(parent("/h/.agents/skills/x"), "/h/.agents/skills");
        assert_eq!(parent("/x"), "/");
    }

    #[test]
    fn within_requires_a_component_boundary() {
        assert!(is_within("/h/.claude/skills/x", "/h/.claude/skills"));
        assert!(is_within("/h/.claude/skills", "/h/.claude/skills/"));
        assert!(!is_within("/h/.claude/skills2/x", "/h/.claude/skills"));
    }

    #[test]
    fn relative_links_climb_to_the_common_root() {
        assert_eq!(
            relative_path("/h/.claude/skills", "/h/.agents/skills/x"),
            "../../.agents/skills/x"
        );
        assert_eq!(
            relative_path("/h/.config/opencode/skills", "/h/.agents/skills/x"),
            "../../../.agents/skills/x"
        );
        assert_eq!(relative_path("/a/b", "/a/b/c"), "c");
    }

    #[test]
    fn resolves_relative_and_absolute_link_targets() {
        assert_eq!(
            resolve_link("/h/.claude/skills", "../../.agents/skills/x"),
            "/h/.agents/skills/x"
        );
        assert_eq!(resolve_link("/h/a", "/opt/x"), "/opt/x");
        assert_eq!(resolve_link("C:/u/a", "C:\\s\\x"), "C:/s/x");
        assert_eq!(collapse("a/./b/../c"), "a/c");
    }
}
