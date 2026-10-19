//! Bounded lookups in Codex's dated rollout store.

use super::{codex_session_meta, is_codex_thread_id_hint, paths_loosely_equal};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, Timelike, Utc};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexSession {
    pub id: String,
    pub path: PathBuf,
}

/// A failed bounded lookup may be reused while its day directories are unchanged.
/// Capture/check these stamps on the background executor, never in a UI update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexHintMiss {
    directories: Vec<(PathBuf, Option<SystemTime>)>,
    scanned_at: Instant,
}

impl CodexHintMiss {
    /// Capture before enumerating so a concurrent file creation forces a retry.
    pub fn capture(home: &Path, hint: &str, now: Instant) -> Self {
        Self {
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
        self.directories == current.directories
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
) -> io::Result<Option<PathBuf>> {
    let Ok(id) = Uuid::parse_str(id) else {
        return Ok(None);
    };
    if let Some(path) = saved {
        if !path.starts_with(home.join(".codex/sessions"))
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || filename_id(path) != Some(id)
        {
            return Ok(None);
        }
        return match stat(path) {
            Ok(metadata) => Ok(metadata.is_file().then(|| path.to_path_buf())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let sibling = match path.extension().and_then(|extension| extension.to_str()) {
                    Some("jsonl") => path.with_extension("jsonl.zst"),
                    Some("zst") => path.with_extension(""),
                    _ => return Ok(None),
                };
                match stat(&sibling) {
                    Ok(metadata) => Ok(metadata.is_file().then_some(sibling)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                }
            }
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
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

#[derive(Default)]
pub struct CodexSessionNames {
    path: Option<PathBuf>,
    stamp: Option<(u64, Option<SystemTime>, Option<SystemTime>)>,
    prefix: Vec<u8>,
    tail: Vec<u8>,
    file: Option<std::fs::File>,
    offset: u64,
    pending: Vec<u8>,
    names: HashMap<String, String>,
}

impl CodexSessionNames {
    pub(super) fn into_names(self) -> HashMap<String, String> {
        self.names
    }

    pub fn names(&self) -> &HashMap<String, String> {
        &self.names
    }

    /// Read only a changed suffix. Retain a partial final row until its newline
    /// arrives. Check a bounded prefix before continuing after growth so rotation
    /// or truncate-and-regrow cannot retain names from a different file prefix.
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
        let stamp = (
            metadata.len(),
            metadata.modified().ok(),
            metadata.created().ok(),
        );
        if self.path.as_ref() == Some(&path) && self.stamp == Some(stamp) {
            return Ok(false);
        }
        #[cfg(test)]
        super::codex_lookup_tests::record_open(&path);
        let mut file = std::fs::File::open(&path)?;
        let metadata = file.metadata()?;
        let stamp = (
            metadata.len(),
            metadata.modified().ok(),
            metadata.created().ok(),
        );
        let mut prefix = vec![0; metadata.len().min(256) as usize];
        file.read_exact(&mut prefix)?;
        #[cfg(test)]
        super::codex_lookup_tests::record_read(0, prefix.len());
        // A retained handle follows the old file through rotation. Compare its
        // current metadata, which also follows ordinary appends, with the path.
        let replaced = self.file.as_ref().is_some_and(|old| match old.metadata() {
            Ok(old) => (old.len(), old.modified().ok(), old.created().ok()) != stamp,
            Err(_) => true,
        });
        let mut old_tail = vec![0; self.tail.len()];
        let tail_matches = if self.offset <= metadata.len() && !old_tail.is_empty() {
            file.seek(SeekFrom::Start(self.offset - old_tail.len() as u64))?;
            file.read_exact(&mut old_tail)?;
            #[cfg(test)]
            super::codex_lookup_tests::record_read(
                self.offset - old_tail.len() as u64,
                old_tail.len(),
            );
            old_tail == self.tail
        } else {
            self.tail.is_empty()
        };
        let reset = replaced
            || !tail_matches
            || self.path.as_ref() != Some(&path)
            || metadata.len() < self.offset
            || self
                .stamp
                .is_some_and(|old| old.2 != stamp.2 || (old.0 == stamp.0 && old.1 != stamp.1))
            || !prefix.starts_with(&self.prefix);
        let offset = if reset { 0 } else { self.offset };
        file.seek(SeekFrom::Start(offset))?;
        let mut appended = Vec::new();
        (&mut file)
            .take(metadata.len().saturating_sub(offset))
            .read_to_end(&mut appended)?;
        #[cfg(test)]
        super::codex_lookup_tests::record_read(offset, appended.len());
        // Finish all fallible reads before changing the cached names.
        let tail_len = metadata.len().min(256) as usize;
        let mut tail = vec![0; tail_len];
        file.seek(SeekFrom::Start(metadata.len() - tail_len as u64))?;
        file.read_exact(&mut tail)?;
        #[cfg(test)]
        super::codex_lookup_tests::record_read(metadata.len() - tail_len as u64, tail_len);
        let mut changed = reset && !self.names.is_empty();
        if reset {
            self.names.clear();
            self.pending.clear();
        }
        self.pending.extend_from_slice(&appended);
        let mut consumed = 0;
        for (index, byte) in self.pending.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if let Ok(value) =
                serde_json::from_slice::<serde_json::Value>(&self.pending[consumed..index])
                && let Some(id) = value.get("id").and_then(serde_json::Value::as_str)
                && let Some(name) = value
                    .get("thread_name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                && self.names.get(id).map(String::as_str) != Some(name)
            {
                self.names.insert(id.to_string(), name.to_string());
                changed = true;
            }
            consumed = index + 1;
        }
        self.pending.drain(..consumed);
        self.offset = offset + appended.len() as u64;
        self.path = Some(path);
        self.stamp = Some(stamp);
        self.prefix = prefix;
        self.tail = tail;
        self.file = Some(file);
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
