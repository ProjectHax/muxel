//! Transcript lookup for one launch batch. Call only from a worker.

use super::{claude_session_cwd_with_len, claude_session_path};
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ClaudeSessionRequest {
    pub id: String,
    pub path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeSession {
    pub path: PathBuf,
    pub cwd: Option<PathBuf>,
}

fn list(path: &Path) -> io::Result<std::fs::ReadDir> {
    #[cfg(test)]
    OPERATIONS.with_borrow_mut(|ops| ops.push(("directory", path.to_path_buf())));
    std::fs::read_dir(path)
}

fn stat(path: &Path) -> io::Result<std::fs::Metadata> {
    #[cfg(test)]
    OPERATIONS.with_borrow_mut(|ops| ops.push(("stat", path.to_path_buf())));
    std::fs::metadata(path)
}

fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.trim_start_matches("//?/").to_ascii_lowercase()
    } else {
        text
    }
}

fn stored_path(home: &Path, path: &Path, id: Uuid) -> bool {
    path.is_absolute()
        && !path.components().any(|part| part == Component::ParentDir)
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| Uuid::parse_str(name).ok())
            == Some(id)
        && path.extension().is_some_and(|ext| ext == "jsonl")
        && path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|parent| normalized(parent) == normalized(&home.join(".claude/projects")))
}

fn found(path: PathBuf, metadata: std::fs::Metadata) -> ClaudeSession {
    // A changing transcript can contain a later /cd even under the same UUID.
    // Reuse the stat's length; do not read metadata again in the tail reader.
    let cwd = claude_session_cwd_with_len(&path, metadata.len())
        .ok()
        .flatten()
        .filter(|cwd| cwd.is_dir());
    ClaudeSession { path, cwd }
}

/// Complete known paths first, then share one all-projects fallback among misses.
/// Errors are never converted to an expired identity. The callback lets a fast
/// pane launch while other panes are still searching their legacy transcripts.
pub fn discover_claude_sessions(
    home: &Path,
    requests: &[ClaudeSessionRequest],
    mut complete: impl FnMut(usize, io::Result<Option<ClaudeSession>>),
) {
    let mut pending: HashMap<Uuid, Vec<usize>> = HashMap::new();
    for (index, request) in requests.iter().enumerate() {
        let Ok(id) = Uuid::parse_str(&request.id) else {
            complete(index, Ok(None));
            continue;
        };
        let preferred = request
            .path
            .as_ref()
            .filter(|path| stored_path(home, path, id))
            .cloned()
            .or_else(|| {
                request
                    .cwd
                    .as_deref()
                    .map(|cwd| claude_session_path(home, cwd, &id.to_string()))
            });
        if let Some(path) = preferred {
            match stat(&path) {
                Ok(metadata) if metadata.is_file() => {
                    complete(index, Ok(Some(found(path, metadata))));
                    continue;
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    complete(index, Err(error));
                    continue;
                }
            }
        }
        pending.entry(id).or_default().push(index);
    }
    if pending.is_empty() {
        return;
    }
    let result = search_missing(home, &mut pending, &mut complete);
    for indexes in pending.into_values() {
        for index in indexes {
            complete(
                index,
                match &result {
                    Ok(()) => Ok(None),
                    Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
                },
            );
        }
    }
}

fn search_missing(
    home: &Path,
    pending: &mut HashMap<Uuid, Vec<usize>>,
    complete: &mut impl FnMut(usize, io::Result<Option<ClaudeSession>>),
) -> io::Result<()> {
    let projects_path = home.join(".claude/projects");
    let projects = match list(&projects_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Windows also reports NotFound when an ancestor is a file. That
            // is a broken store, not proof that the saved conversation expired.
            let mut parent = projects_path.parent();
            while let Some(path) = parent {
                match stat(path) {
                    Ok(metadata) if metadata.is_dir() => return Ok(()),
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            "Claude store ancestor is not a directory",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => parent = path.parent(),
                    Err(error) => return Err(error),
                }
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    for project in projects {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let entries = match list(&project.path()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".jsonl")) else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(stem) else {
                continue;
            };
            if !pending.contains_key(&id) {
                continue;
            }
            let path = entry.path();
            let metadata = match stat(&path) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let session = found(path, metadata);
            for index in pending.remove(&id).unwrap() {
                complete(index, Ok(Some(session.clone())));
            }
            if pending.is_empty() {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static OPERATIONS: std::cell::RefCell<Vec<(&'static str, PathBuf)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::{ClaudeSessionRequest, OPERATIONS, discover_claude_sessions};
    use uuid::Uuid;

    #[test]
    fn known_claude_path_has_one_stat_and_no_directory_search() {
        let home = std::env::temp_dir().join(format!("muxel-claude-bounded-{}", Uuid::new_v4()));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let id = Uuid::new_v4().to_string();
        let path = super::claude_session_path(&home, &cwd, &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::json!({"cwd":cwd}).to_string() + "\n").unwrap();
        OPERATIONS.with_borrow_mut(Vec::clear);
        let mut results = Vec::new();
        discover_claude_sessions(
            &home,
            &[ClaudeSessionRequest {
                id,
                path: Some(path.clone()),
                cwd: Some(cwd.clone()),
            }],
            |_, result| results.push(result.unwrap().unwrap()),
        );
        assert_eq!(results[0].path, path);
        assert_eq!(results[0].cwd, Some(cwd));
        assert_eq!(OPERATIONS.with_borrow(Clone::clone), vec![("stat", path)]);
    }

    #[test]
    fn legacy_claude_panes_share_one_project_directory_walk() {
        let home = std::env::temp_dir().join(format!("muxel-claude-batch-{}", Uuid::new_v4()));
        let project = home.join(".claude/projects/moved");
        std::fs::create_dir_all(&project).unwrap();
        let requests: Vec<_> = (0..3)
            .map(|_| {
                let id = Uuid::new_v4().to_string();
                std::fs::write(project.join(format!("{id}.jsonl")), "{}\n").unwrap();
                ClaudeSessionRequest {
                    id,
                    path: None,
                    cwd: None,
                }
            })
            .collect();
        OPERATIONS.with_borrow_mut(Vec::clear);
        let mut complete = 0;
        discover_claude_sessions(&home, &requests, |_, result| {
            assert!(result.unwrap().is_some());
            complete += 1;
        });
        assert_eq!(complete, 3);
        assert_eq!(
            OPERATIONS
                .with_borrow(|ops| ops.iter().filter(|(kind, _)| *kind == "directory").count()),
            2
        );
    }

    #[test]
    fn known_claude_path_reads_the_latest_cwd_even_when_identity_is_unchanged() {
        let home = std::env::temp_dir().join(format!("muxel-claude-cwd-{}", Uuid::new_v4()));
        // Both directories map to the same Claude project slug.
        let first = home.join("project-name");
        let later = home.join("project/name");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&later).unwrap();
        let id = Uuid::new_v4().to_string();
        let path = super::claude_session_path(&home, &first, &id);
        assert_eq!(path, super::claude_session_path(&home, &later, &id));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::json!({"cwd":first}).to_string() + "\n").unwrap();
        let requests = [ClaudeSessionRequest {
            id,
            path: Some(path.clone()),
            cwd: Some(first.clone()),
        }];
        let mut found = None;
        discover_claude_sessions(&home, &requests, |_, result| found = result.unwrap());
        assert_eq!(found.as_ref().unwrap().cwd, Some(first.clone()));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({"cwd":first}),
                serde_json::json!({"relocatedCwd":later})
            ),
        )
        .unwrap();
        OPERATIONS.with_borrow_mut(Vec::clear);
        discover_claude_sessions(&home, &requests, |_, result| found = result.unwrap());
        assert_eq!(found.unwrap().cwd, Some(later));
        assert_eq!(OPERATIONS.with_borrow(Clone::clone), vec![("stat", path)]);
    }

    #[test]
    fn fast_claude_paths_complete_before_legacy_fallback_and_errors_preserve_identity() {
        let home = std::env::temp_dir().join(format!("muxel-claude-fast-{}", Uuid::new_v4()));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let id = Uuid::new_v4().to_string();
        let path = super::claude_session_path(&home, &cwd, &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        let requests = [
            ClaudeSessionRequest {
                id: Uuid::new_v4().to_string(),
                path: None,
                cwd: None,
            },
            ClaudeSessionRequest {
                id,
                path: Some(path),
                cwd: Some(cwd),
            },
        ];
        let mut order = Vec::new();
        discover_claude_sessions(&home, &requests, |index, result| {
            assert!(result.is_ok());
            order.push(index);
        });
        assert_eq!(order, vec![1, 0]);
        let broken = home.join("not-a-directory");
        std::fs::write(&broken, "x").unwrap();
        let mut errors = 0;
        discover_claude_sessions(&broken, &requests, |_, result| {
            assert!(result.is_err());
            errors += 1;
        });
        assert_eq!(errors, 2);
    }
}
