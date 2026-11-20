use muxel_core::Instance;
use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) fn saved_conversation_available(instance: &Instance) -> Option<bool> {
    let home = super::home_dir()?;
    let id = Uuid::parse_str(instance.session_id.as_deref()?).ok()?;
    let path = conversation_path(&home, id);
    if instance.session_path.as_ref() != Some(&path) {
        return None;
    }
    match conversation_exists(&path) {
        Ok(true) => Some(true),
        // A missing store is inconclusive (the profile may be unavailable).
        Ok(false) if path.parent()?.is_dir() => Some(false),
        _ => None,
    }
}

fn store(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli")
}

pub(super) fn conversation_path(home: &Path, id: Uuid) -> PathBuf {
    store(home).join("conversations").join(format!("{id}.db"))
}

pub(super) fn conversation_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_file() && info.file_attributes() & REPARSE_POINT == 0 => {
            Ok(info.len() > 0)
        }
        Ok(_) => Err(io::Error::other(
            "conversation database is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

const REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const SHARE_READ_WRITE_DELETE: u32 = 0x7;
const FILE_PROCESS_IDS_USING_FILE_INFORMATION: u32 = 47;

#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationFile(
        file: RawHandle,
        status_block: *mut IoStatusBlock,
        information: *mut usize,
        length: u32,
        class: u32,
    ) -> i32;
}

/// An empty presence file can remain indefinitely after release. Query its
/// current holders. The zero-access handle does not itself count as a holder.
/// Recorded on Windows 11 x64 build 26300 with Antigravity CLI 1.5.13.
fn presence_owners(path: &Path, response: &mut [usize; 128]) -> io::Result<usize> {
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut status_block = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // ULONG count, pointer-aligned ULONG_PTR array. Oversize/unsupported
    // replies fail closed. This operation requires no elevation.
    // SAFETY: The owned file handle and sized, aligned buffers outlive this
    // synchronous query. Nothing from the buffers is used on an error.
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            response.as_mut_ptr(),
            size_of_val(response) as u32,
            FILE_PROCESS_IDS_USING_FILE_INFORMATION,
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!(
            "presence owner query returned {status:#x}"
        )));
    }
    let count = response[0] as u32 as usize;
    if count >= response.len() {
        return Err(io::Error::other("invalid presence owner count"));
    }
    Ok(count)
}

pub(super) fn holds_conversation(home: &Path, pid: u32, id: Uuid) -> bool {
    let lock = store(home).join("presence").join(format!("{id}.lock"));
    let mut response = [0; 128];
    fs::symlink_metadata(&lock).is_ok_and(|meta| {
        meta.is_file() && meta.len() == 0 && meta.file_attributes() & REPARSE_POINT == 0
    }) && presence_owners(&lock, &mut response)
        .is_ok_and(|count| count == 1 && response[1] == pid as usize)
}

/// Index one presence scan by the direct PTY children we actually launched.
/// A process holding more than one presence lock has no proven current UUID.
pub(super) fn active_conversations(
    home: &Path,
    children: &HashSet<u32>,
) -> io::Result<HashMap<u32, (Uuid, PathBuf)>> {
    let mut active = HashMap::new();
    if children.is_empty() {
        return Ok(active);
    }
    let entries = match fs::read_dir(store(home).join("presence")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(active),
        Err(error) => return Err(error),
    };
    let mut found = HashMap::new();
    let mut response = [0; 128];
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(stem) else {
            continue;
        };
        if id.to_string() != stem {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file()
            || metadata.len() != 0
            || metadata.file_attributes() & REPARSE_POINT != 0
        {
            continue;
        }
        let count = match presence_owners(&entry.path(), &mut response) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for owner in response[1..count + 1]
            .iter()
            .filter_map(|owner| u32::try_from(*owner).ok())
        {
            if !children.contains(&owner) {
                continue;
            }
            match found.entry(owner) {
                Entry::Vacant(slot) => {
                    slot.insert((count == 1).then_some(id));
                }
                Entry::Occupied(mut slot) => {
                    slot.insert(None);
                }
            }
        }
    }
    for (pid, id) in found.into_iter().filter_map(|(pid, id)| Some((pid, id?))) {
        let path = conversation_path(home, id);
        if conversation_exists(&path)? && holds_conversation(home, pid, id) {
            active.insert(pid, (id, path));
        }
    }
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::{active_conversations, conversation_path, holds_conversation, store};
    use std::collections::HashSet;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    struct TempHome(PathBuf);

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn antigravity_presence_tracks_only_the_current_owned_lock() {
        let path = std::env::temp_dir().join(format!("muxel-antigravity-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        let root = TempHome(path);
        let home = &root.0;
        let presence = store(home).join("presence");
        fs::create_dir_all(&presence).unwrap();
        fs::create_dir_all(store(home).join("conversations")).unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let own = std::process::id();
        for id in [first, second] {
            fs::write(conversation_path(home, id), b"fixture").unwrap();
        }
        let first_lock = fs::File::create(presence.join(format!("{first}.lock"))).unwrap();
        let children = HashSet::from([own]);
        let observed = active_conversations(home, &children).unwrap();
        assert_eq!(observed.get(&own).map(|result| result.0), Some(first));
        assert!(holds_conversation(home, own, first));
        assert!(
            active_conversations(home, &HashSet::from([u32::MAX]))
                .unwrap()
                .is_empty()
        );
        let second_lock = fs::File::create(presence.join(format!("{second}.lock"))).unwrap();
        assert!(active_conversations(home, &children).unwrap().is_empty());
        drop(first_lock);
        assert!(!holds_conversation(home, own, first));
        let observed = active_conversations(home, &children).unwrap();
        assert_eq!(observed.get(&own).map(|result| result.0), Some(second));
        drop(second_lock);
        assert!(active_conversations(home, &children).unwrap().is_empty());
    }
}
