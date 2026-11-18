//! Finding the skills inside a cloned repository.
//!
//! Follows the `skills` CLI's search so a repository installs the same skills
//! with either tool:
//!
//! 1. A `SKILL.md` at the root (or at the requested subpath) is the one skill.
//! 2. Otherwise the conventional folders are searched a few levels deep:
//!    `skills/` (and its `.curated`/`.experimental`/`.system` sets), every
//!    agent's project skills folder, the `paths` in `skills.sh.json`, and the
//!    plugin folders a `.claude-plugin/marketplace.json` or `plugin.json` names.
//!    A `SKILL.md` hides any below it.
//! 3. Failing all that, the whole tree is walked (depth 5), skipping build
//!    output, dependencies and test fixtures.

use std::collections::BTreeSet;

use super::frontmatter::{self, Frontmatter, FrontmatterError};
use super::{SKILL_FILE, join, source::clean_subpath};

/// Folders searched under the root (or subpath), in order.
const SEARCH_DIRS: &[&str] = &[
    "skills",
    "skills/.curated",
    "skills/.experimental",
    "skills/.system",
    ".agents/skills",
    ".agent/skills",
    ".claude/skills",
    ".opencode/skills",
    ".opencode/skill",
    ".grok/skills",
    ".pi/skills",
    ".gemini/skills",
    ".cursor/skills",
    ".github/skills",
    ".factory/skills",
    ".codex/skills",
    ".cline/skills",
    ".kilocode/skills",
    ".kilo/skills",
];

/// How deep below a search folder a `SKILL.md` may sit.
const SEARCH_DEPTH: usize = 3;
/// How deep the last-resort walk goes.
const FALLBACK_DEPTH: usize = 5;
/// Folder names the last-resort walk never enters.
const FALLBACK_SKIP: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    "__pycache__",
    "fixtures",
    "__fixtures__",
    "testdata",
];

/// File extensions that mark a skill as shipping code an agent may run.
const SCRIPT_EXTS: &[&str] = &[
    "sh", "bash", "zsh", "py", "js", "cjs", "mjs", "ts", "rb", "pl", "ps1", "bat", "cmd", "exe",
];

/// A skill found in a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundSkill {
    /// The skill's folder relative to the repository root (`""` for the root).
    pub dir: String,
    pub frontmatter: Frontmatter,
    /// How many files the folder holds.
    pub files: usize,
    /// Whether it ships scripts (by extension).
    pub has_scripts: bool,
}

impl FoundSkill {
    /// The repository path of its `SKILL.md`, as the lock's `skillPath`.
    pub fn skill_path(&self) -> String {
        join(&self.dir, SKILL_FILE)
    }
}

/// A `SKILL.md` that was found but isn't usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub dir: String,
    pub reason: SkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Frontmatter(FrontmatterError),
    Unreadable,
    /// `metadata.internal: true`.
    Internal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub skills: Vec<FoundSkill>,
    pub skipped: Vec<Skipped>,
}

/// Find the skills in a repository.
///
/// `files` lists every file in the clone relative to its root, `/`-separated,
/// without `.git`. `read` returns a file's text (used for `SKILL.md`s and the
/// manifests). `subpath` limits the search to one folder; `only` keeps the
/// skill with that frontmatter or folder name.
pub fn discover(
    files: &[String],
    read: &dyn Fn(&str) -> Option<String>,
    subpath: Option<&str>,
    only: Option<&str>,
) -> Discovery {
    let files: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let base = subpath.unwrap_or("").trim_matches('/');
    let skill_dirs = find_dirs(&files, read, base);
    let mut out = Discovery::default();
    for dir in skill_dirs {
        let Some(text) = read(&join(&dir, SKILL_FILE)) else {
            out.skipped.push(Skipped {
                dir,
                reason: SkipReason::Unreadable,
            });
            continue;
        };
        let fm = match frontmatter::parse(&text) {
            Ok(fm) => fm,
            Err(e) => {
                out.skipped.push(Skipped {
                    dir,
                    reason: SkipReason::Frontmatter(e),
                });
                continue;
            }
        };
        if let Some(only) = only
            && fm.name != only
            && super::file_name(&dir) != only
        {
            continue;
        }
        if fm.internal && only.is_none() {
            out.skipped.push(Skipped {
                dir,
                reason: SkipReason::Internal,
            });
            continue;
        }
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let inside: Vec<&&str> = files.iter().filter(|f| f.starts_with(&prefix)).collect();
        let has_scripts = inside.iter().any(|f| {
            f.rsplit_once('.')
                .is_some_and(|(_, ext)| SCRIPT_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        });
        out.skills.push(FoundSkill {
            dir,
            frontmatter: fm,
            files: inside.len(),
            has_scripts,
        });
    }
    out
}

/// Folders holding a `SKILL.md`, in discovery order, without duplicates.
fn find_dirs(
    files: &BTreeSet<&str>,
    read: &dyn Fn(&str) -> Option<String>,
    base: &str,
) -> Vec<String> {
    if files.contains(join(base, SKILL_FILE).as_str()) {
        return vec![base.to_string()];
    }
    let mut found: Vec<String> = Vec::new();
    let add = |dirs: Vec<String>, found: &mut Vec<String>| {
        for d in dirs {
            if !found.contains(&d) {
                found.push(d);
            }
        }
    };
    // Skills directly below the base (depth 1 only).
    add(skills_under(files, base, 1), &mut found);
    let mut search: Vec<String> = SEARCH_DIRS.iter().map(|d| join(base, d)).collect();
    search.extend(manifest_dirs(read, base));
    for dir in search {
        if files.contains(join(&dir, SKILL_FILE).as_str()) {
            add(vec![dir], &mut found);
        } else {
            add(skills_under(files, &dir, SEARCH_DEPTH), &mut found);
        }
    }
    if found.is_empty() {
        found = fallback(files, base);
    }
    found
}

/// Folders below `dir` (at most `depth` levels down) holding a `SKILL.md`,
/// where a `SKILL.md` hides any deeper ones.
fn skills_under(files: &BTreeSet<&str>, dir: &str, depth: usize) -> Vec<String> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut hits: Vec<String> = files
        .iter()
        .filter_map(|f| f.strip_prefix(prefix.as_str()))
        .filter_map(|rel| rel.strip_suffix(&format!("/{SKILL_FILE}")))
        .filter(|rel| rel.split('/').count() <= depth)
        .map(|rel| join(dir, rel))
        .collect();
    hits.sort_by_key(|h| h.split('/').count());
    let mut kept: Vec<String> = Vec::new();
    for h in hits {
        if !kept.iter().any(|k| h.starts_with(&format!("{k}/"))) {
            kept.push(h);
        }
    }
    kept.sort();
    kept
}

fn fallback(files: &BTreeSet<&str>, base: &str) -> Vec<String> {
    let prefix = if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    };
    let candidates: BTreeSet<&str> = files
        .iter()
        .filter(|f| f.starts_with(&prefix))
        .filter(|f| {
            !f[prefix.len()..]
                .split('/')
                .any(|part| FALLBACK_SKIP.contains(&part))
        })
        .copied()
        .collect();
    skills_under(&candidates, base, FALLBACK_DEPTH)
}

/// Extra search folders named by `skills.sh.json` and Claude plugin manifests.
fn manifest_dirs(read: &dyn Fn(&str) -> Option<String>, base: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let json = |rel: &str| {
        read(&join(base, rel)).and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    };
    let rel = |p: &str| -> Option<String> {
        let p = p.trim();
        if !p.starts_with("./") && p != "." {
            return None;
        }
        clean_subpath(p)
            .ok()
            .map(|s| join(base, &s.unwrap_or_default()))
    };
    if let Some(cfg) = json("skills.sh.json")
        && let Some(paths) = cfg.get("paths").and_then(|p| p.as_array())
    {
        dirs.extend(paths.iter().filter_map(|p| p.as_str()).filter_map(rel));
    }
    if let Some(m) = json(".claude-plugin/marketplace.json") {
        let root = m
            .pointer("/metadata/pluginRoot")
            .and_then(|r| r.as_str())
            .and_then(rel)
            .unwrap_or_else(|| base.to_string());
        for plugin in m
            .get("plugins")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
        {
            let plugin_dir = match plugin.get("source").and_then(|s| s.as_str()) {
                Some(s) => match rel(s) {
                    // `./x` in a marketplace is relative to `pluginRoot`.
                    Some(_) => clean_subpath(s)
                        .ok()
                        .map(|sub| join(&root, &sub.unwrap_or_default())),
                    None => None,
                },
                None => None,
            };
            if let Some(pd) = &plugin_dir {
                dirs.push(join(pd, "skills"));
            }
            for skill in plugin
                .get("skills")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str())
            {
                let anchor = plugin_dir.as_deref().unwrap_or(base);
                if skill.starts_with("./")
                    && let Ok(sub) = clean_subpath(skill)
                {
                    dirs.push(join(anchor, &sub.unwrap_or_default()));
                }
            }
        }
    }
    if let Some(p) = json(".claude-plugin/plugin.json") {
        let skills = p.get("skills");
        let list: Vec<&str> = match skills {
            Some(serde_json::Value::String(s)) => vec![s.as_str()],
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => Vec::new(),
        };
        dirs.extend(list.into_iter().filter_map(rel));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{SkipReason, discover};

    fn skill(name: &str) -> String {
        format!("---\nname: {name}\ndescription: The {name} skill.\n---\n")
    }

    fn run(
        tree: &[(&str, String)],
        sub: Option<&str>,
        only: Option<&str>,
    ) -> (Vec<String>, Vec<String>) {
        let map: HashMap<String, String> = tree
            .iter()
            .map(|(p, c)| (p.to_string(), c.clone()))
            .collect();
        let files: Vec<String> = map.keys().cloned().collect();
        let d = discover(&files, &|p| map.get(p).cloned(), sub, only);
        (
            d.skills.iter().map(|s| s.dir.clone()).collect(),
            d.skipped.iter().map(|s| s.dir.clone()).collect(),
        )
    }

    #[test]
    fn cloudflare_layout() {
        let tree = [
            ("README.md", String::new()),
            ("skills/security-audit/SKILL.md", skill("security-audit")),
            ("skills/security-audit/HUNTING.md", String::new()),
            ("skills/security-audit/validate-findings.cjs", String::new()),
        ];
        let map: HashMap<String, String> = tree
            .iter()
            .map(|(p, c)| (p.to_string(), c.clone()))
            .collect();
        let files: Vec<String> = map.keys().cloned().collect();
        let d = discover(&files, &|p| map.get(p).cloned(), None, None);
        assert_eq!(d.skills.len(), 1);
        let s = &d.skills[0];
        assert_eq!(s.dir, "skills/security-audit");
        assert_eq!(s.skill_path(), "skills/security-audit/SKILL.md");
        assert_eq!(s.files, 3);
        assert!(s.has_scripts);
    }

    #[test]
    fn root_skill_wins() {
        let tree = [
            ("SKILL.md", skill("root")),
            ("skills/other/SKILL.md", skill("other")),
        ];
        assert_eq!(run(&tree, None, None).0, [""]);
    }

    #[test]
    fn subpath_and_only_filter() {
        let tree = [
            ("skills/pdf/SKILL.md", skill("pdf")),
            ("skills/docx/SKILL.md", skill("docx")),
        ];
        assert_eq!(run(&tree, Some("skills/pdf"), None).0, ["skills/pdf"]);
        assert_eq!(run(&tree, None, Some("docx")).0, ["skills/docx"]);
        assert_eq!(run(&tree, None, None).0, ["skills/docx", "skills/pdf"]);
    }

    #[test]
    fn marketplace_plugins_are_searched() {
        let tree = [
            (
                ".claude-plugin/marketplace.json",
                r#"{"plugins":[{"name":"p","source":"./plugins/p"},{"name":"remote","source":{"source":"github"}}]}"#.to_string(),
            ),
            ("plugins/p/skills/diff-review/SKILL.md", skill("diff-review")),
            ("plugins/p/tests/fixtures/x/SKILL.md", skill("fixture")),
        ];
        assert_eq!(run(&tree, None, None).0, ["plugins/p/skills/diff-review"]);
    }

    #[test]
    fn fallback_skips_fixtures_and_dependencies() {
        let tree = [
            ("tools/a/SKILL.md", skill("a")),
            ("node_modules/pkg/SKILL.md", skill("dep")),
            ("x/tests/fixtures/y/SKILL.md", skill("fixture")),
        ];
        assert_eq!(run(&tree, None, None).0, ["tools/a"]);
    }

    #[test]
    fn a_skill_hides_nested_ones() {
        let tree = [
            ("skills/outer/SKILL.md", skill("outer")),
            ("skills/outer/refs/inner/SKILL.md", skill("inner")),
        ];
        assert_eq!(run(&tree, None, None).0, ["skills/outer"]);
    }

    #[test]
    fn bad_and_internal_skills_are_skipped() {
        let tree = [
            ("skills/bad/SKILL.md", "no frontmatter".to_string()),
            (
                "skills/hidden/SKILL.md",
                "---\nname: hidden\ndescription: d\nmetadata:\n  internal: true\n---\n".to_string(),
            ),
            ("skills/ok/SKILL.md", skill("ok")),
        ];
        let (found, skipped) = run(&tree, None, None);
        assert_eq!(found, ["skills/ok"]);
        assert_eq!(skipped, ["skills/bad", "skills/hidden"]);
        // Asking for an internal skill by name installs it.
        assert_eq!(run(&tree, None, Some("hidden")).0, ["skills/hidden"]);
    }

    #[test]
    fn skills_sh_paths_must_stay_inside() {
        let tree = [
            (
                "skills.sh.json",
                r#"{"paths":["./extra","../outside","/abs"]}"#.to_string(),
            ),
            ("extra/one/SKILL.md", skill("one")),
        ];
        assert_eq!(run(&tree, None, None).0, ["extra/one"]);
        let _ = SkipReason::Internal;
    }
}
