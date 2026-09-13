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
    match os {
        RemoteOs::Unix => {
            let mut c = format!("git -C {}", sh_quote(repo));
            for a in args {
                c.push(' ');
                c.push_str(&sh_quote(a));
            }
            c
        }
        RemoteOs::Windows => {
            let mut body = format!("& git -C {}", ps_quote(repo));
            for a in args {
                body.push(' ');
                body.push_str(&ps_quote(a));
            }
            // git signals failure by exit code, not a PowerShell error, so the
            // strict prelude alone would not surface it.
            body.push_str("; exit $LASTEXITCODE");
            ps(&body)
        }
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
                "cd {q} && (git ls-files --cached --others --exclude-standard 2>/dev/null \
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
                "Set-Location -LiteralPath {q}; \
                 $files = & git ls-files --cached --others --exclude-standard 2>$null; \
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
/// and make sure `<ignore_line>` is in `.gitignore`. Idempotent.
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
             && {{ grep -qxF {ign} .gitignore 2>/dev/null || printf '%s\\n' {ign} >> .gitignore; }}",
            root = sh_quote(root),
            dir = sh_quote(dir),
            file = sh_quote(rel_file),
            hdr = sh_quote(header),
            ign = sh_quote(ignore_line),
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
             && {{ grep -qxF {ign} .gitignore 2>/dev/null || printf '%s\\n' {ign} >> .gitignore; }}",
            root = sh_quote(root),
            dir = sh_quote(dir),
            rel = sh_quote(rel_file),
            bak = sh_quote(rel_bak),
            ign = sh_quote(ignore_line),
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

/// The PowerShell half of "add this line to .gitignore unless it is already
/// there", shared by the two callers above.
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

#[cfg(test)]
mod tests {
    use super::{
        ensure_seeded_file, git, list_files, push_prep, read_file, scan_projects, test_dir,
        test_file, write_file,
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
        assert!(list_files(UNIX, "/srv/app/", 500).starts_with("cd /srv/app && (git ls-files"));
    }

    #[test]
    fn windows_commands_are_all_opaque_base64() {
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
            s.contains("git ls-files --cached --others --exclude-standard"),
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
