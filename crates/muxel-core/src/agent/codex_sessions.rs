//! Bounded lookups in Codex's dated rollout store.

use super::{codex_session_meta, is_codex_thread_id_hint, paths_loosely_equal};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, Timelike, Utc};
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexSession {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexResumePath {
    Found(PathBuf),
    /// Neither the exact saved path nor its compression sibling is a usable file.
    Missing,
    /// A dated search or untrusted path cannot establish that the rollout is gone.
    Unknown,
}

/// A failed bounded lookup may be reused while its day directories are unchanged.
/// Capture/check these stamps on the background executor, never in a UI update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexHintMiss {
    hint: String,
    directories: Vec<(PathBuf, Option<SystemTime>)>,
    scanned_at: Instant,
}

impl CodexHintMiss {
    /// Capture before enumerating so a concurrent file creation forces a retry.
    pub fn capture(home: &Path, hint: &str, now: Instant) -> Self {
        Self {
            hint: hint.to_string(),
            directories: directories(home, hint)
                .into_iter()
                .map(|path| {
                    let modified = stat(&path)
                        .ok()
                        .and_then(|metadata| metadata.modified().ok());
                    (path, modified)
                })
                .collect(),
            scanned_at: now,
        }
    }

    pub fn reusable_for(&self, current: &Self) -> bool {
        self.hint == current.hint
            && self.directories == current.directories
            && current
                .scanned_at
                .saturating_duration_since(self.scanned_at)
                < Duration::from_secs(30)
    }
}

fn creation_time(hint: &str) -> Option<DateTime<Utc>> {
    if !is_codex_thread_id_hint(hint) {
        return None;
    }
    let value = hint
        .strip_suffix("...")
        .map(|prefix| format!("{prefix}0000000"))
        .unwrap_or_else(|| hint.to_string());
    let uuid = Uuid::parse_str(&value).ok()?;
    if uuid.get_version_num() != 7 {
        return None;
    }
    let mut timestamp = [0; 8];
    timestamp[2..].copy_from_slice(&uuid.as_bytes()[..6]);
    DateTime::from_timestamp_millis(i64::from_be_bytes(timestamp))
}

fn dates(local: DateTime<FixedOffset>) -> Vec<NaiveDate> {
    let day = local.date_naive();
    let seconds = local.time().num_seconds_from_midnight();
    let neighbor = if seconds < 300 {
        day.pred_opt()
    } else if seconds >= 86_100 {
        day.succ_opt()
    } else {
        let utc_day = local.with_timezone(&Utc).date_naive();
        (utc_day != day).then_some(utc_day)
    };
    std::iter::once(day).chain(neighbor).collect()
}

pub(super) fn directories(home: &Path, hint: &str) -> Vec<PathBuf> {
    let Some(created) = creation_time(hint) else {
        return Vec::new();
    };
    let local = created.with_timezone(&Local).fixed_offset();
    let root = home.join(".codex/sessions");
    dates(local)
        .into_iter()
        .map(|day| root.join(day.format("%Y/%m/%d").to_string()))
        .collect()
}

fn list(path: &Path) -> io::Result<std::fs::ReadDir> {
    #[cfg(test)]
    super::codex_lookup_tests::record_directory(path);
    std::fs::read_dir(path)
}

fn stat(path: &Path) -> io::Result<std::fs::Metadata> {
    #[cfg(test)]
    super::codex_lookup_tests::record_stat(path);
    std::fs::metadata(path)
}

fn filename_uuid(name: &std::ffi::OsStr) -> Option<&str> {
    let name = name.to_str()?;
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    let start = stem.len().checked_sub(36)?;
    if stem.get(start.checked_sub(1)?..start)? != "-" {
        return None;
    }
    stem.get(start..)
}

fn filename_id(path: &Path) -> Option<Uuid> {
    Uuid::parse_str(filename_uuid(path.file_name()?)?).ok()
}

pub fn codex_session_from_hint(home: &Path, hint: &str, cwd: &Path) -> Option<CodexSession> {
    let prefix = hint
        .strip_suffix("...")
        .unwrap_or(hint)
        .to_ascii_lowercase();
    let mut matched: Option<CodexSession> = None;
    for directory in directories(home, hint) {
        let Ok(entries) = list(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(file_uuid) = filename_uuid(&name) else {
                continue;
            };
            if !file_uuid
                .get(..prefix.len())
                .is_some_and(|value| value.eq_ignore_ascii_case(&prefix))
            {
                continue;
            }
            let Ok(file_id) = Uuid::parse_str(file_uuid) else {
                continue;
            };
            let path = entry.path();
            let Some((id, recorded_cwd)) = codex_session_meta(&path) else {
                continue;
            };
            if Uuid::parse_str(&id).ok() != Some(file_id)
                || !paths_loosely_equal(Path::new(&recorded_cwd), cwd)
            {
                continue;
            }
            let id = file_id.to_string();
            if matched.as_ref().is_some_and(|session| session.id != id) {
                return None;
            }
            matched = Some(CodexSession { id, path });
        }
    }
    matched
}

/// A stored path takes one stat in the normal case. A rollout compressed since
/// capture may have its exact `.zst` sibling instead; no directory scan is needed.
/// Archived sessions are deliberately excluded: Codex requires unarchive first.
pub fn codex_resume_path(
    home: &Path,
    id: &str,
    saved: Option<&Path>,
) -> io::Result<CodexResumePath> {
    let Ok(id) = Uuid::parse_str(id) else {
        return Ok(CodexResumePath::Unknown);
    };
    if let Some(path) = saved {
        if !path.starts_with(home.join(".codex/sessions"))
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || filename_id(path) != Some(id)
        {
            return Ok(CodexResumePath::Unknown);
        }
        match stat(path) {
            Ok(metadata) if metadata.is_file() => {
                return Ok(CodexResumePath::Found(path.to_path_buf()));
            }
            // A directory at the saved filename does not rule out a valid
            // compression sibling. Both exact paths must be checked.
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let sibling = match path.extension().and_then(|extension| extension.to_str()) {
            Some("jsonl") => path.with_extension("jsonl.zst"),
            Some("zst") => path.with_extension(""),
            _ => return Ok(CodexResumePath::Unknown),
        };
        return match stat(&sibling) {
            Ok(metadata) if metadata.is_file() => Ok(CodexResumePath::Found(sibling)),
            Ok(_) => Ok(CodexResumePath::Missing),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CodexResumePath::Missing),
            Err(error) => Err(error),
        };
    }
    let id = id.to_string();
    for directory in directories(home, &id) {
        let entries = match list(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            if filename_uuid(&entry.file_name())
                .is_some_and(|value| value.eq_ignore_ascii_case(&id))
            {
                let path = entry.path();
                if !stat(&path)?.is_file() {
                    continue;
                }
                return Ok(CodexResumePath::Found(path));
            }
        }
    }
    Ok(CodexResumePath::Unknown)
}

#[derive(Default)]
pub struct CodexSessionNames {
    path: Option<PathBuf>,
    stamp: Option<(u64, Option<SystemTime>)>,
    names: HashMap<String, String>,
}

impl CodexSessionNames {
    pub(super) fn into_names(self) -> HashMap<String, String> {
        self.names
    }

    pub fn names(&self) -> &HashMap<String, String> {
        &self.names
    }

    /// Unchanged indexes take one stat. Reread a changed file in full on the
    /// worker, including earlier rows that may have been rewritten in place.
    /// Ignore an incomplete final row until its newline arrives.
    pub fn refresh(&mut self, home: &Path) -> io::Result<bool> {
        let path = home.join(".codex/session_index.jsonl");
        let metadata = match stat(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *self = Self::default();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let stamp = (metadata.len(), metadata.modified().ok());
        if self.path.as_ref() == Some(&path) && self.stamp == Some(stamp) {
            return Ok(false);
        }
        #[cfg(test)]
        super::codex_lookup_tests::record_open(&path);
        let file = std::fs::File::open(&path)?;
        let metadata = file.metadata()?;
        let stamp = (metadata.len(), metadata.modified().ok());
        let mut bytes = Vec::new();
        file.take(metadata.len()).read_to_end(&mut bytes)?;
        #[cfg(test)]
        super::codex_lookup_tests::record_read(0, bytes.len());
        let mut names = HashMap::new();
        for row in bytes.split_inclusive(|byte| *byte == b'\n') {
            if row.last() != Some(&b'\n') {
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(row)
                && let Some(id) = value.get("id").and_then(serde_json::Value::as_str)
                && let Some(name) = value
                    .get("thread_name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
            {
                names.insert(id.to_string(), name.to_string());
            }
        }
        // Retain the last good map if opening or reading the changed file fails.
        let changed = self.names != names;
        self.names = names;
        self.path = Some(path);
        self.stamp = Some(stamp);
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::dates;
    use chrono::DateTime;

    #[test]
    fn day_lookup_has_one_day_normally_and_one_neighbor_at_boundaries() {
        let cases = [
            (
                "2026-10-20T01:56:59+13:00",
                vec!["2026-10-20", "2026-10-19"],
            ),
            ("2026-10-19T05:56:59-07:00", vec!["2026-10-19"]),
            (
                "2026-10-19T00:02:00-07:00",
                vec!["2026-10-19", "2026-10-18"],
            ),
            (
                "2026-10-19T23:58:00-07:00",
                vec!["2026-10-19", "2026-10-20"],
            ),
            (
                "2026-10-19T18:00:00-07:00",
                vec!["2026-10-19", "2026-10-20"],
            ),
            (
                "2026-10-19T06:00:00+09:00",
                vec!["2026-10-19", "2026-10-18"],
            ),
        ];
        for (timestamp, expected) in cases {
            let days: Vec<_> = dates(DateTime::parse_from_rfc3339(timestamp).unwrap())
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(days, expected);
        }
    }
}
