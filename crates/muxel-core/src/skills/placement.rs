//! Where to put a skill so exactly the chosen agents see it, and the file
//! operations that get it there from where it is now.
//!
//! An agent sees a skill when one of the folders it reads holds it (see
//! [`SkillHome::read_dirs`]). Folders are shared — `~/.agents/skills` is read by
//! most agents, `~/.claude/skills` by Claude Code and, for compatibility, by
//! opencode, Amp and Grok — so a choice of agents can't always be met exactly.
//! The planner picks the set of folders that reaches every chosen agent while
//! exposing the skill to as few other agents as possible, then as few folders as
//! possible, and reports who will see it.
//!
//! One folder holds the real copy (the *canonical* one): the shared folder when
//! it's used, as the `skills` CLI does. The others get a link to it — a relative
//! symlink, or a copy where symlinks aren't dependable (Windows).

use std::collections::BTreeSet;

use super::{Scope, SkillAgent, SkillHome, join, norm};

/// How a non-canonical folder refers to the canonical copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMode {
    Symlink,
    /// A full copy (Windows).
    Copy,
}

/// The folders a skill should be in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The folder holding the real copy.
    pub canonical: String,
    /// Folders holding a link (or copy) of it.
    pub links: Vec<String>,
    /// Every agent (chosen or detected) that will see it — the chosen ones
    /// plus any that read one of these folders too.
    pub visible_to: BTreeSet<SkillAgent>,
}

impl Placement {
    pub fn dirs(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.canonical).chain(&self.links)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// No agent chosen.
    NoAgents,
    /// This agent has no skills folder at this scope (Hermes in a project).
    Unreachable(SkillAgent),
}

/// The agents (among `agents`) that read any of `dirs`.
pub fn visible_to(
    home: &SkillHome,
    scope: &Scope,
    agents: &[SkillAgent],
    dirs: &[String],
) -> BTreeSet<SkillAgent> {
    agents
        .iter()
        .copied()
        .filter(|&a| dirs.iter().any(|d| home.reads(a, scope, d)))
        .collect()
}

/// Plan where a skill goes so `selected` agents see it. `detected` are the
/// agents installed on the machine (whose exposure counts against a plan).
/// `prefer` keeps an existing canonical folder in place when the plan uses it.
pub fn plan(
    home: &SkillHome,
    scope: &Scope,
    detected: &[SkillAgent],
    selected: &BTreeSet<SkillAgent>,
    prefer: Option<&str>,
) -> Result<Placement, PlanError> {
    if selected.is_empty() {
        return Err(PlanError::NoAgents);
    }
    let shared = home.shared_dir(scope);
    let mut candidates = vec![shared.clone()];
    for &a in selected {
        let own = home.own_dir(a, scope).ok_or(PlanError::Unreachable(a))?;
        if !candidates.contains(&own) {
            candidates.push(own);
        }
    }
    let mut everyone: Vec<SkillAgent> = detected.to_vec();
    for &a in selected {
        if !everyone.contains(&a) {
            everyone.push(a);
        }
    }
    // (leaked agents, folder count, uses shared ? 0 : 1, mask)
    let mut best: Option<(usize, usize, usize, u32)> = None;
    for mask in 1u32..(1 << candidates.len()) {
        let dirs: Vec<String> = (0..candidates.len())
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| candidates[i].clone())
            .collect();
        let seen = visible_to(home, scope, &everyone, &dirs);
        if !selected.iter().all(|a| seen.contains(a)) {
            continue;
        }
        let leaked = seen.iter().filter(|a| !selected.contains(a)).count();
        let cost = (leaked, dirs.len(), usize::from(mask & 1 == 0), mask);
        if best.is_none_or(|b| cost < b) {
            best = Some(cost);
        }
    }
    // Every selected agent's own folder is a candidate, so the full set always
    // covers them.
    let (_, _, _, mask) = best.expect("the full candidate set covers every selected agent");
    let dirs: Vec<String> = (0..candidates.len())
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| candidates[i].clone())
        .collect();
    let prefer = prefer.map(norm);
    let canonical = prefer
        .filter(|p| dirs.contains(p))
        .unwrap_or_else(|| dirs[0].clone());
    let visible = visible_to(home, scope, &everyone, &dirs);
    Ok(Placement {
        links: dirs.into_iter().filter(|d| *d != canonical).collect(),
        canonical,
        visible_to: visible,
    })
}

/// What sits at `<dir>/<name>` now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocKind {
    /// A real folder.
    Real,
    /// A symlink, with its resolved absolute target.
    Link { target: String },
}

/// One place a skill is installed now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Current {
    /// The skills folder (not the skill's own folder).
    pub dir: String,
    pub kind: LocKind,
    /// Synced or bundled: never changed.
    pub protected: bool,
}

/// One file operation on `<dir>/<name>`. Each replaces whatever is at its
/// destination (a link is removed, never followed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillOp {
    /// Write the fetched skill's files into `dir`.
    Write { dir: String },
    /// Move the real copy from one skills folder to another.
    Move { from: String, to: String },
    /// Point `dir`'s entry at the canonical copy in `target_dir` (a symlink, or
    /// a copy of it in [`LinkMode::Copy`]).
    Link { dir: String, target_dir: String },
    /// Delete `dir`'s entry (a link itself, or a real folder).
    Remove { dir: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffError {
    /// Nothing to copy from: no real, changeable copy exists and no new content
    /// was fetched.
    NoSource,
}

/// The operations that turn `current` into `placement` for skill `name`.
/// `fresh` means new content was fetched (install or update) and is written to
/// the canonical folder.
pub fn diff(
    placement: &Placement,
    current: &[Current],
    name: &str,
    mode: LinkMode,
    fresh: bool,
) -> Result<Vec<SkillOp>, DiffError> {
    let canonical = &placement.canonical;
    let canonical_path = join(canonical, name);
    let at = |dir: &str| current.iter().find(|c| norm(&c.dir) == norm(dir));
    let mut ops = Vec::new();
    let mut moved_from: Option<String> = None;
    if fresh {
        ops.push(SkillOp::Write {
            dir: canonical.clone(),
        });
    } else if !at(canonical).is_some_and(|c| c.kind == LocKind::Real && !c.protected) {
        // Prefer the copy the links point at; else any changeable real copy.
        let real: Vec<&Current> = current
            .iter()
            .filter(|c| c.kind == LocKind::Real && !c.protected)
            .collect();
        let pointed = real.iter().find(|r| {
            current.iter().any(|c| {
                matches!(&c.kind, LocKind::Link { target } if norm(target) == norm(&join(&r.dir, name)))
            })
        });
        let from = pointed.or(real.first()).ok_or(DiffError::NoSource)?;
        ops.push(SkillOp::Move {
            from: from.dir.clone(),
            to: canonical.clone(),
        });
        moved_from = Some(norm(&from.dir));
    }
    for dir in &placement.links {
        let ok = match (at(dir).map(|c| &c.kind), mode) {
            (Some(LocKind::Link { target }), LinkMode::Symlink) => {
                norm(target) == norm(&canonical_path)
            }
            (Some(LocKind::Real), LinkMode::Copy) => !fresh,
            _ => false,
        };
        if !ok {
            ops.push(SkillOp::Link {
                dir: dir.clone(),
                target_dir: canonical.clone(),
            });
        }
    }
    let wanted: BTreeSet<String> = placement.dirs().map(|d| norm(d)).collect();
    for c in current {
        let d = norm(&c.dir);
        if c.protected || wanted.contains(&d) || moved_from.as_deref() == Some(d.as_str()) {
            continue;
        }
        ops.push(SkillOp::Remove { dir: c.dir.clone() });
    }
    Ok(ops)
}

/// The operations that remove a skill from everywhere it can be changed: links
/// first, then real copies.
pub fn remove_all(current: &[Current]) -> Vec<SkillOp> {
    let mut ops: Vec<SkillOp> = current
        .iter()
        .filter(|c| !c.protected && matches!(c.kind, LocKind::Link { .. }))
        .map(|c| SkillOp::Remove { dir: c.dir.clone() })
        .collect();
    ops.extend(
        current
            .iter()
            .filter(|c| !c.protected && c.kind == LocKind::Real)
            .map(|c| SkillOp::Remove { dir: c.dir.clone() }),
    );
    ops
}

/// The operations that update a skill in place: every changeable real copy is
/// rewritten; symlinks follow the canonical one.
pub fn update_ops(current: &[Current]) -> Vec<SkillOp> {
    current
        .iter()
        .filter(|c| !c.protected && c.kind == LocKind::Real)
        .map(|c| SkillOp::Write { dir: c.dir.clone() })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        Current, DiffError, LinkMode, LocKind, PlanError, SkillOp, diff, plan, remove_all,
        update_ops,
    };
    use crate::RemoteOs;
    use crate::skills::{Scope, SkillAgent, SkillHome};

    use SkillAgent::*;

    fn home() -> SkillHome {
        SkillHome::new("/h", RemoteOs::Unix)
    }

    fn set(a: &[SkillAgent]) -> BTreeSet<SkillAgent> {
        a.iter().copied().collect()
    }

    /// A typical mix of installed agents.
    const DETECTED: &[SkillAgent] = &[Claude, Opencode, Amp, Grok, Pi, Hermes];

    #[test]
    fn all_agents_use_the_shared_folder_plus_links() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(DETECTED), None).unwrap();
        assert_eq!(p.canonical, "/h/.agents/skills");
        assert_eq!(p.links, ["/h/.claude/skills", "/h/.hermes/skills"]);
        assert_eq!(p.visible_to, set(DETECTED));
    }

    #[test]
    fn universal_agents_only_need_the_shared_folder() {
        let sel = set(&[Opencode, Pi]);
        let p = plan(&home(), &Scope::User, DETECTED, &sel, None).unwrap();
        // The shared folder would also reach Amp and Grok; opencode's and pi's
        // own folders reach nobody else.
        assert_eq!(p.canonical, "/h/.config/opencode/skills");
        assert_eq!(p.links, ["/h/.pi/agent/skills"]);
        assert_eq!(p.visible_to, sel);
    }

    #[test]
    fn shared_folder_when_every_reader_is_chosen() {
        let sel = set(&[Opencode, Amp, Grok, Pi]);
        let p = plan(&home(), &Scope::User, DETECTED, &sel, None).unwrap();
        assert_eq!(p.canonical, "/h/.agents/skills");
        assert!(p.links.is_empty());
    }

    #[test]
    fn claude_alone_reports_the_agents_that_also_read_its_folder() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(&[Claude]), None).unwrap();
        assert_eq!(p.canonical, "/h/.claude/skills");
        assert!(p.links.is_empty());
        assert_eq!(p.visible_to, set(&[Claude, Opencode, Amp, Grok]));
    }

    #[test]
    fn prefer_keeps_the_existing_canonical_folder() {
        let p = plan(
            &home(),
            &Scope::User,
            DETECTED,
            &set(DETECTED),
            Some("/h/.claude/skills/"),
        )
        .unwrap();
        assert_eq!(p.canonical, "/h/.claude/skills");
        assert_eq!(p.links, ["/h/.agents/skills", "/h/.hermes/skills"]);
    }

    #[test]
    fn project_scope_and_errors() {
        let s = Scope::Project("/p".into());
        // Claude's project folder already reaches opencode, Amp and Grok, so
        // the shared one costs nothing extra and is preferred over pi's own.
        let p = plan(&home(), &s, DETECTED, &set(&[Claude, Pi]), None).unwrap();
        assert_eq!(p.canonical, "/p/.agents/skills");
        assert_eq!(p.links, ["/p/.claude/skills"]);
        assert_eq!(
            plan(&home(), &s, DETECTED, &set(&[Hermes]), None),
            Err(PlanError::Unreachable(Hermes))
        );
        assert_eq!(
            plan(&home(), &Scope::User, DETECTED, &set(&[]), None),
            Err(PlanError::NoAgents)
        );
    }

    fn real(dir: &str) -> Current {
        Current {
            dir: dir.into(),
            kind: LocKind::Real,
            protected: false,
        }
    }

    fn link(dir: &str, target: &str) -> Current {
        Current {
            dir: dir.into(),
            kind: LocKind::Link {
                target: target.into(),
            },
            protected: false,
        }
    }

    #[test]
    fn fresh_install_writes_then_links() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(DETECTED), None).unwrap();
        let ops = diff(&p, &[], "x", LinkMode::Symlink, true).unwrap();
        assert_eq!(
            ops,
            [
                SkillOp::Write {
                    dir: "/h/.agents/skills".into()
                },
                SkillOp::Link {
                    dir: "/h/.claude/skills".into(),
                    target_dir: "/h/.agents/skills".into()
                },
                SkillOp::Link {
                    dir: "/h/.hermes/skills".into(),
                    target_dir: "/h/.agents/skills".into()
                },
            ]
        );
    }

    #[test]
    fn existing_links_are_kept_and_stray_copies_removed() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(DETECTED), None).unwrap();
        let current = [
            real("/h/.agents/skills"),
            link("/h/.claude/skills", "/h/.agents/skills/x"),
            // A hand-made copy in Grok's folder (Grok reads the shared one).
            real("/h/.grok/skills"),
        ];
        let ops = diff(&p, &current, "x", LinkMode::Symlink, false).unwrap();
        assert_eq!(
            ops,
            [
                SkillOp::Link {
                    dir: "/h/.hermes/skills".into(),
                    target_dir: "/h/.agents/skills".into()
                },
                SkillOp::Remove {
                    dir: "/h/.grok/skills".into()
                },
            ]
        );
    }

    #[test]
    fn narrowing_moves_the_canonical_copy_out_of_the_shared_folder() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(&[Claude]), None).unwrap();
        let current = [
            real("/h/.agents/skills"),
            link("/h/.claude/skills", "/h/.agents/skills/x"),
        ];
        let ops = diff(&p, &current, "x", LinkMode::Symlink, false).unwrap();
        assert_eq!(
            ops,
            [SkillOp::Move {
                from: "/h/.agents/skills".into(),
                to: "/h/.claude/skills".into()
            }]
        );
    }

    #[test]
    fn copy_mode_keeps_copies_unless_fresh() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(DETECTED), None).unwrap();
        let current = [
            real("/h/.agents/skills"),
            real("/h/.claude/skills"),
            real("/h/.hermes/skills"),
        ];
        assert!(
            diff(&p, &current, "x", LinkMode::Copy, false)
                .unwrap()
                .is_empty()
        );
        let fresh = diff(&p, &current, "x", LinkMode::Copy, true).unwrap();
        assert_eq!(fresh.len(), 3);
    }

    #[test]
    fn protected_copies_are_never_touched() {
        let p = plan(&home(), &Scope::User, DETECTED, &set(&[Pi]), None).unwrap();
        let synced = Current {
            protected: true,
            ..real("/h/.claude/skills/synced/b")
        };
        assert_eq!(
            diff(
                &p,
                std::slice::from_ref(&synced),
                "x",
                LinkMode::Symlink,
                false
            ),
            Err(DiffError::NoSource)
        );
        let ops = diff(
            &p,
            &[synced.clone(), real("/h/.agents/skills")],
            "x",
            LinkMode::Symlink,
            false,
        )
        .unwrap();
        assert_eq!(
            ops,
            [SkillOp::Move {
                from: "/h/.agents/skills".into(),
                to: "/h/.pi/agent/skills".into()
            }]
        );
        assert!(remove_all(std::slice::from_ref(&synced)).is_empty());
        assert!(update_ops(&[synced]).is_empty());
    }

    #[test]
    fn remove_all_takes_links_first() {
        let ops = remove_all(&[
            real("/h/.agents/skills"),
            link("/h/.claude/skills", "/h/.agents/skills/x"),
        ]);
        assert_eq!(
            ops,
            [
                SkillOp::Remove {
                    dir: "/h/.claude/skills".into()
                },
                SkillOp::Remove {
                    dir: "/h/.agents/skills".into()
                },
            ]
        );
    }
}
