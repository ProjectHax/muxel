//! Pure file-tree model for the file browser: turn a flat (gitignore-aware) list
//! of project file paths into renderable rows — an expandable folder tree, or a
//! flat search result. No GPUI here, so it's unit-testable on its own.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// One renderable row in the file browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Absolute path (a file, or a directory when `is_dir`).
    pub abs_path: PathBuf,
    /// Display label (the final component for the tree; the relative path in search).
    pub name: String,
    /// Nesting depth from the project root (root's children are depth 0).
    pub depth: usize,
    pub is_dir: bool,
    /// For a directory, whether it's currently expanded (its children follow).
    pub expanded: bool,
}

/// Immediate children of `dir` derived from the flat file list: `name -> is_dir`
/// (a name is a directory if some file lies deeper under it), directories first
/// then files, each case-insensitively alphabetical.
fn children_of(dir: &Path, files: &[PathBuf]) -> Vec<(String, bool)> {
    let mut map: BTreeMap<String, bool> = BTreeMap::new();
    for f in files {
        let Ok(rel) = f.strip_prefix(dir) else {
            continue;
        };
        let mut comps = rel.components();
        let Some(first) = comps.next() else {
            continue;
        };
        let name = first.as_os_str().to_string_lossy().to_string();
        let is_dir = comps.next().is_some(); // more components below → it's a directory
        map.entry(name)
            .and_modify(|d| *d = *d || is_dir)
            .or_insert(is_dir);
    }
    let mut v: Vec<(String, bool)> = map.into_iter().collect();
    // Directories first (`!is_dir` puts false<true → dirs lead), then name.
    v.sort_by_key(|(name, is_dir)| (!*is_dir, name.to_lowercase()));
    v
}

fn emit(
    dir: &Path,
    depth: usize,
    files: &[PathBuf],
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<Row>,
) {
    for (name, is_dir) in children_of(dir, files) {
        let abs = dir.join(&name);
        let expand = is_dir && expanded.contains(&abs);
        out.push(Row {
            abs_path: abs.clone(),
            name,
            depth,
            is_dir,
            expanded: expand,
        });
        if expand {
            emit(&abs, depth + 1, files, expanded, out);
        }
    }
}

/// The visible tree rows for the browser: top-down, descending into a directory
/// only when it's in `expanded`.
pub fn visible_rows(files: &[PathBuf], root: &Path, expanded: &HashSet<PathBuf>) -> Vec<Row> {
    let mut out = Vec::new();
    emit(root, 0, files, expanded, &mut out);
    out
}

/// Flat search result: files whose path (relative to `root`) contains `query`
/// (case-insensitive). The label is the relative path; capped for snappiness.
pub fn filter(files: &[PathBuf], root: &Path, query: &str) -> Vec<Row> {
    let q = query.to_lowercase();
    let mut out: Vec<Row> = files
        .iter()
        .filter_map(|f| {
            let rel = f
                .strip_prefix(root)
                .unwrap_or(f)
                .to_string_lossy()
                .to_string();
            rel.to_lowercase().contains(&q).then(|| Row {
                abs_path: f.clone(),
                name: rel,
                depth: 0,
                is_dir: false,
                expanded: false,
            })
        })
        .collect();
    out.sort_by_key(|r| r.name.to_lowercase());
    out.truncate(500);
    out
}

#[cfg(test)]
mod tests {
    use super::{Row, filter, visible_rows};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    fn files() -> Vec<PathBuf> {
        [
            "/proj/src/app.rs",
            "/proj/src/editor.rs",
            "/proj/docs/README.md",
            "/proj/FEATURES.md",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }

    fn names(rows: &[Row]) -> Vec<(String, usize, bool)> {
        rows.iter()
            .map(|r| (r.name.clone(), r.depth, r.is_dir))
            .collect()
    }

    #[test]
    fn collapsed_shows_top_level_dirs_first() {
        let rows = visible_rows(&files(), Path::new("/proj"), &HashSet::new());
        assert_eq!(
            names(&rows),
            vec![
                ("docs".into(), 0, true),
                ("src".into(), 0, true),
                ("FEATURES.md".into(), 0, false),
            ]
        );
    }

    #[test]
    fn expanding_a_dir_reveals_its_children() {
        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("/proj/src"));
        let rows = visible_rows(&files(), Path::new("/proj"), &expanded);
        assert_eq!(
            names(&rows),
            vec![
                ("docs".into(), 0, true),
                ("src".into(), 0, true),
                ("app.rs".into(), 1, false),
                ("editor.rs".into(), 1, false),
                ("FEATURES.md".into(), 0, false),
            ]
        );
    }

    #[test]
    fn search_is_flat_and_case_insensitive() {
        let rows = filter(&files(), Path::new("/proj"), "EDITOR");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "src/editor.rs");
        assert!(!rows[0].is_dir);
        assert_eq!(rows[0].abs_path, PathBuf::from("/proj/src/editor.rs"));
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(filter(&files(), Path::new("/proj"), "").len(), 4);
    }
}
