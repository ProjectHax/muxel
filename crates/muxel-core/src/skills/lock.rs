//! Records of where installed skills came from.
//!
//! - The `skills` CLI's global lock (`~/.agents/.skill-lock.json`, version 3),
//!   which muxel reads and writes so skills installed by either tool show up and
//!   update in both. Fields muxel doesn't know are kept as they are.
//! - muxel's sidecar ([`super::SIDECAR_FILE`]) inside a skill's canonical
//!   folder, holding the same entry — for project and partial installs, which
//!   the global lock doesn't describe.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The lock version muxel reads and writes.
pub const LOCK_VERSION: u64 = 3;

/// One installed skill's source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockEntry {
    /// `owner/repo` for GitHub/GitLab, else the URL.
    pub source: String,
    /// `github`, `gitlab`, `git`, … (the CLI has others; kept as text).
    pub source_type: String,
    pub source_url: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The skill's `SKILL.md` inside the repository.
    #[serde(default)]
    pub skill_path: String,
    /// For git sources, the git tree SHA of the skill's folder.
    #[serde(default)]
    pub skill_folder_hash: String,
    #[serde(default)]
    pub installed_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl LockEntry {
    /// The folder inside the repository that holds the skill.
    pub fn skill_dir(&self) -> String {
        let p = self.skill_path.trim_matches('/');
        match p.strip_suffix(super::SKILL_FILE) {
            Some(dir) => dir.trim_end_matches('/').to_string(),
            None => p.to_string(),
        }
    }
}

/// The whole lock file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillLock {
    pub version: u64,
    #[serde(default)]
    pub skills: BTreeMap<String, LockEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for SkillLock {
    fn default() -> Self {
        SkillLock {
            version: LOCK_VERSION,
            skills: BTreeMap::new(),
            extra: Map::from_iter([("dismissed".to_string(), Value::Object(Map::new()))]),
        }
    }
}

/// Why an existing lock can't be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockError {
    /// Not valid JSON (or not the expected shape). muxel never overwrites it.
    Malformed,
    /// Written by a newer version of the `skills` CLI.
    Newer(u64),
}

/// Parse a lock file's text. A missing file (`None`) and an older version both
/// mean a fresh lock, as the CLI treats them.
pub fn parse_lock(text: Option<&str>) -> Result<SkillLock, LockError> {
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return Ok(SkillLock::default());
    };
    let value: Value = serde_json::from_str(text).map_err(|_| LockError::Malformed)?;
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version < LOCK_VERSION {
        return Ok(SkillLock::default());
    }
    if version > LOCK_VERSION {
        return Err(LockError::Newer(version));
    }
    serde_json::from_value(value).map_err(|_| LockError::Malformed)
}

/// The lock as the CLI writes it: two-space JSON plus a newline.
pub fn write_lock(lock: &SkillLock) -> String {
    let mut s = serde_json::to_string_pretty(lock).unwrap_or_default();
    s.push('\n');
    s
}

/// Add or replace an entry, keeping the original `installedAt`.
pub fn upsert(lock: &mut SkillLock, name: &str, mut entry: LockEntry) {
    if let Some(old) = lock.skills.get(name)
        && !old.installed_at.is_empty()
    {
        entry.installed_at = old.installed_at.clone();
    }
    lock.skills.insert(name.to_string(), entry);
}

/// The sidecar's text.
pub fn write_sidecar(entry: &LockEntry) -> String {
    let mut s = serde_json::to_string_pretty(entry).unwrap_or_default();
    s.push('\n');
    s
}

/// A sidecar's entry; `None` when unreadable.
pub fn parse_sidecar(text: &str) -> Option<LockEntry> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        LockEntry, LockError, parse_lock, parse_sidecar, upsert, write_lock, write_sidecar,
    };

    const REAL: &str = r#"{
  "version": 3,
  "skills": {
    "security-audit": {
      "source": "cloudflare/security-audit-skill",
      "sourceType": "github",
      "sourceUrl": "https://github.com/cloudflare/security-audit-skill.git",
      "skillPath": "skills/security-audit/SKILL.md",
      "skillFolderHash": "ccbc33ed119b28c3ef7b44a7e2081ab191ad63af",
      "installedAt": "2026-10-24T13:18:49.248Z",
      "updatedAt": "2026-11-12T04:16:21.494Z",
      "pluginName": "x"
    }
  },
  "dismissed": {},
  "lastSelectedAgents": ["claude-code"]
}"#;

    #[test]
    fn round_trips_and_keeps_unknown_fields() {
        let lock = parse_lock(Some(REAL)).unwrap();
        let e = &lock.skills["security-audit"];
        assert_eq!(e.source, "cloudflare/security-audit-skill");
        assert_eq!(e.skill_dir(), "skills/security-audit");
        assert_eq!(e.extra["pluginName"], "x");
        let again = parse_lock(Some(&write_lock(&lock))).unwrap();
        assert_eq!(again, lock);
        assert!(write_lock(&lock).contains("\"lastSelectedAgents\""));
    }

    #[test]
    fn missing_old_and_bad_locks() {
        assert!(parse_lock(None).unwrap().skills.is_empty());
        assert!(
            parse_lock(Some(r#"{"version":2,"skills":{"x":{}}}"#))
                .unwrap()
                .skills
                .is_empty()
        );
        assert_eq!(parse_lock(Some("{")), Err(LockError::Malformed));
        assert_eq!(
            parse_lock(Some(r#"{"version":4}"#)),
            Err(LockError::Newer(4))
        );
        let fresh = write_lock(&parse_lock(None).unwrap());
        assert!(fresh.contains("\"version\": 3"));
        assert!(fresh.contains("\"dismissed\": {}"));
    }

    #[test]
    fn upsert_keeps_installed_at() {
        let mut lock = parse_lock(Some(REAL)).unwrap();
        upsert(
            &mut lock,
            "security-audit",
            LockEntry {
                installed_at: "new".into(),
                updated_at: "now".into(),
                ..Default::default()
            },
        );
        let e = &lock.skills["security-audit"];
        assert_eq!(e.installed_at, "2026-10-24T13:18:49.248Z");
        assert_eq!(e.updated_at, "now");
    }

    #[test]
    fn sidecar_round_trip_and_root_skill_dir() {
        let e = LockEntry {
            source: "o/r".into(),
            skill_path: "SKILL.md".into(),
            git_ref: Some("dev".into()),
            ..Default::default()
        };
        let back = parse_sidecar(&write_sidecar(&e)).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.skill_dir(), "");
        assert!(write_sidecar(&e).contains("\"ref\": \"dev\""));
        assert_eq!(parse_sidecar("nope"), None);
    }
}
