//! Library configuration helpers: display name, add-time validation and
//! duplicate detection. A library's `url` and `branch` are fixed once added.

use super::LibraryConfig;

/// Why adding a library was rejected; the app translates each variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddError {
    EmptyUrl,
    UrlStartsWithDash,
    BranchStartsWithDash,
    /// Same trimmed URL and branch as a configured library.
    Duplicate,
}

/// Display name of a library: the configured name if it is not
/// blank, else the last non-empty `/`- or `\`-separated segment of the URL
/// without a `.git` suffix, else the whole trimmed URL.
pub fn display_name(config: &LibraryConfig) -> String {
    let name = config.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let url = config.url.trim();
    let segment = url
        .split(['/', '\\'])
        .rfind(|s| !s.is_empty())
        .map(|s| s.strip_suffix(".git").unwrap_or(s));
    match segment {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => url.to_string(),
    }
}

/// Validates the URL and branch of a library being added and returns them
/// trimmed. Duplicates are checked separately with [`is_duplicate`].
pub fn validate_new(url: &str, branch: &str) -> Result<(String, String), AddError> {
    let url = url.trim();
    let branch = branch.trim();
    if url.is_empty() {
        return Err(AddError::EmptyUrl);
    }
    if url.starts_with('-') {
        return Err(AddError::UrlStartsWithDash);
    }
    if branch.starts_with('-') {
        return Err(AddError::BranchStartsWithDash);
    }
    Ok((url.to_string(), branch.to_string()))
}

/// Whether a library with this URL and branch is already configured:
/// case-sensitive after trimming; the empty branch is its own value.
pub fn is_duplicate(configs: &[LibraryConfig], url: &str, branch: &str) -> bool {
    let url = url.trim();
    let branch = branch.trim();
    configs
        .iter()
        .any(|c| c.url.trim() == url && c.branch.trim() == branch)
}

/// Trims a display name to store; `""` means "use the default name".
pub fn normalize_name(name: &str) -> String {
    name.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{AddError, display_name, is_duplicate, normalize_name, validate_new};
    use crate::library::LibraryConfig;
    use uuid::Uuid;

    fn lib(url: &str, branch: &str, name: &str) -> LibraryConfig {
        LibraryConfig {
            id: Uuid::new_v4(),
            url: url.to_string(),
            branch: branch.to_string(),
            name: name.to_string(),
            last_pull_ok: None,
        }
    }

    fn unnamed(url: &str) -> String {
        display_name(&lib(url, "", ""))
    }

    #[test]
    fn name_from_https_url_with_git_suffix() {
        assert_eq!(unnamed("https://github.com/acme/team-lib.git"), "team-lib");
    }

    #[test]
    fn name_from_scp_like_ssh_url() {
        assert_eq!(unnamed("git@github.com:acme/team-lib.git"), "team-lib");
    }

    #[test]
    fn name_from_file_url_with_trailing_slash() {
        assert_eq!(unnamed("file:///tmp/x/R/"), "R");
    }

    #[test]
    fn name_from_windows_path() {
        assert_eq!(unnamed("C:\\repos\\lib"), "lib");
    }

    #[test]
    fn name_from_windows_path_with_trailing_backslash() {
        assert_eq!(unnamed("C:\\repos\\lib\\"), "lib");
    }

    #[test]
    fn segment_empty_after_git_suffix_falls_back_to_url() {
        assert_eq!(unnamed("https://h/.git"), "https://h/.git");
    }

    #[test]
    fn no_non_empty_segment_falls_back_to_url() {
        assert_eq!(unnamed("///"), "///");
    }

    #[test]
    fn configured_name_wins() {
        let c = lib("https://github.com/acme/team-lib.git", "", "Acme");
        assert_eq!(display_name(&c), "Acme");
    }

    #[test]
    fn blank_configured_name_counts_as_empty() {
        let c = lib("https://github.com/acme/team-lib.git", "", "   ");
        assert_eq!(display_name(&c), "team-lib");
    }

    #[test]
    fn fallback_is_trimmed_url() {
        assert_eq!(unnamed("  https://h/.git  "), "https://h/.git");
    }

    #[test]
    fn git_only_stripped_as_suffix() {
        assert_eq!(unnamed("https://h/a.git.b"), "a.git.b");
    }

    #[test]
    fn normalize_name_trims() {
        assert_eq!(normalize_name("  Team  "), "Team");
        assert_eq!(normalize_name("   "), "");
    }

    #[test]
    fn renamed_then_blank_back_to_default() {
        let mut c = lib("file://R", "", "");
        c.name = normalize_name("  Team  ");
        assert_eq!(display_name(&c), "Team");
        assert_eq!(c.url, "file://R");
        assert_eq!(c.branch, "");
        c.name = normalize_name("   ");
        assert_eq!(display_name(&c), "R");
    }

    fn try_add(list: &mut Vec<LibraryConfig>, url: &str, branch: &str) -> Result<(), AddError> {
        let (url, branch) = validate_new(url, branch)?;
        if is_duplicate(list, &url, &branch) {
            return Err(AddError::Duplicate);
        }
        list.push(lib(&url, &branch, ""));
        Ok(())
    }

    #[test]
    fn empty_url_rejected() {
        let mut list = Vec::new();
        assert_eq!(try_add(&mut list, "", ""), Err(AddError::EmptyUrl));
        assert!(list.is_empty());
    }

    #[test]
    fn blank_url_rejected() {
        let mut list = Vec::new();
        assert_eq!(try_add(&mut list, "   ", ""), Err(AddError::EmptyUrl));
        assert!(list.is_empty());
    }

    #[test]
    fn url_starting_with_dash_rejected() {
        let mut list = Vec::new();
        assert_eq!(
            try_add(&mut list, "-uhack", ""),
            Err(AddError::UrlStartsWithDash)
        );
        assert!(list.is_empty());
    }

    #[test]
    fn url_starting_with_dash_after_trim_rejected() {
        let mut list = Vec::new();
        assert_eq!(
            try_add(&mut list, " --upload-pack=x", ""),
            Err(AddError::UrlStartsWithDash)
        );
        assert!(list.is_empty());
    }

    #[test]
    fn branch_starting_with_dash_rejected() {
        let mut list = Vec::new();
        assert_eq!(
            try_add(&mut list, "file://R", "--orphan"),
            Err(AddError::BranchStartsWithDash)
        );
        assert!(list.is_empty());
    }

    #[test]
    fn valid_input_is_trimmed_and_accepted() {
        assert_eq!(
            validate_new("  file://my-repo  ", " feat-x "),
            Ok(("file://my-repo".to_string(), "feat-x".to_string()))
        );
        assert_eq!(
            validate_new("file://R", ""),
            Ok(("file://R".to_string(), String::new()))
        );
    }

    #[test]
    fn same_url_default_branch_is_duplicate() {
        let mut list = vec![lib("file://R", "", "")];
        assert_eq!(try_add(&mut list, "file://R", ""), Err(AddError::Duplicate));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn padded_url_is_duplicate() {
        let mut list = vec![lib("file://R", "", "")];
        assert_eq!(
            try_add(&mut list, "  file://R  ", ""),
            Err(AddError::Duplicate)
        );
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn named_branch_differs_from_default_branch() {
        let list = vec![lib("file://R", "", "")];
        assert!(!is_duplicate(&list, "file://R", "main"));
    }

    #[test]
    fn trailing_slash_differs() {
        let list = vec![lib("file://R", "", "")];
        assert!(!is_duplicate(&list, "file://R/", ""));
    }

    #[test]
    fn url_case_differs() {
        let list = vec![lib("file://R", "", "")];
        assert!(!is_duplicate(&list, "FILE://R", ""));
    }

    #[test]
    fn all_three_accepted_as_distinct_libraries() {
        let mut list = vec![lib("file://R", "", "")];
        assert_eq!(try_add(&mut list, "file://R", "main"), Ok(()));
        assert_eq!(try_add(&mut list, "file://R/", ""), Ok(()));
        assert_eq!(try_add(&mut list, "FILE://R", ""), Ok(()));
        assert_eq!(list.len(), 4);
    }

    #[test]
    fn padded_branch_is_duplicate() {
        let list = vec![lib("file://R", "main", "")];
        assert!(is_duplicate(&list, "file://R", "  main "));
        assert!(!is_duplicate(&list, "file://R", ""));
    }
}
