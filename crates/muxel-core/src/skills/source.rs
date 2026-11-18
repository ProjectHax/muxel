//! Where to fetch skills from: what the user typed into "Install from GitHub",
//! parsed the way the `skills` CLI reads its source argument.
//!
//! Accepted: `owner/repo`, `owner/repo/sub/path`, `owner/repo@skill`,
//! `github:`/`gitlab:` prefixes, a `#ref` suffix on anything, GitHub
//! `https://github.com/o/r[/tree/<ref>/<path>]` URLs, GitLab `/-/tree/` URLs,
//! SSH (`git@host:o/r.git`) and any other git URL.

/// Which host a source lives on; the lock records it as `sourceType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Github,
    Gitlab,
    Git,
}

impl SourceKind {
    pub fn lock_name(self) -> &'static str {
        match self {
            SourceKind::Github => "github",
            SourceKind::Gitlab => "gitlab",
            SourceKind::Git => "git",
        }
    }
}

/// A parsed source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillSource {
    pub kind: SourceKind,
    /// What `git clone` is given.
    pub clone_url: String,
    /// The short form shown and stored as the lock's `source`: `owner/repo`
    /// for GitHub/GitLab, else the URL.
    pub display: String,
    /// Branch, tag or commit to fetch; `None` for the default branch.
    pub git_ref: Option<String>,
    /// Folder inside the repository to look in (no leading/trailing `/`).
    pub subpath: Option<String>,
    /// Only this skill (by frontmatter name or folder name).
    pub skill: Option<String>,
}

impl SkillSource {
    /// A web page for the source, when it's on GitHub or GitLab.
    pub fn web_url(&self) -> Option<String> {
        let host = match self.kind {
            SourceKind::Github => "https://github.com",
            SourceKind::Gitlab => "https://gitlab.com",
            SourceKind::Git => return None,
        };
        Some(format!("{host}/{}", self.display))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    Empty,
    /// Not something muxel can fetch.
    Unrecognized,
    /// A path that climbs out of the repository (`..`) or is absolute.
    UnsafePath,
}

/// Parse what the user typed.
pub fn parse(input: &str) -> Result<SkillSource, SourceError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(SourceError::Empty);
    }
    // An argument git would read as an option.
    if s.starts_with('-') || s.chars().any(char::is_whitespace) {
        return Err(SourceError::Unrecognized);
    }
    let (s, fragment_ref) = match s.split_once('#') {
        Some((a, r)) if !r.is_empty() => (a, Some(r.to_string())),
        Some((a, _)) => (a, None),
        None => (s, None),
    };
    let mut src = if let Some(rest) = s.strip_prefix("github:") {
        shorthand(rest, SourceKind::Github)?
    } else if let Some(rest) = s.strip_prefix("gitlab:") {
        shorthand(rest, SourceKind::Gitlab)?
    } else if s.contains("://") {
        url(s)?
    } else if let Some(rest) = s.strip_prefix("git@") {
        ssh(rest)?
    } else {
        shorthand(s, SourceKind::Github)?
    };
    if fragment_ref.is_some() {
        src.git_ref = fragment_ref;
    }
    if let Some(r) = &src.git_ref
        && (r.starts_with('-') || r.contains(".."))
    {
        return Err(SourceError::Unrecognized);
    }
    Ok(src)
}

fn valid_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Clean a subpath: `./` and slashes trimmed, `..` and absolute paths refused.
pub fn clean_subpath(p: &str) -> Result<Option<String>, SourceError> {
    let p = p.trim().replace('\\', "/");
    if p.starts_with('/') || p.as_bytes().get(1) == Some(&b':') {
        return Err(SourceError::UnsafePath);
    }
    let parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(SourceError::UnsafePath);
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

/// Split a trailing `@x`: a version or commit is a ref, anything else names a
/// skill.
fn split_at_suffix(s: &str) -> (&str, Option<String>, Option<String>) {
    match s.rsplit_once('@') {
        Some((head, tail)) if !tail.is_empty() && !head.is_empty() => {
            if looks_like_ref(tail) {
                (head, Some(tail.to_string()), None)
            } else {
                (head, None, Some(tail.to_string()))
            }
        }
        _ => (s, None, None),
    }
}

fn looks_like_ref(s: &str) -> bool {
    let sha = s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit());
    let v = s.strip_prefix('v').unwrap_or(s);
    let semver = v.split('.').count() >= 2
        && v.split('.')
            .next()
            .is_some_and(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()));
    sha || semver
}

fn shorthand(s: &str, kind: SourceKind) -> Result<SkillSource, SourceError> {
    let (s, git_ref, skill) = split_at_suffix(s);
    let parts: Vec<&str> = s.trim_matches('/').split('/').collect();
    if parts.len() < 2 {
        return Err(SourceError::Unrecognized);
    }
    if parts.contains(&"..") {
        return Err(SourceError::UnsafePath);
    }
    let (owner, repo) = (parts[0], parts[1].trim_end_matches(".git"));
    if !valid_segment(owner) || !valid_segment(repo) {
        return Err(SourceError::Unrecognized);
    }
    let subpath = clean_subpath(&parts[2..].join("/"))?;
    Ok(hosted(kind, owner, repo, git_ref, subpath, skill))
}

fn hosted(
    kind: SourceKind,
    owner: &str,
    repo: &str,
    git_ref: Option<String>,
    subpath: Option<String>,
    skill: Option<String>,
) -> SkillSource {
    let host = match kind {
        SourceKind::Gitlab => "gitlab.com",
        _ => "github.com",
    };
    SkillSource {
        kind,
        clone_url: format!("https://{host}/{owner}/{repo}.git"),
        display: format!("{owner}/{repo}"),
        git_ref,
        subpath,
        skill,
    }
}

fn url(s: &str) -> Result<SkillSource, SourceError> {
    let (scheme, rest) = s.split_once("://").ok_or(SourceError::Unrecognized)?;
    if !matches!(scheme, "https" | "http" | "ssh" | "git" | "file") {
        return Err(SourceError::Unrecognized);
    }
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host_name = host.rsplit('@').next().unwrap_or(host).to_ascii_lowercase();
    let path = path.trim_end_matches('/');
    if matches!(scheme, "https" | "http") && host_name == "github.com" {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            let (owner, repo) = (parts[0], parts[1].trim_end_matches(".git"));
            if valid_segment(owner) && valid_segment(repo) {
                let (git_ref, subpath) = if parts.len() >= 4 && matches!(parts[2], "tree" | "blob")
                {
                    let sub = clean_subpath(&parts[4..].join("/"))?;
                    // A link to a file (…/blob/main/x/SKILL.md) means its folder.
                    let sub = sub.map(|p| {
                        p.strip_suffix("/SKILL.md")
                            .map(str::to_string)
                            .unwrap_or_else(|| if p == "SKILL.md" { String::new() } else { p })
                    });
                    (Some(parts[3].to_string()), sub.filter(|p| !p.is_empty()))
                } else {
                    (None, None)
                };
                return Ok(hosted(
                    SourceKind::Github,
                    owner,
                    repo,
                    git_ref,
                    subpath,
                    None,
                ));
            }
        }
    }
    if matches!(scheme, "https" | "http") && host_name == "gitlab.com" {
        let (repo_path, tree) = match path.split_once("/-/tree/") {
            Some((r, t)) => (r, Some(t)),
            None => (path, None),
        };
        let repo_path = repo_path.trim_end_matches(".git");
        if repo_path.split('/').count() >= 2 && repo_path.split('/').all(valid_segment) {
            let (git_ref, subpath) = match tree {
                Some(t) => {
                    let (r, sub) = t.split_once('/').unwrap_or((t, ""));
                    (Some(r.to_string()), clean_subpath(sub)?)
                }
                None => (None, None),
            };
            return Ok(SkillSource {
                kind: SourceKind::Gitlab,
                clone_url: format!("https://gitlab.com/{repo_path}.git"),
                display: repo_path.to_string(),
                git_ref,
                subpath,
                skill: None,
            });
        }
    }
    if path.split('/').any(|p| p == "..") {
        return Err(SourceError::UnsafePath);
    }
    Ok(SkillSource {
        kind: SourceKind::Git,
        clone_url: s.to_string(),
        display: s.to_string(),
        git_ref: None,
        subpath: None,
        skill: None,
    })
}

/// `git@host:owner/repo.git`.
fn ssh(rest: &str) -> Result<SkillSource, SourceError> {
    let (host, path) = rest.split_once(':').ok_or(SourceError::Unrecognized)?;
    if host.is_empty() || path.is_empty() || path.split('/').any(|p| p == "..") {
        return Err(SourceError::Unrecognized);
    }
    let repo_path = path.trim_end_matches(".git");
    let kind = match host {
        "github.com" => SourceKind::Github,
        "gitlab.com" => SourceKind::Gitlab,
        _ => SourceKind::Git,
    };
    let display = if kind == SourceKind::Git {
        format!("git@{rest}")
    } else {
        repo_path.to_string()
    };
    Ok(SkillSource {
        kind,
        clone_url: format!("git@{rest}"),
        display,
        git_ref: None,
        subpath: None,
        skill: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{SourceError, SourceKind, clean_subpath, parse};

    #[test]
    fn github_shorthand() {
        let s = parse("cloudflare/security-audit-skill").unwrap();
        assert_eq!(s.kind, SourceKind::Github);
        assert_eq!(
            s.clone_url,
            "https://github.com/cloudflare/security-audit-skill.git"
        );
        assert_eq!(s.display, "cloudflare/security-audit-skill");
        assert_eq!(
            s.web_url().unwrap(),
            "https://github.com/cloudflare/security-audit-skill"
        );
        assert_eq!((s.git_ref, s.subpath, s.skill), (None, None, None));
    }

    #[test]
    fn shorthand_with_path_skill_and_ref() {
        let s = parse("anthropics/skills/skills/pdf").unwrap();
        assert_eq!(s.subpath.as_deref(), Some("skills/pdf"));
        let s = parse("anthropics/skills@pdf").unwrap();
        assert_eq!(s.skill.as_deref(), Some("pdf"));
        assert_eq!(s.git_ref, None);
        let s = parse("o/r@v1.2.0").unwrap();
        assert_eq!(s.git_ref.as_deref(), Some("v1.2.0"));
        let s = parse("o/r#dev").unwrap();
        assert_eq!(s.git_ref.as_deref(), Some("dev"));
        let s = parse("gitlab:g/r").unwrap();
        assert_eq!(s.clone_url, "https://gitlab.com/g/r.git");
    }

    #[test]
    fn github_urls() {
        let s = parse("https://github.com/obra/superpowers").unwrap();
        assert_eq!(s.display, "obra/superpowers");
        let s = parse("https://github.com/anthropics/skills/tree/main/skills/pdf/").unwrap();
        assert_eq!(s.git_ref.as_deref(), Some("main"));
        assert_eq!(s.subpath.as_deref(), Some("skills/pdf"));
        let s = parse("https://github.com/o/r/blob/main/skills/x/SKILL.md").unwrap();
        assert_eq!(s.subpath.as_deref(), Some("skills/x"));
        let s = parse("https://github.com/o/r.git").unwrap();
        assert_eq!(s.clone_url, "https://github.com/o/r.git");
    }

    #[test]
    fn gitlab_ssh_and_other_urls() {
        let s = parse("https://gitlab.com/g/sub/r/-/tree/main/skills").unwrap();
        assert_eq!(s.kind, SourceKind::Gitlab);
        assert_eq!(s.display, "g/sub/r");
        assert_eq!(s.subpath.as_deref(), Some("skills"));
        let s = parse("git@github.com:o/r.git").unwrap();
        assert_eq!((s.kind, s.display.as_str()), (SourceKind::Github, "o/r"));
        assert_eq!(s.clone_url, "git@github.com:o/r.git");
        let s = parse("https://git.example.com/team/skills.git").unwrap();
        assert_eq!(s.kind, SourceKind::Git);
        assert_eq!(s.web_url(), None);
    }

    #[test]
    fn rejects_unsafe_or_unusable_input() {
        assert_eq!(parse("  "), Err(SourceError::Empty));
        assert_eq!(parse("--upload-pack=x"), Err(SourceError::Unrecognized));
        assert_eq!(parse("justone"), Err(SourceError::Unrecognized));
        assert_eq!(parse("o/r/../../etc"), Err(SourceError::UnsafePath));
        assert_eq!(parse("o/r#-x"), Err(SourceError::Unrecognized));
        assert_eq!(parse("a b/c"), Err(SourceError::Unrecognized));
        assert_eq!(parse("ftp://x/y"), Err(SourceError::Unrecognized));
        assert_eq!(clean_subpath("/etc"), Err(SourceError::UnsafePath));
        assert_eq!(clean_subpath("./a//b/"), Ok(Some("a/b".into())));
    }
}
