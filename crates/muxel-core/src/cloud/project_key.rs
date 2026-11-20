//! Portable project identity. A loop names its project by a per-install UUID;
//! across devices the same project is recognised by its git remote (plus the
//! subdirectory within the repository), falling back to its name.

/// Normalise a git remote URL so the HTTPS and SSH spellings of one repository
/// compare equal: `git@github.com:Org/Repo.git`, `ssh://git@github.com/org/repo`
/// and `https://github.com/org/repo/` all become `github.com/org/repo`.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let rest = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        rest
    } else if let Some((userhost, path)) = url.split_once(':') {
        // scp-like `user@host:path` — but not a Windows drive (`C:\…`).
        if userhost.len() <= 1 || userhost.contains('/') {
            return None;
        }
        return normalize_remote(&format!("ssh://{userhost}/{path}"));
    } else {
        return None;
    };
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    let path = path
        .trim_matches('/')
        .trim_end_matches(".git")
        .trim_matches('/')
        .to_ascii_lowercase();
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

/// The portable key of a project: `git:<remote>[#<subdir>]` when it has a
/// remote, else `name:<lowercased name>`.
pub fn project_key(remote_url: Option<&str>, subdir: &str, name: &str) -> String {
    if let Some(remote) = remote_url.and_then(normalize_remote) {
        let sub = subdir.trim_matches(['/', '\\']).replace('\\', "/");
        if sub.is_empty() {
            format!("git:{remote}")
        } else {
            format!("git:{remote}#{sub}")
        }
    } else {
        format!("name:{}", name.trim().to_lowercase())
    }
}

/// Resolve a key against local projects `(key, id)`. A git key also matches a
/// project by name as a last resort only when the key carries none.
pub fn resolve<'a, I>(key: &str, local: I) -> Option<uuid::Uuid>
where
    I: IntoIterator<Item = (&'a str, uuid::Uuid)>,
{
    local.into_iter().find(|(k, _)| *k == key).map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::{normalize_remote, project_key};

    #[test]
    fn spellings_of_one_repo_agree() {
        let want = Some("github.com/org/repo".to_string());
        assert_eq!(normalize_remote("git@github.com:Org/Repo.git"), want);
        assert_eq!(normalize_remote("ssh://git@github.com/org/repo"), want);
        assert_eq!(normalize_remote("https://github.com/org/repo/"), want);
        assert_eq!(
            normalize_remote("https://user:pw@github.com:443/org/repo.git"),
            want
        );
    }

    #[test]
    fn non_remotes_are_rejected() {
        assert_eq!(normalize_remote(""), None);
        assert_eq!(normalize_remote("/srv/git/repo"), None);
        assert_eq!(normalize_remote("C:\\repos\\x"), None);
        assert_eq!(normalize_remote("file:///srv/repo"), None);
    }

    #[test]
    fn keys() {
        assert_eq!(
            project_key(Some("git@github.com:o/r.git"), "", "R"),
            "git:github.com/o/r"
        );
        assert_eq!(
            project_key(Some("git@github.com:o/r.git"), "crates\\app/", "R"),
            "git:github.com/o/r#crates/app"
        );
        assert_eq!(project_key(None, "", " My App "), "name:my app");
    }
}
