# Git reads and writes

Background polling and display commands run with optional git locking disabled.
They also set `diff.autoRefreshIndex=false`: porcelain `git diff` can otherwise
run `update-index --refresh` even with optional locks disabled. See the
[`git diff` configuration documentation](https://git.github.io/htmldocs/git-diff.html).
This includes status/change counts, diffs, branch labels, revision counts,
remote file listings, and library checks. Git may otherwise refresh the index during
`status`, briefly taking `index.lock` in a repository an agent is also using.

Local reads use the shared `git_read_command` / `read_only_git` policy. Reads at a
`RepoLoc` use `git_output`, which applies the same policy locally and passes
`--no-optional-locks -c diff.autoRefreshIndex=false` to git on an SSH host. Remote file listings also disable
optional locks. New display commands should use these read helpers.

Writes use `git_write_output` or an ordinary git command. Staging, committing,
merging, checking out branches, fetching/pulling/pushing, and creating or removing
worktrees retain git's normal locks. Disabling optional locks never bypasses the
required locks that protect these operations.
