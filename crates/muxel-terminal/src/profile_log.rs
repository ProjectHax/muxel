//! Bounded dated history shared by the opt-in profilers. Call only on log workers.
//!
//! Each live path has one writer. Archives append `.YYYY-MM-DD.NNNNNN` to that
//! path; the date belongs to the data, not the instant it was archived.

use chrono::{DateTime, Datelike, Local, NaiveDate};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub struct ProfileLogLimits {
    pub file_bytes: u64,
    pub keep_days: u64,
    pub total_bytes: u64,
}

impl Default for ProfileLogLimits {
    fn default() -> Self {
        Self {
            file_bytes: 2 * 1024 * 1024,
            keep_days: 14,
            total_bytes: 256 * 1024 * 1024,
        }
    }
}

impl ProfileLogLimits {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let positive = |name: &str, fallback| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|&value| value > 0)
                .unwrap_or(fallback)
        };
        Self {
            keep_days: positive("MUXEL_PROFILE_KEEP_DAYS", defaults.keep_days),
            total_bytes: positive("MUXEL_PROFILE_MAX_BYTES", defaults.total_bytes)
                .max(defaults.file_bytes),
            ..defaults
        }
    }
}

pub struct ProfileLog {
    path: PathBuf,
    limits: ProfileLogLimits,
    file: Option<File>,
    live_day: Option<NaiveDate>,
    bytes: u64,
    pruned_on: Option<NaiveDate>,
    /// Whether this log has named a failure on stderr yet.
    reported: bool,
}

impl ProfileLog {
    pub fn new(path: PathBuf, limits: ProfileLogLimits) -> Self {
        let metadata = fs::metadata(&path).ok();
        Self {
            live_day: metadata
                .as_ref()
                .filter(|meta| meta.is_file())
                .and_then(|_| first_record_day(&path))
                .or_else(|| {
                    metadata
                        .as_ref()
                        .and_then(|meta| meta.modified().ok())
                        .map(|at| DateTime::<Local>::from(at).date_naive())
                }),
            bytes: metadata.map_or(0, |meta| meta.len()),
            path,
            limits,
            file: None,
            pruned_on: None,
            reported: false,
        }
    }

    /// Append one record. The log's first append or prune failure is named
    /// on stderr; later ones aren't, so a lasting fault shows once rather
    /// than on every record.
    pub fn append(&mut self, line: &str) -> io::Result<()> {
        let result = self.append_dated(Local::now().date_naive(), line);
        if let Err(error) = &result {
            self.report("append", error);
        }
        result
    }

    fn report(&mut self, what: &str, error: &io::Error) {
        if !self.reported {
            self.reported = true;
            eprintln!(
                "profile log {}: {what} failed: {error} (later failures are not reported)",
                self.path.display()
            );
        }
    }

    fn append_dated(&mut self, today: NaiveDate, line: &str) -> io::Result<()> {
        let day = record_day(line.as_bytes()).unwrap_or(today);
        self.append_on(day, today, line, |from, to| fs::rename(from, to))
    }

    fn append_on(
        &mut self,
        day: NaiveDate,
        today: NaiveDate,
        line: &str,
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.path.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile path must name a file",
            ));
        }
        if self.file.is_none() {
            match fs::metadata(&self.path) {
                Ok(meta) if !meta.is_file() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "profile path is not a file",
                    ));
                }
                Ok(meta) => self.bytes = meta.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.bytes = 0;
                    self.live_day = None;
                    self.pruned_on = None;
                }
                Err(error) => return Err(error),
            }
        }
        let length = line.len() as u64 + 1;
        if length > self.limits.file_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile record exceeds cap",
            ));
        }
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        if self.bytes > 0
            && (self.live_day != Some(day)
                || self.bytes.saturating_add(length) > self.limits.file_bytes)
        {
            // Drop the live handle before rename (including on Windows).
            self.file = None;
            let archive = self.archive_path(self.live_day.unwrap_or(day))?;
            if rename(&self.path, &archive).is_err() {
                // A tailer without delete-sharing must not cause unbounded growth.
                // If truncation also fails, propagate the error and skip the write.
                OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&self.path)?;
            }
            self.bytes = 0;
            self.pruned_on = None;
        }
        if self.pruned_on != Some(today) {
            // An archive that can't be removed (read-only, or held open
            // without delete sharing) doesn't stop the log while the family
            // still fits its budget; pruning tries it again at the next roll
            // or date. Over budget, skip the write rather than grow.
            if let Some(error) = self.prune(today)? {
                self.report("prune", &error);
            }
            self.pruned_on = Some(today);
        }
        self.live_day = Some(day);
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        let result = (|| {
            let file = self.file.as_mut().expect("opened profile log");
            writeln!(file, "{line}")?;
            file.flush()
        })();
        // Re-read length after an I/O error, including a partial write.
        let metadata = fs::metadata(&self.path);
        if result.is_err() || metadata.is_err() {
            self.file = None;
        }
        self.bytes = metadata?.len();
        result
    }

    fn parent(&self) -> &Path {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    }

    fn family(&self) -> String {
        format!(
            "{}.",
            self.path.file_name().unwrap_or_default().to_string_lossy()
        )
    }

    fn archive_path(&self, day: NaiveDate) -> io::Result<PathBuf> {
        let prefix = format!("{}{day}.", self.family());
        let mut sequence = 0_u64;
        for entry in fs::read_dir(self.parent())? {
            let name = entry?.file_name();
            if let Some(suffix) = name.to_string_lossy().strip_prefix(&prefix)
                && let Ok(number) = suffix.parse::<u64>()
            {
                sequence = sequence.max(number);
            }
        }
        let sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("profile archive sequence exhausted"))?;
        Ok(self.parent().join(format!("{prefix}{sequence:06}")))
    }

    /// Remove expired archives, then the oldest until the family fits its
    /// budget. A file that can't be removed is skipped and the rest are still
    /// pruned. `Ok(Some(error))` names the first such failure when the family
    /// fits anyway; `Err` means it doesn't (or the directory can't be read),
    /// and the caller must not write.
    fn prune(&self, today: NaiveDate) -> io::Result<Option<io::Error>> {
        let prefix = self.family();
        let mut first_error = None;
        let mut remove = |path: &Path| match fs::remove_file(path) {
            Ok(()) => true,
            Err(error) => {
                first_error.get_or_insert(error);
                false
            }
        };
        let mut archives = Vec::new();
        for entry in fs::read_dir(self.parent())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(suffix) = name.strip_prefix(&prefix) else {
                continue;
            };
            let parsed = suffix.split_once('.').and_then(|(date, sequence)| {
                let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
                let sequence = sequence.parse::<u64>().ok()?;
                Some((date, sequence))
            });
            // Count the previous format's one backup too, so upgrades stay bounded.
            let (day, sequence) = match parsed {
                Some(value) => value,
                None if suffix == "1" => (
                    DateTime::<Local>::from(entry.metadata()?.modified()?).date_naive(),
                    0,
                ),
                None => continue,
            };
            if u64::try_from(today.signed_duration_since(day).num_days())
                .is_ok_and(|age| age >= self.limits.keep_days)
            {
                remove(&entry.path());
            } else {
                archives.push((day, sequence, entry.path(), entry.metadata()?.len()));
            }
        }
        archives.sort_unstable_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        let mut total = archives.iter().map(|entry| entry.3).sum::<u64>();
        // Reserve a full live file, avoiding a directory walk for every line.
        let budget = self
            .limits
            .total_bytes
            .saturating_sub(self.limits.file_bytes);
        for (_, _, path, bytes) in archives {
            if total <= budget {
                break;
            }
            if remove(&path) {
                total = total.saturating_sub(bytes);
            }
        }
        if total > budget {
            return Err(first_error
                .unwrap_or_else(|| io::Error::other("profile history over its byte budget")));
        }
        Ok(first_error)
    }
}

fn record_day(bytes: &[u8]) -> Option<NaiveDate> {
    let end = bytes.iter().position(|&byte| byte == b']')?;
    let prefix = &bytes[..end];
    let prefix = std::str::from_utf8(prefix).ok()?.strip_prefix('[')?;
    let (seconds, millis) = prefix.split_once('.')?;
    if millis.len() != 3 || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let at = DateTime::from_timestamp(seconds.parse().ok()?, 0)?;
    // Profiler prefixes are nonnegative Unix timestamps. Reject malformed
    // extreme years before asking the OS to convert them to local time.
    if !(1970..=9999).contains(&at.year()) {
        return None;
    }
    Some(at.with_timezone(&Local).date_naive())
}

fn first_record_day(path: &Path) -> Option<NaiveDate> {
    let mut prefix = [0; 64];
    let length = File::open(path).ok()?.read(&mut prefix).ok()?;
    record_day(&prefix[..length])
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("muxel-profile-history-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn log(&self, limits: ProfileLogLimits) -> ProfileLog {
            ProfileLog::new(self.0.join("term.log"), limits)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, n).unwrap()
    }
    fn append(log: &mut ProfileLog, date: NaiveDate, line: &str) {
        log.append_on(date, date, line, |from, to| fs::rename(from, to))
            .unwrap();
    }
    fn limits() -> ProfileLogLimits {
        ProfileLogLimits {
            file_bytes: 8,
            keep_days: 14,
            total_bytes: 128,
        }
    }

    #[test]
    fn size_rolls_preserve_multiple_segments_and_keep_live_path() {
        let fixture = Fixture::new();
        let mut log = fixture.log(limits());
        for line in ["1234567", "abcdefg", "current"] {
            append(&mut log, day(1), line);
        }
        assert_eq!(
            fs::read_to_string(fixture.0.join("term.log.2026-11-01.000001")).unwrap(),
            "1234567\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.0.join("term.log.2026-11-01.000002")).unwrap(),
            "abcdefg\n"
        );
        assert_eq!(fs::read_to_string(&log.path).unwrap(), "current\n");
    }

    #[test]
    fn local_date_rolls_before_size_cap_and_uses_previous_day() {
        let fixture = Fixture::new();
        let mut log = fixture.log(limits());
        append(&mut log, day(1), "old");
        append(&mut log, day(2), "new");
        assert_eq!(
            fs::read_to_string(fixture.0.join("term.log.2026-11-01.000001")).unwrap(),
            "old\n"
        );
        assert_eq!(fs::read_to_string(&log.path).unwrap(), "new\n");
    }

    #[test]
    fn restart_uses_live_modification_date_and_prunes_legacy_backup() {
        let fixture = Fixture::new();
        let modified = day(1)
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .unwrap();
        for name in ["term.log", "term.log.1"] {
            let path = fixture.0.join(name);
            fs::write(&path, "old\n").unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified.into()))
                .unwrap();
        }
        let mut log = fixture.log(limits());
        append(&mut log, day(15), "new");
        assert!(!fixture.0.join("term.log.1").exists());
        assert!(!fixture.0.join("term.log.2026-11-01.000001").exists());
        assert_eq!(fs::read_to_string(&log.path).unwrap(), "new\n");
    }

    #[test]
    fn deferred_record_date_survives_a_later_write_time_and_restart() {
        let fixture = Fixture::new();
        let at = |n| {
            day(n)
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .single()
                .unwrap()
        };
        let path = fixture.0.join("term.log");
        let limits = ProfileLogLimits {
            file_bytes: 128,
            total_bytes: 512,
            ..limits()
        };
        let mut log = fixture.log(limits);
        log.append_dated(day(2), &format!("[{}.000] old", at(1).timestamp()))
            .unwrap();
        log.append_dated(day(2), &format!("[{}.000] new", at(2).timestamp()))
            .unwrap();
        let archive = fixture.0.join("term.log.2026-11-01.000001");
        assert!(
            archive.exists(),
            "the archived date must follow the record, not its later modification time"
        );
        assert!(fs::read_to_string(&path).unwrap().ends_with("new\n"));
        drop(log);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(at(3).into()))
            .unwrap();
        let mut restarted = fixture.log(limits);
        restarted
            .append_dated(day(3), &format!("[{}.000] next", at(3).timestamp()))
            .unwrap();
        let archive = fixture.0.join("term.log.2026-11-02.000001");
        assert!(fs::read_to_string(archive).unwrap().ends_with("new\n"));
        assert!(fs::read_to_string(path).unwrap().ends_with("next\n"));
    }

    #[test]
    fn age_prunes_only_this_family_and_keeps_fourteen_local_days() {
        let fixture = Fixture::new();
        for name in [
            "term.log.2026-11-01.000001",
            "term.log.2026-11-02.000001",
            "ui.log.2026-11-01.000001",
            "term.log.notes",
        ] {
            fs::write(fixture.0.join(name), "old").unwrap();
        }
        let mut log = fixture.log(limits());
        append(&mut log, day(15), "new");
        assert!(!fixture.0.join("term.log.2026-11-01.000001").exists());
        for name in [
            "term.log.2026-11-02.000001",
            "ui.log.2026-11-01.000001",
            "term.log.notes",
        ] {
            assert!(fixture.0.join(name).exists(), "{name}");
        }
    }

    #[test]
    fn total_budget_removes_oldest_segments_and_reserves_live_capacity() {
        let fixture = Fixture::new();
        let mut log = fixture.log(ProfileLogLimits {
            total_bytes: 24,
            ..limits()
        });
        for line in ["1111111", "2222222", "3333333", "4444444"] {
            append(&mut log, day(1), line);
        }
        assert!(!fixture.0.join("term.log.2026-11-01.000001").exists());
        assert_eq!(
            fs::read_dir(&fixture.0)
                .unwrap()
                .map(|entry| entry.unwrap().metadata().unwrap().len())
                .sum::<u64>(),
            24
        );
    }

    #[test]
    fn rename_failure_truncates_instead_of_growing_unbounded() {
        let fixture = Fixture::new();
        let mut log = fixture.log(limits());
        append(&mut log, day(1), "1234567");
        log.append_on(day(1), day(1), "next", |_, _| {
            Err(io::ErrorKind::PermissionDenied.into())
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&log.path).unwrap(), "next\n");
        assert!(log.bytes <= limits().file_bytes);
    }

    #[test]
    fn failed_rename_and_truncation_refuse_the_record() {
        let fixture = Fixture::new();
        let mut log = fixture.log(limits());
        append(&mut log, day(1), "1234567");
        assert!(
            log.append_on(day(1), day(1), "next", |from, _| {
                fs::remove_file(from)?;
                fs::create_dir(from)?;
                Err(io::ErrorKind::PermissionDenied.into())
            })
            .is_err()
        );
        assert!(log.file.is_none());
        assert!(log.path.is_dir());
        assert!(fs::read_dir(&log.path).unwrap().next().is_none());
    }

    /// A reader holding an expired archive open without delete sharing (the
    /// usual tailer on Windows) must not stop the log: the record is written,
    /// the other expired archive is still pruned, and the held one goes on a
    /// later prune once it's released. Unix can't make one file undeletable
    /// without locking the whole directory, so this runs on Windows.
    #[cfg(windows)]
    #[test]
    fn held_archive_does_not_block_appends_or_other_pruning() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let held = fixture.0.join("term.log.2026-11-01.000001");
        let expired = fixture.0.join("term.log.2026-11-01.000002");
        fs::write(&held, "old").unwrap();
        fs::write(&expired, "old").unwrap();
        // FILE_SHARE_READ | FILE_SHARE_WRITE, without FILE_SHARE_DELETE.
        let reader = File::options()
            .read(true)
            .share_mode(1 | 2)
            .open(&held)
            .unwrap();
        let mut log = fixture.log(limits());
        log.append_on(day(15), day(15), "new", |from, to| fs::rename(from, to))
            .expect("an undeletable archive refused the record");
        assert_eq!(fs::read_to_string(&log.path).unwrap(), "new\n");
        assert!(!expired.exists(), "the other expired archive was kept");
        assert!(held.exists());
        drop(reader);
        append(&mut log, day(16), "next");
        assert!(!held.exists(), "the released archive was not pruned later");
    }

    /// The other side of the same rule: when the archive that can't be
    /// removed keeps the family over its byte budget, the record is refused
    /// rather than growing the history past its bound.
    #[cfg(windows)]
    #[test]
    fn held_archive_over_budget_still_refuses_the_record() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let held = fixture.0.join("term.log.2026-11-15.000001");
        fs::write(&held, "x".repeat(20)).unwrap();
        // FILE_SHARE_READ | FILE_SHARE_WRITE, without FILE_SHARE_DELETE.
        let _reader = File::options()
            .read(true)
            .share_mode(1 | 2)
            .open(&held)
            .unwrap();
        // Budget: 24 total less an 8-byte live file leaves 16 for archives.
        let mut log = fixture.log(ProfileLogLimits {
            total_bytes: 24,
            ..limits()
        });
        assert!(
            log.append_on(day(15), day(15), "new", |from, to| fs::rename(from, to))
                .is_err()
        );
        assert!(!log.path.exists());
    }

    #[test]
    fn directory_log_path_does_not_move_contents_or_prune_neighbors() {
        let fixture = Fixture::new();
        let path = fixture.0.join("term.log");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep.txt"), "keep").unwrap();
        let neighbor = fixture.0.join("term.log.2026-11-01.000001");
        fs::write(&neighbor, "keep history").unwrap();
        let mut log = fixture.log(limits());
        assert!(
            log.append_on(day(15), day(15), "record", |from, to| fs::rename(from, to))
                .is_err()
        );
        assert_eq!(fs::read_to_string(path.join("keep.txt")).unwrap(), "keep");
        assert_eq!(fs::read_to_string(neighbor).unwrap(), "keep history");
    }
}
