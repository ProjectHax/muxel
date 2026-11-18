//! Turning a scan of every agent's skills folders into one row per skill.
//!
//! The same skill commonly sits in several folders — a real copy in
//! `~/.agents/skills` and symlinks to it elsewhere, or a hand-made copy per
//! agent — so rows are keyed by the skill's folder name and list every place it
//! was found.

use std::collections::{BTreeMap, BTreeSet};

use super::agents::Protection;
use super::lock::{LockEntry, SkillLock, parse_sidecar};
use super::placement::{Current, LocKind};
use super::{Scope, SkillAgent, SkillHome, file_name, frontmatter, join, norm, parent};

/// One skill folder a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanEntry {
    /// The skills folder that was scanned.
    pub root: String,
    /// The skill's folder below `root`: `x`, or `category/x` (Hermes), or
    /// `synced/<bucket>/x` (Claude).
    pub rel: String,
    /// When the skill's folder is a symlink: its resolved absolute target.
    pub link_target: Option<String>,
    /// The link's target doesn't exist.
    pub broken: bool,
    /// The `SKILL.md` text (at least its frontmatter), when readable.
    pub skill_md: Option<String>,
    /// The muxel sidecar's text, when present.
    pub sidecar: Option<String>,
}

/// One place a skill is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The skills folder scanned.
    pub root: String,
    /// The folder directly holding the skill's folder.
    pub dir: String,
    /// The skill's folder.
    pub path: String,
    pub kind: LocKind,
    pub broken: bool,
    pub protection: Option<Protection>,
    /// Below a category folder (`root/category/x`): left where it is when
    /// availability changes, though it can still be removed.
    pub nested: bool,
}

/// One skill, wherever it's installed.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillRow {
    /// The folder name every location shares.
    pub key: String,
    /// Frontmatter `name`, else the folder name.
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub locations: Vec<Location>,
    /// Where it came from: muxel's sidecar, else the `skills` CLI's lock.
    pub record: Option<LockEntry>,
}

impl SkillRow {
    /// Whether `agent` loads this skill from one of its locations.
    pub fn seen_by(&self, home: &SkillHome, scope: &Scope, agent: SkillAgent) -> bool {
        self.locations
            .iter()
            .filter(|l| !l.broken)
            .any(|l| match l.protection {
                Some(Protection::Bundled(b)) => b == agent,
                Some(Protection::Synced) => agent == SkillAgent::Claude,
                None if l.nested => agent == SkillAgent::Hermes && home.is_hermes_dir(&l.root),
                None => home.reads(agent, scope, &l.dir),
            })
    }

    /// The agents among `agents` that load it.
    pub fn visible_to(
        &self,
        home: &SkillHome,
        scope: &Scope,
        agents: &[SkillAgent],
    ) -> BTreeSet<SkillAgent> {
        agents
            .iter()
            .copied()
            .filter(|&a| self.seen_by(home, scope, a))
            .collect()
    }

    /// Why it can't be changed, when no location can.
    pub fn read_only(&self) -> Option<Protection> {
        if self.locations.iter().all(|l| l.protection.is_some()) {
            self.locations.first().and_then(|l| l.protection)
        } else {
            None
        }
    }

    /// The real, changeable copy the others refer to: one that links point at,
    /// else the first real one.
    pub fn canonical(&self) -> Option<&Location> {
        let real = || {
            self.locations
                .iter()
                .filter(|l| l.kind == LocKind::Real && l.protection.is_none())
        };
        real()
            .find(|r| {
                self.locations.iter().any(|l| {
                    matches!(&l.kind, LocKind::Link { target } if norm(target) == norm(&r.path))
                })
            })
            .or_else(|| real().find(|r| !r.nested))
            .or_else(|| real().next())
    }

    /// The locations as the placement planner sees them. When `for_removal`,
    /// nested copies count as removable; otherwise they stay where they are.
    pub fn current(&self, for_removal: bool) -> Vec<Current> {
        self.locations
            .iter()
            .map(|l| Current {
                dir: l.dir.clone(),
                kind: l.kind.clone(),
                protected: l.protection.is_some() || (l.nested && !for_removal),
            })
            .collect()
    }
}

/// Merge a scan into rows, sorted by name. `hermes_bundled` is Hermes's
/// manifest; `lock` is the `skills` CLI's lock on that machine.
pub fn build(
    entries: Vec<ScanEntry>,
    home: &SkillHome,
    scope: &Scope,
    hermes_bundled: &[String],
    lock: &SkillLock,
) -> Vec<SkillRow> {
    let mut groups: BTreeMap<String, Vec<ScanEntry>> = BTreeMap::new();
    for e in entries {
        let key = file_name(&e.rel).to_string();
        if key.is_empty() || key.starts_with('.') {
            continue;
        }
        groups.entry(key).or_default().push(e);
    }
    let shared = home.shared_dir(scope);
    let mut rows: Vec<SkillRow> = groups
        .into_iter()
        .map(|(key, entries)| {
            let mut name = String::new();
            let mut description = String::new();
            let mut license = None;
            let mut sidecar = None;
            let mut locations = Vec::new();
            for e in &entries {
                if name.is_empty()
                    && let Some(fm) = e
                        .skill_md
                        .as_deref()
                        .and_then(|t| frontmatter::parse(t).ok())
                {
                    name = fm.name;
                    description = fm.description;
                    license = fm.license;
                }
                if sidecar.is_none() && e.link_target.is_none() {
                    sidecar = e.sidecar.as_deref().and_then(parse_sidecar);
                }
                let path = join(&e.root, &e.rel);
                let rel_parent = parent(&e.rel);
                locations.push(Location {
                    root: norm(&e.root),
                    dir: norm(&join(&e.root, rel_parent)),
                    kind: match &e.link_target {
                        Some(t) => LocKind::Link { target: norm(t) },
                        None => LocKind::Real,
                    },
                    broken: e.broken,
                    protection: home.protection(&path, hermes_bundled),
                    nested: !rel_parent.is_empty(),
                    path: norm(&path),
                });
            }
            if name.is_empty() {
                name = key.clone();
            }
            let in_shared = locations.iter().any(|l| l.dir == shared);
            let record = sidecar.or_else(|| {
                (*scope == Scope::User && in_shared)
                    .then(|| lock.skills.get(&key).cloned())
                    .flatten()
            });
            SkillRow {
                key,
                name,
                description,
                license,
                locations,
                record,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.key.cmp(&b.key)));
    rows
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{ScanEntry, build};
    use crate::RemoteOs;
    use crate::skills::agents::Protection;
    use crate::skills::lock::parse_lock;
    use crate::skills::placement::LocKind;
    use crate::skills::{Scope, SkillAgent, SkillHome};

    use SkillAgent::*;

    fn md(name: &str) -> Option<String> {
        Some(format!(
            "---\nname: {name}\ndescription: About {name}.\n---\n"
        ))
    }

    fn entry(root: &str, rel: &str) -> ScanEntry {
        ScanEntry {
            root: root.into(),
            rel: rel.into(),
            skill_md: md(crate::skills::file_name(rel)),
            ..Default::default()
        }
    }

    const ALL: &[SkillAgent] = &[Claude, Opencode, Amp, Grok, Pi, Hermes, Codex];

    #[test]
    fn groups_copies_and_links_into_one_row() {
        let home = SkillHome::new("/h", RemoteOs::Unix);
        let lock = parse_lock(Some(
            r#"{"version":3,"skills":{"security-audit":{"source":"cloudflare/security-audit-skill","sourceType":"github","sourceUrl":"u"}}}"#,
        ))
        .unwrap();
        let rows = build(
            vec![
                entry("/h/.agents/skills", "security-audit"),
                entry("/h/.grok/skills", "security-audit"),
                ScanEntry {
                    link_target: Some("/h/.agents/skills/security-audit".into()),
                    ..entry("/h/.claude/skills", "security-audit")
                },
            ],
            &home,
            &Scope::User,
            &[],
            &lock,
        );
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.name, "security-audit");
        assert_eq!(r.description, "About security-audit.");
        assert_eq!(r.locations.len(), 3);
        assert_eq!(
            r.record.as_ref().unwrap().source,
            "cloudflare/security-audit-skill"
        );
        assert_eq!(r.canonical().unwrap().dir, "/h/.agents/skills");
        assert_eq!(
            r.visible_to(&home, &Scope::User, ALL),
            [Claude, Opencode, Amp, Grok, Pi, Codex]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(r.read_only(), None);
    }

    #[test]
    fn bundled_synced_and_nested_skills() {
        let home = SkillHome::new("/h", RemoteOs::Unix);
        let lock = parse_lock(None).unwrap();
        let rows = build(
            vec![
                entry("/h/.claude/skills", "synced/bucket/docx"),
                entry("/h/.hermes/skills", "research/arxiv"),
                entry("/h/.hermes/skills", "mine/tool"),
                entry("/h/.grok/bundled/skills", "review"),
                entry("/h/.agents/skills", ".hidden"),
            ],
            &home,
            &Scope::User,
            &["arxiv".to_string()],
            &lock,
        );
        let names: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(names, ["arxiv", "docx", "review", "tool"]);
        assert_eq!(rows[0].read_only(), Some(Protection::Bundled(Hermes)));
        assert_eq!(rows[1].read_only(), Some(Protection::Synced));
        assert_eq!(
            rows[1].visible_to(&home, &Scope::User, ALL),
            [Claude].into_iter().collect::<BTreeSet<_>>()
        );
        assert_eq!(
            rows[2].visible_to(&home, &Scope::User, ALL),
            [Grok].into_iter().collect::<BTreeSet<_>>()
        );
        let tool = &rows[3];
        assert_eq!(tool.read_only(), None);
        assert!(tool.locations[0].nested);
        assert_eq!(tool.locations[0].dir, "/h/.hermes/skills/mine");
        assert_eq!(
            tool.visible_to(&home, &Scope::User, ALL),
            [Hermes].into_iter().collect::<BTreeSet<_>>()
        );
        // Kept in place when availability changes, removable on delete.
        assert!(tool.current(false)[0].protected);
        assert!(!tool.current(true)[0].protected);
    }

    #[test]
    fn sidecar_wins_and_broken_links_hide_nothing() {
        let home = SkillHome::new("/h", RemoteOs::Unix);
        let lock = parse_lock(None).unwrap();
        let rows = build(
            vec![
                ScanEntry {
                    sidecar: Some(
                        r#"{"source":"o/r","sourceType":"github","sourceUrl":"u"}"#.into(),
                    ),
                    ..entry("/p/.claude/skills", "x")
                },
                ScanEntry {
                    link_target: Some("/gone/x".into()),
                    broken: true,
                    skill_md: None,
                    ..entry("/p/.pi/skills", "x")
                },
            ],
            &home,
            &Scope::Project("/p".into()),
            &[],
            &lock,
        );
        let r = &rows[0];
        assert_eq!(r.record.as_ref().unwrap().source, "o/r");
        assert!(matches!(r.locations[1].kind, LocKind::Link { .. }));
        assert!(!r.seen_by(&home, &Scope::Project("/p".into()), Pi));
        assert!(r.seen_by(&home, &Scope::Project("/p".into()), Claude));
    }
}
