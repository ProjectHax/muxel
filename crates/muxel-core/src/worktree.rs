//! Pure helpers for naming git worktrees and branches, plus the [`Worktree`]
//! registry record. The binary runs the actual `git worktree` commands.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A named git worktree, owned by one project and shared by one or more agent
/// instances. Promotes the old per-instance `worktree_path`/`worktree_branch`
/// into a first-class entity so worktrees can be named, colored, and shared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    pub id: Uuid,
    pub project_id: Uuid,
    /// Auto-assigned adjective-noun display name; user-renamable. Decoupled from
    /// the git branch, so renaming never touches git.
    pub name: String,
    pub path: PathBuf,
    /// Git branch (`muxel/<id8>`); stable for the worktree's lifetime.
    pub branch: String,
    /// Index into the app's worktree color palette.
    pub color: u8,
    /// True when no instance references it but it's kept on disk (resumable).
    #[serde(default)]
    pub detached: bool,
}

const ADJECTIVES: &[&str] = &[
    "amber", "azure", "bold", "brave", "bright", "calm", "crisp", "deft", "eager", "fleet",
    "fresh", "glad", "keen", "kind", "lush", "merry", "nimble", "proud", "quick", "quiet", "rapid",
    "sage", "sharp", "solar", "steady", "swift", "teal", "warm", "wise", "zesty",
];

const NOUNS: &[&str] = &[
    "acorn", "beacon", "bloom", "brook", "cedar", "crest", "dawn", "delta", "dune", "echo", "fern",
    "fjord", "flint", "forge", "frost", "glade", "grove", "haven", "ledge", "mesa", "mist", "peak",
    "pine", "prism", "reef", "ridge", "spark", "spire", "tide", "vale",
];

/// A random `adjective-noun` display name for a new worktree (e.g. `swift-pine`).
/// Collisions are harmless (names are display-only), so cheap time entropy is fine.
pub fn random_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize)
        .unwrap_or(0);
    let adj = ADJECTIVES[seed % ADJECTIVES.len()];
    let noun = NOUNS[(seed / ADJECTIVES.len()) % NOUNS.len()];
    format!("{adj}-{noun}")
}

/// A filesystem-safe slug for a repo/project name.
pub fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-');
    if s.is_empty() {
        "repo".to_string()
    } else {
        s.to_string()
    }
}

/// The branch name muxel creates for an instance's worktree.
pub fn branch_name(instance: Uuid) -> String {
    format!("muxel/{}", &instance.simple().to_string()[..8])
}

/// Directory name for an instance's worktree, e.g. `myrepo_1a2b3c4d`.
pub fn dir_name(repo_name: &str, instance: Uuid) -> String {
    format!(
        "{}_{}",
        slug(repo_name),
        &instance.simple().to_string()[..8]
    )
}

/// Full worktree path: `<base>/<repo-slug>_<id8>`.
pub fn worktree_path(base: &Path, repo_name: &str, instance: Uuid) -> PathBuf {
    base.join(dir_name(repo_name, instance))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitizes() {
        assert_eq!(slug("My Repo!"), "My-Repo");
        assert_eq!(slug("///"), "repo");
    }

    #[test]
    fn branch_and_dir_use_id_prefix() {
        let id = Uuid::nil();
        assert_eq!(branch_name(id), "muxel/00000000");
        assert_eq!(dir_name("My Repo", id), "My-Repo_00000000");
    }

    #[test]
    fn worktree_path_joins_base() {
        let p = worktree_path(Path::new("/data/worktrees"), "repo", Uuid::nil());
        assert_eq!(p, PathBuf::from("/data/worktrees/repo_00000000"));
    }

    #[test]
    fn random_name_is_adjective_hyphen_noun() {
        for _ in 0..20 {
            let name = random_name();
            let (adj, noun) = name.split_once('-').expect("adj-noun");
            assert!(ADJECTIVES.contains(&adj), "unknown adjective: {adj}");
            assert!(NOUNS.contains(&noun), "unknown noun: {noun}");
        }
    }
}
