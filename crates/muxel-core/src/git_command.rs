//! A git command the user types into a project's **Git command…** dialog:
//! turning the line into git's arguments, and the part of what it printed that
//! the dialog shows. Pure — the app runs the arguments with `git -C <root>`,
//! locally or over SSH ([`crate::remote_ops::git_unattended`]).

/// Why a typed line isn't a git command muxel can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCommandError {
    /// Nothing to run: blank, or a bare `git`.
    Empty,
    /// An unclosed quote or a trailing backslash.
    Unbalanced,
}

/// git's arguments in `line`, split as a shell splits words
/// ([`crate::split_words`]): quotes group, nothing expands, and `;`, `&&` or `|`
/// are passed to git as arguments rather than run. A leading `git` is dropped,
/// so `git log -3` and `log -3` run the same command.
pub fn parse_git_command(line: &str) -> Result<Vec<String>, GitCommandError> {
    let mut words = crate::split_words(line).ok_or(GitCommandError::Unbalanced)?;
    if words.first().is_some_and(|w| w == "git") {
        words.remove(0);
    }
    if words.is_empty() {
        return Err(GitCommandError::Empty);
    }
    Ok(words)
}

/// How a notification names a typed command: `git <line>`, not doubling a
/// `git` the user typed, and cut short past 60 characters.
pub fn command_title(line: &str) -> String {
    let line = line.trim();
    let line = line
        .strip_prefix("git")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        .map_or(line, str::trim_start);
    let mut title = format!("git {line}");
    if let Some((cut, _)) = title.char_indices().nth(60) {
        title.truncate(cut);
        title.push('…');
    }
    title
}

/// What a command printed, as the dialog shows it: stdout, then stderr (git
/// writes push/fetch progress and most errors there), each without trailing
/// blank lines.
pub fn command_output(stdout: &str, stderr: &str) -> String {
    let (out, err) = (stdout.trim_end(), stderr.trim_end());
    match (out.is_empty(), err.is_empty()) {
        (false, false) => format!("{out}\n{err}"),
        (false, true) => out.to_string(),
        _ => err.to_string(),
    }
}

/// The dialog shows at most this many lines of output…
pub const MAX_OUTPUT_LINES: usize = 2000;
/// …and at most this many bytes of it.
pub const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// The start of `text` within `max_lines` lines and `max_bytes` bytes (cut on a
/// character boundary), and whether anything was left off.
pub fn clip_output(text: &str, max_lines: usize, max_bytes: usize) -> (&str, bool) {
    let mut end = text.len();
    if let Some((ix, _)) = text.match_indices('\n').nth(max_lines.saturating_sub(1)) {
        end = if max_lines == 0 { 0 } else { ix };
    }
    if end > max_bytes {
        end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    let clipped = text[end..].trim_start_matches('\n');
    (&text[..end], !clipped.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{GitCommandError, clip_output, command_output, command_title, parse_git_command};

    fn args(line: &str) -> Vec<String> {
        parse_git_command(line).unwrap()
    }

    #[test]
    fn plain_words_are_gits_arguments() {
        assert_eq!(args("log --oneline -5"), ["log", "--oneline", "-5"]);
    }

    #[test]
    fn a_leading_git_is_dropped() {
        assert_eq!(args("git status"), ["status"]);
        assert_eq!(args("  git   status  "), ["status"]);
        // Only a word of its own: `gitk` or a later `git` stay.
        assert_eq!(args("gitk"), ["gitk"]);
        assert_eq!(args("grep git"), ["grep", "git"]);
    }

    #[test]
    fn quotes_group_a_message() {
        assert_eq!(
            args(r#"commit -am "fix the thing""#),
            ["commit", "-am", "fix the thing"]
        );
        assert_eq!(
            args("tag -m 'v1 notes' v1"),
            ["tag", "-m", "v1 notes", "v1"]
        );
    }

    #[test]
    fn shell_operators_are_plain_arguments() {
        // Nothing reaches a shell: `&&` is an argument git will reject.
        assert_eq!(
            args("status && rm -rf x"),
            ["status", "&&", "rm", "-rf", "x"]
        );
        assert_eq!(args("log $(whoami)"), ["log", "$(whoami)"]);
    }

    #[test]
    fn nothing_to_run_is_empty() {
        assert_eq!(parse_git_command(""), Err(GitCommandError::Empty));
        assert_eq!(parse_git_command("   "), Err(GitCommandError::Empty));
        assert_eq!(parse_git_command("git"), Err(GitCommandError::Empty));
    }

    #[test]
    fn an_open_quote_is_unbalanced() {
        assert_eq!(
            parse_git_command("commit -m \"oops"),
            Err(GitCommandError::Unbalanced)
        );
        assert_eq!(
            parse_git_command("log \\"),
            Err(GitCommandError::Unbalanced)
        );
    }

    #[test]
    fn a_title_names_the_command_once() {
        assert_eq!(command_title("status"), "git status");
        assert_eq!(command_title("  git status "), "git status");
        assert_eq!(command_title("gitk"), "git gitk");
        let long = command_title(&format!("log {}", "x".repeat(100)));
        assert_eq!(long.chars().count(), 61);
        assert!(long.starts_with("git log xxx") && long.ends_with('…'));
    }

    #[test]
    fn output_is_stdout_then_stderr() {
        assert_eq!(command_output("a\nb\n", "warn\n"), "a\nb\nwarn");
        assert_eq!(command_output("only\n\n", ""), "only");
        assert_eq!(command_output("", "To origin\n"), "To origin");
        assert_eq!(command_output("\n", " \n"), "");
    }

    #[test]
    fn short_output_is_kept_whole() {
        assert_eq!(clip_output("a\nb\nc", 3, 100), ("a\nb\nc", false));
        assert_eq!(clip_output("a\nb\nc\n", 3, 100), ("a\nb\nc", false));
        assert_eq!(clip_output("", 3, 100), ("", false));
    }

    #[test]
    fn long_output_keeps_its_first_lines() {
        assert_eq!(clip_output("a\nb\nc\nd", 2, 100), ("a\nb", true));
        assert_eq!(clip_output("a\nb\nc\nd", 0, 100), ("", true));
    }

    #[test]
    fn wide_output_is_cut_on_a_character_boundary() {
        assert_eq!(clip_output("abcdef", 10, 4), ("abcd", true));
        // `é` is two bytes: a cut inside it backs off to before it.
        assert_eq!(clip_output("aé", 10, 2), ("a", true));
    }
}
