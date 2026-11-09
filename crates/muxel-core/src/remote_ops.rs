//! The remote command line for each operation muxel runs over SSH, for both
//! remote families.
//!
//! These used to be inline `format!`s in the app crate, where they could only be
//! exercised by talking to a real host. They are pure string construction, so
//! they live here and are unit-tested on both OSes instead — the app crate keeps
//! the I/O and the output parsing.
//!
//! Every function takes a [`RemoteOs`] and returns one string, to be passed as
//! the single argument after `ssh … --`. The Unix forms are the existing ones,
//! unchanged, so no current host changes behavior. The Windows forms go out
//! base64'd (see [`crate::winshell`]) and so never depend on the far side's
//! `DefaultShell`.
//!
//! Two rules hold throughout the Windows side:
//!
//! - **`-LiteralPath`, never `-Path`.** `-Path` treats `[` and `]` as wildcards,
//!   so a directory named `proj[old]` is simply not found, and a `?` silently
//!   matches something else.
//! - **`$ErrorActionPreference='Stop'` and an explicit non-zero exit**, via
//!   [`crate::winshell::strict_script`]. PowerShell's default is the opposite of
//!   `sh`'s `&&` chain: a failing cmdlet lets the next line run and the command
//!   still exits 0, which the caller would read as success.

use crate::RemoteOs;
use crate::ssh::sh_quote;
use crate::winshell::{ps_quote, script_command, strict_script};

/// Wrap a PowerShell body in the strict prelude and the encoded-command envelope.
fn ps(body: &str) -> String {
    script_command(&strict_script(body))
}

/// Trailing separators differ per family, but a remote root is stored with `/`
/// on both — PowerShell and git accept forward slashes on Windows.
fn root_of(root: &str) -> &str {
    root.trim_end_matches('/')
}

/// `git -C <repo> <args…>`.
///
/// The same binary on both sides; only the quoting differs.
pub fn git(os: RemoteOs, repo: &str, args: &[&str]) -> String {
    git_with_policy(os, repo, args, false)
}

/// Display reads avoid optional locks without requiring Git 2.15's CLI flag.
pub fn git_read_only(os: RemoteOs, repo: &str, args: &[&str]) -> String {
    git_with_policy(os, repo, args, true)
}

fn git_with_policy(os: RemoteOs, repo: &str, args: &[&str], read_only: bool) -> String {
    match os {
        RemoteOs::Unix => sh_git(repo, args, read_only),
        // git signals failure by exit code, not a PowerShell error, so the
        // strict prelude alone would not surface it.
        RemoteOs::Windows => ps(&format!(
            "{}; exit $LASTEXITCODE",
            ps_git(repo, args, read_only)
        )),
    }
}

const READ_ONLY_POLICY: &str = " -c diff.autoRefreshIndex=false";

/// `git -C <repo> <args…>` for `sh`.
fn sh_git(repo: &str, args: &[&str], read_only: bool) -> String {
    let (env, policy) = if read_only {
        ("GIT_OPTIONAL_LOCKS=0 ", READ_ONLY_POLICY)
    } else {
        ("", "")
    };
    let mut c = format!("{env}git{policy} -C {}", sh_quote(repo));
    for a in args {
        c.push(' ');
        c.push_str(&sh_quote(a));
    }
    c
}

/// `git -C <repo> <args…>` as PowerShell statements, git's exit code left in
/// `$LASTEXITCODE`.
fn ps_git(repo: &str, args: &[&str], read_only: bool) -> String {
    let (env, policy) = if read_only {
        ("$env:GIT_OPTIONAL_LOCKS='0'; ", READ_ONLY_POLICY)
    } else {
        ("", "")
    };
    let mut body = format!("{env}& git{policy} -C {}", ps_quote(repo));
    for a in args {
        body.push(' ');
        body.push_str(&ps_quote(a));
    }
    body
}

/// What [`branches`] prints, on a line of its own, when git failed to list
/// the branches of a repository. ssh's failures and sshpass's have exit codes
/// of their own (sshpass's 5 is a wrong password), so the code alone can't
/// say whose failure it was.
pub const BRANCHES_FAILED: &str = "muxel: git could not list the branches";

/// The local branches, one a line, `*` before HEAD's and a space before the
/// others; then the branch HEAD names on a line of its own, since git's list
/// has none for it before the first commit (exit 1 where HEAD names none).
/// One command, so one ssh for both. Where the listing fails in a repository
/// (a git older than 1.9 doesn't know `%(HEAD)`), git's message goes to
/// stderr and [`BRANCHES_FAILED`] to stdout; in a folder holding no
/// repository, or none at all, it prints nothing and succeeds. Telling the
/// two apart costs a `git rev-parse` after a failure only, run in the folder
/// rather than with `-C`: a git older than 1.8.5 has no `-C`, which fails the
/// listing before `%(HEAD)` does. On Windows the check's `Test-Path` loads a
/// module, which Windows PowerShell 5.1 reports as progress, and ssh's stderr
/// would get it as XML after git's message; progress is turned off.
pub fn branches(os: RemoteOs, repo: &str) -> String {
    const LIST: &[&str] = &[
        "for-each-ref",
        "--format=%(HEAD)%(refname:short)",
        "refs/heads/",
    ];
    const HEAD: &[&str] = &["symbolic-ref", "--quiet", "--short", "HEAD"];
    match os {
        RemoteOs::Unix => format!(
            "{} || {{ {{ cd {} && git rev-parse --git-dir; }} >/dev/null 2>&1 || exit 0; \
             echo {}; exit 2; }}; {}",
            sh_git(repo, LIST, true),
            sh_quote(repo),
            sh_quote(BRANCHES_FAILED),
            sh_git(repo, HEAD, true)
        ),
        RemoteOs::Windows => ps(&format!(
            "$ProgressPreference='SilentlyContinue'; {}; if ($LASTEXITCODE -ne 0) {{ \
             if (-not (Test-Path -LiteralPath {repo} -PathType Container)) {{ exit 0 }}; \
             Set-Location -LiteralPath {repo}; & git rev-parse --git-dir | Out-Null; \
             if ($LASTEXITCODE -ne 0) {{ exit 0 }}; {}; exit 2 }}; {}; exit $LASTEXITCODE",
            ps_git(repo, LIST, true),
            ps_quote(BRANCHES_FAILED),
            ps_git(repo, HEAD, true),
            repo = ps_quote(repo),
        )),
    }
}

/// List files under a project root: gitignore-aware via `git ls-files` when it is
/// a repo, else a bounded directory walk. Output is one path per line, relative
/// to the root; the caller joins them back on.
pub fn list_files(os: RemoteOs, root: &str, cap: usize) -> String {
    let root = root_of(root);
    match os {
        RemoteOs::Unix => {
            let q = sh_quote(root);
            format!(
                "cd {q} && (GIT_OPTIONAL_LOCKS=0 git -c diff.autoRefreshIndex=false ls-files --cached --others --exclude-standard 2>/dev/null \
                 || find . -type f -not -path '*/.git/*') | head -n {cap}"
            )
        }
        RemoteOs::Windows => {
            let q = ps_quote(root);
            // `git ls-files` is tried first for the same reason as on Unix: it
            // honors .gitignore, which the fallback walk cannot. Its failure is an
            // exit code, so test $LASTEXITCODE rather than catching.
            //
            // The walk's paths are trimmed back to root-relative and forward-
            // slashed so both branches emit the same shape and the caller has
            // one thing to parse.
            ps(&format!(
                "Set-Location -LiteralPath {q}; $env:GIT_OPTIONAL_LOCKS='0'; \
                 $files = & git -c diff.autoRefreshIndex=false ls-files --cached --others --exclude-standard 2>$null; \
                 if ($LASTEXITCODE -ne 0 -or $null -eq $files) {{ \
                 $root = (Get-Location).Path; \
                 $files = Get-ChildItem -LiteralPath $root -Recurse -File -Force \
                 -ErrorAction SilentlyContinue | \
                 Where-Object {{ $_.FullName -notlike '*\\.git\\*' }} | \
                 ForEach-Object {{ $_.FullName.Substring($root.Length).TrimStart('\\','/') \
                 -replace '\\\\','/' }} }}; \
                 $files | Select-Object -First {cap}"
            ))
        }
    }
}

/// Read a regular file, but only when it exists and is within `max_bytes`.
/// Prints nothing (and still succeeds) when it is missing or too big, matching
/// the Unix form the caller already handles.
pub fn read_file(os: RemoteOs, abs_path: &str, max_bytes: u64) -> String {
    match os {
        RemoteOs::Unix => {
            let p = sh_quote(abs_path);
            format!("if [ -f {p} ] && [ \"$(wc -c < {p})\" -le {max_bytes} ]; then cat {p}; fi")
        }
        RemoteOs::Windows => {
            let p = ps_quote(abs_path);
            // Bytes, start to finish. `Get-Content` would split the file into
            // lines and rejoin them with the host's newline — rewriting every LF
            // to CRLF — and `[Console]::Out` would re-encode the result through
            // whatever code page the console is in, mangling anything non-ASCII.
            // Reading and writing raw bytes leaves the file exactly as it is on
            // disk, which is what an editor round-trip requires.
            ps(&format!(
                "$f = Get-Item -LiteralPath {p} -ErrorAction SilentlyContinue; \
                 if ($f -and -not $f.PSIsContainer -and $f.Length -le {max_bytes}) {{ \
                 $bytes = [System.IO.File]::ReadAllBytes($f.FullName); \
                 $stdout = [Console]::OpenStandardOutput(); \
                 $stdout.Write($bytes, 0, $bytes.Length); $stdout.Flush() }}"
            ))
        }
    }
}

/// Overwrite a file with whatever the caller pipes to the command's stdin.
pub fn write_file(os: RemoteOs, abs_path: &str) -> String {
    match os {
        RemoteOs::Unix => format!("cat > {}", sh_quote(abs_path)),
        RemoteOs::Windows => {
            let p = ps_quote(abs_path);
            // The mirror of `read_file`: copy stdin's bytes through untouched.
            // `[Console]::In.ReadToEnd()` would decode through the console code
            // page first, and `Set-Content -Encoding UTF8` on PowerShell 5.1
            // would then add a BOM — corrupting the first line of every JSON and
            // source file muxel saves.
            ps(&format!(
                "$stdin = [Console]::OpenStandardInput(); \
                 $buf = New-Object System.IO.MemoryStream; \
                 $stdin.CopyTo($buf); \
                 [System.IO.File]::WriteAllBytes({p}, $buf.ToArray())"
            ))
        }
    }
}

/// Whether a path exists and is a directory. Exit status carries the answer.
pub fn test_dir(os: RemoteOs, dir: &str) -> String {
    match os {
        RemoteOs::Unix => format!("test -d {}", sh_quote(dir)),
        RemoteOs::Windows => format!(
            // Not the strict wrapper: a false answer is an expected result here,
            // not an error, and must exit non-zero without a stderr message.
            "powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
            crate::winshell::encoded_command(&format!(
                "if (Test-Path -LiteralPath {} -PathType Container) {{ exit 0 }} else {{ exit 1 }}",
                ps_quote(dir)
            ))
        ),
    }
}

/// Whether a path exists and is a regular file. Exit status carries the answer.
pub fn test_file(os: RemoteOs, path: &str) -> String {
    match os {
        RemoteOs::Unix => format!("test -f {}", sh_quote(path)),
        RemoteOs::Windows => format!(
            "powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
            crate::winshell::encoded_command(&format!(
                "if (Test-Path -LiteralPath {} -PathType Leaf) {{ exit 0 }} else {{ exit 1 }}",
                ps_quote(path)
            ))
        ),
    }
}

/// Create `<root>/<dir>`, seed `<root>/<dir>/<file>` with `header` when absent,
/// and make sure git ignores `<ignore_line>`, adding it to `.gitignore` when no
/// ignore source covers it yet. Idempotent.
pub fn ensure_seeded_file(
    os: RemoteOs,
    root: &str,
    dir: &str,
    rel_file: &str,
    header: &str,
    ignore_line: &str,
) -> String {
    let root = root_of(root);
    match os {
        RemoteOs::Unix => format!(
            "cd {root} && mkdir -p {dir} && {{ test -f {file} || printf '%s' {hdr} > {file}; }} \
             && {{ {ignore}; }}",
            root = sh_quote(root),
            dir = sh_quote(dir),
            file = sh_quote(rel_file),
            hdr = sh_quote(header),
            ignore = append_gitignore_sh(ignore_line),
        ),
        RemoteOs::Windows => ps(&format!(
            "Set-Location -LiteralPath {root}; \
             New-Item -ItemType Directory -Force -Path {dir} | Out-Null; \
             if (-not (Test-Path -LiteralPath {file} -PathType Leaf)) {{ \
             [System.IO.File]::WriteAllText((Join-Path (Get-Location).Path {file}), {hdr}, \
             (New-Object System.Text.UTF8Encoding $false)) }}; \
             {ignore}",
            root = ps_quote(root),
            dir = ps_quote(dir),
            file = ps_quote(rel_file),
            hdr = ps_quote(header),
            ignore = append_gitignore_ps(ignore_line),
        )),
    }
}

/// Prepare a root for a layout push: ensure `<dir>/` exists, back the current
/// file up, and keep `<dir>/` git-ignored.
pub fn push_prep(
    os: RemoteOs,
    root: &str,
    dir: &str,
    rel_file: &str,
    rel_bak: &str,
    ignore_line: &str,
) -> String {
    let root = root_of(root);
    match os {
        RemoteOs::Unix => format!(
            "cd {root} && mkdir -p {dir} && {{ test -f {rel} && cp -f {rel} {bak} || true; }} \
             && {{ {ignore}; }}",
            root = sh_quote(root),
            dir = sh_quote(dir),
            rel = sh_quote(rel_file),
            bak = sh_quote(rel_bak),
            ignore = append_gitignore_sh(ignore_line),
        ),
        RemoteOs::Windows => ps(&format!(
            "Set-Location -LiteralPath {root}; \
             New-Item -ItemType Directory -Force -Path {dir} | Out-Null; \
             if (Test-Path -LiteralPath {rel} -PathType Leaf) {{ \
             Copy-Item -LiteralPath {rel} -Destination {bak} -Force }}; \
             {ignore}",
            root = ps_quote(root),
            dir = ps_quote(dir),
            rel = ps_quote(rel_file),
            bak = ps_quote(rel_bak),
            ignore = append_gitignore_ps(ignore_line),
        )),
    }
}

/// The POSIX shell half of "add this line to .gitignore unless git already
/// ignores it", shared by the two callers above.
///
/// `git check-ignore` covers the ignore sources other than `.gitignore`, such
/// as `.git/info/exclude` and `core.excludesFile`. A missing git or a root
/// outside a repository makes it fail, so the line is appended as before.
fn append_gitignore_sh(ignore_line: &str) -> String {
    let ign = sh_quote(ignore_line);
    format!(
        "grep -qxF {ign} .gitignore 2>/dev/null \
         || git check-ignore -q -- {ign} 2>/dev/null \
         || printf '%s\\n' {ign} >> .gitignore"
    )
}

/// The PowerShell twin of [`append_gitignore_sh`].
///
/// Matched against the whole trimmed line, mirroring `grep -qxF`, so a
/// `.muxel/` entry is not considered present because some other line happens to
/// contain that text.
fn append_gitignore_ps(ignore_line: &str) -> String {
    let ign = ps_quote(ignore_line);
    format!(
        "$gi = '.gitignore'; \
         $have = (Test-Path -LiteralPath $gi -PathType Leaf) -and \
         ((Get-Content -LiteralPath $gi -ErrorAction SilentlyContinue | \
         ForEach-Object {{ $_.Trim() }}) -contains {ign}); \
         if (-not $have -and (Get-Command git -ErrorAction SilentlyContinue)) {{ \
         try {{ & git check-ignore -q -- {ign} 2>$null; $have = $LASTEXITCODE -eq 0 }} \
         catch {{ $have = $false }} }}; \
         if (-not $have) {{ Add-Content -LiteralPath $gi -Value {ign} }}"
    )
}

/// Find every project root under the user's home directory, by locating the
/// marker file each one carries. Prints one absolute marker path per line.
pub fn scan_projects(os: RemoteOs, marker_rel: &str, max_depth: usize, prune: &[&str]) -> String {
    match os {
        RemoteOs::Unix => {
            let names: Vec<String> = prune
                .iter()
                .map(|n| format!("-name {}", sh_quote(n)))
                .collect();
            format!(
                "find \"$HOME\" -maxdepth {max_depth} \\( {} \\) -prune -o -type f -path '*{marker}' -print 2>/dev/null",
                names.join(" -o "),
                marker = marker_rel,
            )
        }
        RemoteOs::Windows => {
            let prune_list = prune
                .iter()
                .map(|n| ps_quote(n))
                .collect::<Vec<_>>()
                .join(",");
            // Get-ChildItem has no -prune, so the heavy directories are filtered
            // out of the path instead. Without this the walk descends into every
            // node_modules on the machine and takes minutes.
            //
            // Emitted with forward slashes so the caller strips the same marker
            // suffix it strips on Unix.
            ps(&format!(
                "$prune = @({prune_list}); \
                 $marker = {marker}; \
                 Get-ChildItem -LiteralPath $HOME -Recurse -Depth {max_depth} -Force \
                 -Filter (Split-Path $marker -Leaf) -File -ErrorAction SilentlyContinue | \
                 ForEach-Object {{ $_.FullName -replace '\\\\','/' }} | \
                 Where-Object {{ $_.EndsWith($marker) }} | \
                 Where-Object {{ $p = $_; -not ($prune | Where-Object {{ $p -like ('*/' + $_ + '/*') }}) }}",
                marker = ps_quote(marker_rel),
            ))
        }
    }
}

/// Days a pasted file is kept on the host before the next paste clears it.
const PASTE_KEEP_DAYS: u32 = 7;

/// Store stdin as file `name` in muxel's paste folder on the host and print the
/// file's absolute path — the text pasted into a remote pane in place of a
/// clipboard image or a local file. An agent there can't read this machine's
/// clipboard or disk, but it can read a path on its own host.
///
/// The folder is the user's own: `~/.cache/muxel/paste`, owner-only, rather than a
/// shared `/tmp` directory another user could create first and then read every
/// screenshot pasted through it. On Windows it is the user's temp directory, which
/// is per-user already. Files older than [`PASTE_KEEP_DAYS`] are cleared on the way
/// in, so the folder doesn't grow forever.
///
/// `name` must already be a plain file name ([`paste_file_name`]).
pub fn upload_paste(os: RemoteOs, name: &str) -> String {
    match os {
        RemoteOs::Unix => format!(
            "umask 077 && d=\"$HOME/.cache/muxel/paste\" && mkdir -p \"$d\" && \
             {{ find \"$d\" -type f -mtime +{PASTE_KEEP_DAYS} -delete 2>/dev/null; true; }} && \
             f=\"$d\"/{} && cat > \"$f\" && printf '%s\\n' \"$f\"",
            sh_quote(name)
        ),
        RemoteOs::Windows => ps(&format!(
            "$d = Join-Path ([System.IO.Path]::GetTempPath()) 'muxel-paste'; \
             [void](New-Item -ItemType Directory -Force -Path $d); \
             Get-ChildItem -LiteralPath $d -File | \
             Where-Object {{ $_.LastWriteTime -lt (Get-Date).AddDays(-{PASTE_KEEP_DAYS}) }} | \
             Remove-Item -Force -ErrorAction SilentlyContinue; \
             $f = Join-Path $d {}; \
             $stdin = [Console]::OpenStandardInput(); \
             $buf = New-Object System.IO.MemoryStream; \
             $stdin.CopyTo($buf); \
             [System.IO.File]::WriteAllBytes($f, $buf.ToArray()); \
             Write-Output $f",
            ps_quote(name)
        )),
    }
}

/// The name a pasted file gets on the host: `unique`, then `original` reduced to
/// characters that need no quoting anywhere (`[A-Za-z0-9._-]`, the rest `_`), so the
/// path pasted into the prompt reads cleanly and an agent can't mistake part of it
/// for something else. The extension survives; an overlong stem is cut.
pub fn paste_file_name(unique: &str, original: &str) -> String {
    const MAX_STEM: usize = 60;
    let clean: String = original
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.');
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (clean, None),
    };
    let stem: String = stem.chars().take(MAX_STEM).collect();
    let stem = if stem.is_empty() { "file" } else { &stem };
    match ext {
        Some(ext) => format!("{unique}-{stem}.{ext}"),
        None => format!("{unique}-{stem}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BRANCHES_FAILED, branches, ensure_seeded_file, git, git_read_only, list_files,
        paste_file_name, push_prep, read_file, scan_projects, test_dir, test_file, upload_paste,
        write_file,
    };
    use crate::RemoteOs;
    use base64::Engine as _;

    /// Decode the `-EncodedCommand` payload of a Windows command back to script
    /// text, so the assertions below read against what PowerShell will actually
    /// run rather than an opaque blob.
    fn script_of(cmd: &str) -> String {
        let b64 = cmd.rsplit(' ').next().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    /// Every Windows command must be a bare `powershell.exe … -EncodedCommand
    /// <base64>`: nothing the outer cmd.exe/PowerShell could reinterpret.
    fn assert_opaque(cmd: &str) {
        assert!(
            cmd.starts_with("powershell.exe -NoProfile -NonInteractive -EncodedCommand "),
            "not an encoded command: {cmd}"
        );
        let payload = cmd.rsplit(' ').next().unwrap();
        assert!(
            payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
            "payload is not bare base64: {payload}"
        );
    }

    const WIN: RemoteOs = RemoteOs::Windows;
    const UNIX: RemoteOs = RemoteOs::Unix;

    #[test]
    fn unix_forms_are_unchanged() {
        // These are the exact strings the app built before this module existed;
        // a remote Linux host must see no difference at all.
        assert_eq!(git(UNIX, "/srv/app", &["status"]), "git -C /srv/app status");
        assert_eq!(write_file(UNIX, "/tmp/x"), "cat > /tmp/x");
        assert_eq!(test_dir(UNIX, "/srv/app"), "test -d /srv/app");
        assert_eq!(test_file(UNIX, "/srv/a b"), "test -f '/srv/a b'");
        assert!(read_file(UNIX, "/tmp/x", 2_000_000).starts_with("if [ -f /tmp/x ]"));
        assert!(list_files(UNIX, "/srv/app/", 500).starts_with(
            "cd /srv/app && (GIT_OPTIONAL_LOCKS=0 git -c diff.autoRefreshIndex=false ls-files"
        ));
    }

    #[test]
    fn branches_are_listed_and_head_named_in_one_command() {
        // The Unix form runs in the app's tests (`integrations`); here, that
        // Windows asks the same, in order, and says git failed only where a
        // repository is.
        let s = script_of(&branches(WIN, "C:/p"));
        let list = s.find("'for-each-ref'").expect("lists");
        let gone = s
            .find("if (-not (Test-Path -LiteralPath 'C:/p' -PathType Container)) { exit 0 }")
            .expect("no folder: no branch");
        let repo = s
            .find("Set-Location -LiteralPath 'C:/p'; & git rev-parse --git-dir | Out-Null")
            .expect("checks for a repository in the folder");
        let none = s[repo..]
            .find("{ exit 0 }")
            .expect("no repository: no branch")
            + repo;
        let said = s
            .find(&format!("'{BRANCHES_FAILED}'; exit 2"))
            .expect("marks git's failure");
        let head = s.find("'symbolic-ref'").expect("names HEAD");
        assert!(
            list < gone && gone < repo && repo < none && none < said && said < head,
            "{s}"
        );
        // A git older than 1.8.5 has no `-C`; the check mustn't need it.
        assert!(!s[list..said].contains(" -C "), "{s}");
        assert!(s[head..].contains("; exit $LASTEXITCODE"), "{s}");
    }

    /// [`branches`]' Windows form, run here by Windows PowerShell as ssh runs
    /// it on a Windows host, after `prelude` (a stand-in for that host's
    /// git): its exit code, stdout and stderr, trimmed.
    #[cfg(windows)]
    fn branches_in_windows_powershell(
        dir: &std::path::Path,
        prelude: &str,
    ) -> (i32, String, String) {
        use std::os::windows::process::CommandExt as _;
        let dir = dir.to_string_lossy().replace('\\', "/");
        let script = format!("{prelude}{}", script_of(&branches(WIN, &dir)));
        let out = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &crate::winshell::encoded_command(&script),
            ])
            // A folder outside any repository stays outside one.
            .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
        let code = out.status.code().unwrap_or(-1);
        (code, text(&out.stdout), text(&out.stderr))
    }

    /// Windows PowerShell 5.1 answers as the Unix form does, and where git
    /// can't list the branches, stderr holds what git said and nothing else:
    /// the notice shows it. (5.1 reports loading a cmdlet's module as progress,
    /// which a redirected stderr, as over ssh, gets as a block of XML.)
    #[cfg(windows)]
    #[test]
    fn windows_powershell_lists_branches_and_says_only_what_git_said() {
        let root = std::env::temp_dir().join(format!("muxel-branches-{}", uuid::Uuid::new_v4()));
        // Brackets for `-LiteralPath`, quotes for the quoting.
        let repo = root.join("repo [x] 'q' \u{2019}q\u{2019}");
        let plain = root.join("plain [x]");
        let gone = root.join("gone");
        std::fs::create_dir_all(&plain).unwrap();
        let repo_arg = repo.to_string_lossy().into_owned();
        for args in [
            vec!["init", "-q", "-b", "main", &repo_arg],
            vec![
                "-C",
                &repo_arg,
                "-c",
                "user.name=muxel",
                "-c",
                "user.email=muxel@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "one",
            ],
        ] {
            let ok = std::process::Command::new("git").args(&args).status();
            assert!(ok.is_ok_and(|s| s.success()), "git {args:?}");
        }
        // git dying at a field it doesn't know, as one older than 1.9 dies at
        // `%(HEAD)`; and git with no `-C`, as one older than 1.8.5.
        let old_git = "function git { $a = @($args | ForEach-Object { \
                       if ($_ -eq '--format=%(HEAD)%(refname:short)') \
                       { '--format=%(no-such-field)' } else { $_ } }); \
                       & git.exe @a; $global:LASTEXITCODE = $LASTEXITCODE }; ";
        let no_c_git = "function git { if ($args -contains '-C') \
                        { & git.exe --muxel-no-such-option } else { & git.exe @args }; \
                        $global:LASTEXITCODE = $LASTEXITCODE }; ";

        let (code, out, err) = branches_in_windows_powershell(&repo, "");
        assert_eq!(
            (code, out.lines().collect::<Vec<_>>()),
            (0, vec!["*main", "main"])
        );
        assert_eq!(err, "");
        let (code, out, err) = branches_in_windows_powershell(&repo, old_git);
        assert_eq!((code, out.as_str()), (2, BRANCHES_FAILED));
        assert_eq!(err, "fatal: unknown field name: no-such-field");
        let (code, out, err) = branches_in_windows_powershell(&repo, no_c_git);
        assert_eq!((code, out.as_str()), (2, BRANCHES_FAILED));
        assert!(
            err.contains("--muxel-no-such-option") && !err.contains("CLIXML"),
            "{err}"
        );
        // No repository, or no folder: no branch, and nothing said.
        for (dir, prelude) in [(&plain, ""), (&plain, no_c_git), (&gone, no_c_git)] {
            let (code, out, err) = branches_in_windows_powershell(dir, prelude);
            assert_eq!((code, out.as_str()), (0, ""), "{dir:?}: {err}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn windows_commands_are_all_opaque_base64() {
        assert_opaque(&branches(WIN, "C:/p"));
        assert_opaque(&git(WIN, "C:/p", &["status"]));
        assert_opaque(&list_files(WIN, "C:/p", 10));
        assert_opaque(&read_file(WIN, "C:/p/a.txt", 10));
        assert_opaque(&write_file(WIN, "C:/p/a.txt"));
        assert_opaque(&test_dir(WIN, "C:/p"));
        assert_opaque(&test_file(WIN, "C:/p/a.txt"));
        assert_opaque(&scan_projects(
            WIN,
            "/.muxel/workspace.json",
            7,
            &["node_modules"],
        ));
        assert_opaque(&ensure_seeded_file(
            WIN,
            "C:/p",
            ".muxel",
            ".muxel/MEMORY.md",
            "# h",
            ".muxel/",
        ));
        assert_opaque(&push_prep(
            WIN,
            "C:/p",
            ".muxel",
            ".muxel/workspace.json",
            ".muxel/workspace.bak.json",
            ".muxel/",
        ));
    }

    #[test]
    fn windows_paths_with_quotes_and_spaces_stay_literal() {
        // A single quote in a path is the one character that could break out of
        // a PowerShell literal, so it must arrive doubled.
        let s = script_of(&read_file(WIN, r"C:\Users\o'brien\a b.txt", 99));
        assert!(s.contains(r"'C:\Users\o''brien\a b.txt'"), "{s}");

        // And expansion syntax must survive as text, not be evaluated.
        let s = script_of(&test_file(WIN, r"C:\x\$env:PATH"));
        assert!(s.contains(r"'C:\x\$env:PATH'"), "{s}");
    }

    #[test]
    fn windows_uses_literal_path_everywhere() {
        // -Path would treat `[` and `]` in a project directory as wildcards.
        for cmd in [
            read_file(WIN, "C:/p[1]/a", 9),
            write_file(WIN, "C:/p[1]/a"),
            test_dir(WIN, "C:/p[1]"),
            test_file(WIN, "C:/p[1]/a"),
            list_files(WIN, "C:/p[1]", 5),
        ] {
            let s = script_of(&cmd);
            assert!(!s.contains(" -Path "), "used -Path: {s}");
        }
    }

    #[test]
    fn windows_git_propagates_its_exit_code() {
        // git reports failure with an exit code, which the strict prelude alone
        // would not turn into a failed command.
        let s = script_of(&git(WIN, "C:/p", &["rev-parse", "HEAD"]));
        assert!(s.contains("exit $LASTEXITCODE"), "{s}");
        assert!(s.contains("& git -C 'C:/p' 'rev-parse' 'HEAD'"), "{s}");
    }

    #[test]
    fn remote_git_reads_use_environment_policy_without_new_git_flags() {
        assert_eq!(
            git_read_only(UNIX, "/srv/a b", &["status", "--porcelain"]),
            "GIT_OPTIONAL_LOCKS=0 git -c diff.autoRefreshIndex=false -C '/srv/a b' status --porcelain"
        );
        let s = script_of(&git_read_only(WIN, "C:/a b", &["status"]));
        assert!(s.contains("$env:GIT_OPTIONAL_LOCKS='0'; & git -c diff.autoRefreshIndex=false -C 'C:/a b' 'status'"), "{s}");
        assert!(s.contains("exit $LASTEXITCODE"), "{s}");
        for cmd in [
            git(UNIX, "/srv/app", &["add", "."]),
            script_of(&git(WIN, "C:/p", &["add", "."])),
        ] {
            assert!(!cmd.contains("GIT_OPTIONAL_LOCKS"), "mutation: {cmd}");
        }
    }

    #[test]
    fn windows_scripts_stop_on_the_first_error() {
        for cmd in [
            write_file(WIN, "C:/p/a"),
            ensure_seeded_file(WIN, "C:/p", ".muxel", ".muxel/M.md", "#", ".muxel/"),
            push_prep(
                WIN,
                "C:/p",
                ".muxel",
                ".muxel/w.json",
                ".muxel/b.json",
                ".muxel/",
            ),
        ] {
            let s = script_of(&cmd);
            assert!(s.contains("$ErrorActionPreference='Stop'"), "{s}");
            assert!(s.contains("exit 1"), "{s}");
        }
    }

    /// A file muxel reads and writes back must be byte-identical. Both
    /// directions therefore move raw bytes: text APIs here would rewrite LF to
    /// CRLF, re-encode through the console code page, or add a UTF-8 BOM.
    #[test]
    fn windows_file_io_is_byte_exact_in_both_directions() {
        let w = script_of(&write_file(WIN, "C:/p/a.json"));
        assert!(w.contains("OpenStandardInput"), "{w}");
        assert!(w.contains("WriteAllBytes"), "{w}");
        assert!(
            !w.contains("Set-Content"),
            "text write would add a BOM: {w}"
        );
        assert!(!w.contains("ReadToEnd"), "text read re-encodes: {w}");

        let r = script_of(&read_file(WIN, "C:/p/a.json", 10));
        assert!(r.contains("ReadAllBytes"), "{r}");
        assert!(r.contains("OpenStandardOutput"), "{r}");
        assert!(
            !r.contains("Get-Content"),
            "Get-Content rewrites newlines: {r}"
        );
    }

    #[test]
    fn test_helpers_do_not_wrap_a_false_answer_as_an_error() {
        // "not a directory" is an answer, not a failure: it must exit non-zero
        // with no stderr, so the caller can tell it apart from a broken link.
        let s = script_of(&test_dir(WIN, "C:/p"));
        assert!(!s.contains("Write-Error"), "{s}");
        assert!(s.contains("exit 0") && s.contains("exit 1"), "{s}");
    }

    #[test]
    fn windows_list_files_prefers_git_then_walks() {
        let s = script_of(&list_files(WIN, "C:/p", 42));
        assert!(
            s.contains("$env:GIT_OPTIONAL_LOCKS='0';")
                && s.contains("git -c diff.autoRefreshIndex=false ls-files --cached --others --exclude-standard"),
            "{s}"
        );
        assert!(s.contains("Get-ChildItem"), "{s}");
        assert!(s.contains("Select-Object -First 42"), "{s}");
        // Relative, forward-slashed output so the caller parses one shape.
        assert!(s.contains("-replace"), "{s}");
    }

    #[test]
    fn windows_scan_prunes_heavy_directories() {
        let s = script_of(&scan_projects(
            WIN,
            "/.muxel/workspace.json",
            7,
            &["node_modules", ".git"],
        ));
        assert!(s.contains("'node_modules'") && s.contains("'.git'"), "{s}");
        assert!(s.contains("-Depth 7"), "{s}");
        assert!(s.contains("EndsWith($marker)"), "{s}");
    }

    #[test]
    fn gitignore_check_matches_a_whole_line() {
        // `grep -qxF`'s Windows twin: a line that merely *contains* `.muxel/`
        // must not count as already ignored.
        let s = script_of(&push_prep(
            WIN,
            "C:/p",
            ".muxel",
            ".muxel/w.json",
            ".muxel/b.json",
            ".muxel/",
        ));
        assert!(s.contains("-contains '.muxel/'"), "{s}");
        assert!(s.contains("$_.Trim()"), "{s}");
    }

    /// Both hosts ask git before appending, so a `.muxel/` that
    /// `.git/info/exclude` already ignores leaves `.gitignore` alone.
    #[test]
    fn gitignore_append_defers_to_git_check_ignore() {
        for cmd in [
            ensure_seeded_file(UNIX, "/p", ".muxel", ".muxel/M.md", "h", ".muxel/"),
            push_prep(
                UNIX,
                "/p",
                ".muxel",
                ".muxel/w.json",
                ".muxel/b.json",
                ".muxel/",
            ),
        ] {
            assert!(
                cmd.contains(
                    "grep -qxF .muxel/ .gitignore 2>/dev/null \
                     || git check-ignore -q -- .muxel/ 2>/dev/null \
                     || printf '%s\\n' .muxel/ >> .gitignore"
                ),
                "{cmd}"
            );
        }
        let s = script_of(&ensure_seeded_file(
            WIN,
            "C:/p",
            ".muxel",
            ".muxel/M.md",
            "h",
            ".muxel/",
        ));
        assert!(
            s.contains(
                "try { & git check-ignore -q -- '.muxel/' 2>$null; $have = $LASTEXITCODE -eq 0 } \
                 catch { $have = $false }"
            ),
            "{s}"
        );
        assert!(s.contains("Get-Command git"), "{s}");
    }

    #[test]
    fn a_paste_lands_in_an_owner_only_folder_and_prints_its_path() {
        let u = upload_paste(UNIX, "20261014-a1b2c3-image.png");
        assert!(u.starts_with("umask 077 && "), "{u}");
        assert!(u.contains("\"$HOME/.cache/muxel/paste\""), "{u}");
        assert!(u.contains("-mtime +7 -delete"), "{u}");
        assert!(
            u.ends_with(
                "f=\"$d\"/20261014-a1b2c3-image.png && cat > \"$f\" && printf '%s\\n' \"$f\""
            ),
            "{u}"
        );
        // A name that needs quoting is quoted, never spliced in raw.
        assert!(upload_paste(UNIX, "a b.png").contains("f=\"$d\"/'a b.png'"));

        let w = upload_paste(WIN, "x.png");
        assert_opaque(&w);
        let w = script_of(&w);
        assert!(w.contains("GetTempPath()"), "{w}");
        assert!(w.contains("Join-Path $d 'x.png'"), "{w}");
        assert!(w.contains("WriteAllBytes"), "binary-safe: {w}");
        assert!(w.contains("Write-Output $f"), "{w}");
    }

    #[test]
    fn pasted_file_names_need_no_quoting() {
        assert_eq!(paste_file_name("u1", "image.png"), "u1-image.png");
        assert_eq!(
            paste_file_name("u1", "Screen Shot 2026-10-14 at 3.04.05 PM.png"),
            "u1-Screen_Shot_2026-10-14_at_3.04.05_PM.png"
        );
        assert_eq!(
            paste_file_name("u1", "$(rm -rf ~).txt"),
            "u1-__rm_-rf___.txt"
        );
        assert_eq!(paste_file_name("u1", ".env"), "u1-env");
        assert_eq!(paste_file_name("u1", "Makefile"), "u1-Makefile");
        assert_eq!(paste_file_name("u1", "日本.jpg"), "u1-__.jpg");
        assert_eq!(paste_file_name("u1", ""), "u1-file");
        let long = format!("{}.png", "a".repeat(200));
        assert_eq!(
            paste_file_name("u1", &long),
            format!("u1-{}.png", "a".repeat(60))
        );
    }

    #[test]
    fn trailing_root_separator_is_ignored_on_both() {
        assert_eq!(
            list_files(UNIX, "/srv/app/", 5),
            list_files(UNIX, "/srv/app", 5)
        );
        assert_eq!(
            push_prep(WIN, "C:/p/", ".m", "a", "b", "i"),
            push_prep(WIN, "C:/p", ".m", "a", "b", "i")
        );
    }
}
