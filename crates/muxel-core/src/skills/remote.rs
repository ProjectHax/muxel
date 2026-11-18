//! The remote command lines the skill manager runs over SSH, and the parsing of
//! their output.
//!
//! Like [`crate::remote_ops`], each function returns one string for `ssh … --`,
//! for a POSIX shell or Windows PowerShell (strict prelude, `-LiteralPath`
//! throughout). The app crate does the I/O.
//!
//! Scan output carries text from skill files the user may not control, so its
//! records are framed by a per-call random `nonce` the content can't forge.

use std::collections::BTreeMap;

use super::agents::HOME_ENV_VARS;
use super::inventory::ScanEntry;
use super::{MAX_SKILL_MD_BYTES, SCAN_DEPTH, SIDECAR_FILE, SKILL_FILE, join, norm, parent};
use crate::RemoteOs;
use crate::ssh::{sh_quote, tmux_path_prelude};
use crate::winshell::{ps_quote, script_command, strict_script};

fn ps(body: &str) -> String {
    script_command(&strict_script(body))
}

/// A Unix command line as `sh -c '…'`, so it runs whatever the user's login
/// shell is (fish, nushell, …): ssh hands the command to that shell, which then
/// only has to start `sh`. Windows command lines are returned unchanged.
pub fn for_host(os: RemoteOs, cmd: String) -> String {
    match os {
        RemoteOs::Unix => format!("sh -c {}", sh_quote(&cmd)),
        RemoteOs::Windows => cmd,
    }
}

fn sh_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| sh_quote(i))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ps_list(items: &[String]) -> String {
    if items.is_empty() {
        return "@()".to_string();
    }
    format!(
        "@({})",
        items
            .iter()
            .map(|i| ps_quote(i))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// What [`probe`] learned about a host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    pub home: String,
    pub env: BTreeMap<String, String>,
    /// The agent programs found on the host's `PATH`.
    pub programs: Vec<String>,
}

/// The host's home folder, the environment variables that move agent folders,
/// and which of `programs` are installed. Output lines are `HOME\t…`,
/// `ENV\t<name>\t<value>` and `PROG\t<name>`.
pub fn probe(os: RemoteOs, programs: &[&str]) -> String {
    let programs: Vec<String> = programs.iter().map(|p| p.to_string()).collect();
    match os {
        RemoteOs::Unix => {
            let mut s = format!("{}; printf 'HOME\\t%s\\n' \"$HOME\"", tmux_path_prelude());
            for v in HOME_ENV_VARS {
                s.push_str(&format!("; printf 'ENV\\t{v}\\t%s\\n' \"${{{v}-}}\""));
            }
            s.push_str(&format!(
                "; for p in {}; do command -v \"$p\" >/dev/null 2>&1 && printf 'PROG\\t%s\\n' \"$p\"; done; true",
                sh_list(&programs)
            ));
            s
        }
        RemoteOs::Windows => {
            let mut body = "Write-Output (\"HOME`t\" + $env:USERPROFILE)".to_string();
            for v in HOME_ENV_VARS {
                body.push_str(&format!(
                    "; Write-Output (\"ENV`t{v}`t\" + [Environment]::GetEnvironmentVariable('{v}'))"
                ));
            }
            body.push_str(&format!(
                "; foreach ($p in {}) {{ if (Get-Command $p -ErrorAction SilentlyContinue) {{ Write-Output (\"PROG`t\" + $p) }} }}",
                ps_list(&programs)
            ));
            ps(&body)
        }
    }
}

/// Parse [`probe`]'s output; `None` without a home folder.
pub fn parse_probe(out: &str) -> Option<Probe> {
    let mut p = Probe::default();
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        let mut parts = line.splitn(3, '\t');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("HOME"), Some(h), None) => p.home = norm(h.trim()),
            (Some("ENV"), Some(k), Some(v)) if !v.trim().is_empty() => {
                p.env.insert(k.to_string(), v.trim().to_string());
            }
            (Some("PROG"), Some(name), None) => p.programs.push(name.trim().to_string()),
            _ => {}
        }
    }
    (!p.home.is_empty()).then_some(p)
}

/// One request to [`scan`]: which folders to search for skills, which folders
/// to report the existence of (agent homes), and which small files to read
/// (the lock, Hermes's manifest).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanRequest {
    pub roots: Vec<String>,
    pub test_dirs: Vec<String>,
    pub files: Vec<String>,
}

/// What [`scan`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanOutput {
    pub entries: Vec<ScanEntry>,
    /// The `test_dirs` that exist.
    pub existing: Vec<String>,
    /// The `files` that were read, by path.
    pub files: BTreeMap<String, String>,
}

/// Search `roots` for skills (`SKILL.md` up to [`SCAN_DEPTH`] folders down,
/// following symlinked skill folders), report which `test_dirs` exist, and
/// read `files`. Records are framed by `@@<nonce>@@` lines.
pub fn scan(os: RemoteOs, req: &ScanRequest, nonce: &str) -> String {
    let tag = format!("@@{nonce}@@");
    let md_cap = MAX_SKILL_MD_BYTES;
    let depth = SCAN_DEPTH + 1;
    match os {
        RemoteOs::Unix => {
            // Frontmatter only: from the first line through the closing `---`.
            let front = "awk '{sub(/\\r$/,\"\")} NR==1 && $0!=\"---\"{exit} {print} NR>1 && $0==\"---\"{exit}'";
            format!(
                "T={tag}; \
                 for d in {tests}; do [ -d \"$d\" ] && printf '%s DIR\\t%s\\n' \"$T\" \"$d\"; done; \
                 for f in {files}; do if [ -f \"$f\" ] && [ \"$(wc -c < \"$f\")\" -le {md_cap} ]; then printf '%s FILE\\t%s\\n' \"$T\" \"$f\"; cat \"$f\"; printf '\\n%s END\\n' \"$T\"; fi; done; \
                 for r in {roots}; do [ -d \"$r\" ] || continue; \
                 for l in \"$r\"/*; do if [ -L \"$l\" ] && [ ! -e \"$l\" ]; then printf '%s BROKEN\\t%s\\t%s\\t%s\\n' \"$T\" \"$r\" \"$l\" \"$(readlink \"$l\")\"; fi; done; \
                 find -L \"$r\" -mindepth 2 -maxdepth {depth} -name {skill} -type f 2>/dev/null | while IFS= read -r m; do \
                 s=$(dirname \"$m\"); printf '%s SKILL\\t%s\\t%s\\n' \"$T\" \"$r\" \"$s\"; \
                 if [ -L \"$s\" ]; then printf '%s LINK\\t%s\\n' \"$T\" \"$(readlink \"$s\")\"; fi; \
                 printf '%s MD\\n' \"$T\"; head -c {md_cap} \"$m\" | {front}; \
                 if [ -f \"$s\"/{sidecar} ]; then printf '\\n%s SIDECAR\\n' \"$T\"; head -c 65536 \"$s\"/{sidecar}; fi; \
                 printf '\\n%s END\\n' \"$T\"; done; done; true",
                tests = sh_list(&req.test_dirs),
                files = sh_list(&req.files),
                roots = sh_list(&req.roots),
                skill = SKILL_FILE,
                sidecar = SIDECAR_FILE,
            )
        }
        RemoteOs::Windows => {
            let t = ps_quote(&tag);
            ps(&format!(
                "$T={t}; \
                 foreach ($d in {tests}) {{ if (Test-Path -LiteralPath $d -PathType Container) {{ Write-Output \"$T DIR`t$d\" }} }}; \
                 foreach ($f in {files}) {{ $i = Get-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue; \
                 if ($i -and -not $i.PSIsContainer -and $i.Length -le {md_cap}) {{ Write-Output \"$T FILE`t$f\"; \
                 Write-Output ([System.IO.File]::ReadAllText($i.FullName)); Write-Output \"$T END\" }} }}; \
                 foreach ($r in {roots}) {{ if (-not (Test-Path -LiteralPath $r -PathType Container)) {{ continue }}; \
                 $root = (Get-Item -LiteralPath $r -Force).FullName.TrimEnd('\\'); \
                 Get-ChildItem -LiteralPath $r -Recurse -Depth {depth_ps} -Filter {skill} -File -Force -ErrorAction SilentlyContinue | ForEach-Object {{ \
                 $s = $_.Directory; $rel = $s.FullName.Substring($root.Length).TrimStart('\\'); \
                 if ($rel -eq '') {{ return }}; \
                 Write-Output (\"$T SKILL`t\" + $r + \"`t\" + $r + '/' + ($rel -replace '\\\\','/')); \
                 if ($s.LinkType) {{ Write-Output (\"$T LINK`t\" + ($s.Target | Select-Object -First 1)) }}; \
                 Write-Output \"$T MD\"; \
                 $lines = Get-Content -LiteralPath $_.FullName -TotalCount 400 -Encoding UTF8; \
                 if ($lines.Count -gt 0 -and $lines[0].TrimEnd() -eq '---') {{ Write-Output $lines[0]; \
                 for ($k = 1; $k -lt $lines.Count; $k++) {{ Write-Output $lines[$k]; if ($lines[$k].TrimEnd() -eq '---') {{ break }} }} }}; \
                 $sc = Join-Path $s.FullName {sidecar}; \
                 if (Test-Path -LiteralPath $sc -PathType Leaf) {{ Write-Output \"$T SIDECAR\"; Write-Output ([System.IO.File]::ReadAllText($sc)) }}; \
                 Write-Output \"$T END\" }} }}",
                tests = ps_list(&req.test_dirs),
                files = ps_list(&req.files),
                roots = ps_list(&req.roots),
                // `-Depth` counts folders below the start; the file sits one deeper.
                depth_ps = SCAN_DEPTH,
                skill = ps_quote(SKILL_FILE),
                sidecar = ps_quote(SIDECAR_FILE),
            ))
        }
    }
}

/// Parse [`scan`]'s output.
pub fn parse_scan(out: &str, nonce: &str) -> ScanOutput {
    let tag = format!("@@{nonce}@@ ");
    let mut res = ScanOutput::default();
    enum Body {
        None,
        File(String, String),
        Md,
        Sidecar,
    }
    let mut cur: Option<ScanEntry> = None;
    let mut body = Body::None;
    let mut md = String::new();
    let mut sidecar = String::new();
    let finish = |cur: &mut Option<ScanEntry>,
                  md: &mut String,
                  sidecar: &mut String,
                  res: &mut ScanOutput| {
        if let Some(mut e) = cur.take() {
            e.skill_md = (!md.trim().is_empty()).then(|| std::mem::take(md));
            e.sidecar = (!sidecar.trim().is_empty()).then(|| std::mem::take(sidecar));
            res.entries.push(e);
        }
        md.clear();
        sidecar.clear();
    };
    for raw in out.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let Some(rec) = line.strip_prefix(&tag) else {
            match &mut body {
                Body::File(_, text) => text.push_str(raw),
                Body::Md => md.push_str(raw),
                Body::Sidecar => sidecar.push_str(raw),
                Body::None => {}
            }
            continue;
        };
        let mut parts = rec.split('\t');
        match parts.next() {
            Some("DIR") => {
                if let Some(d) = parts.next() {
                    res.existing.push(norm(d));
                }
            }
            Some("FILE") => {
                if let Some(f) = parts.next() {
                    body = Body::File(norm(f), String::new());
                }
            }
            Some("BROKEN") => {
                let (Some(root), Some(link), target) = (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let Some(rel) = rel_below(root, link) else {
                    continue;
                };
                res.entries.push(ScanEntry {
                    root: norm(root),
                    link_target: Some(super::resolve_link(
                        parent(&norm(link)),
                        target.unwrap_or(""),
                    )),
                    broken: true,
                    rel,
                    ..Default::default()
                });
            }
            Some("SKILL") => {
                finish(&mut cur, &mut md, &mut sidecar, &mut res);
                body = Body::None;
                let (Some(root), Some(dir)) = (parts.next(), parts.next()) else {
                    continue;
                };
                if let Some(rel) = rel_below(root, dir) {
                    cur = Some(ScanEntry {
                        root: norm(root),
                        rel,
                        ..Default::default()
                    });
                }
            }
            Some("LINK") => {
                if let (Some(e), Some(t)) = (cur.as_mut(), parts.next()) {
                    let dir = join(&e.root, &e.rel);
                    e.link_target = Some(super::resolve_link(parent(&dir), t));
                }
            }
            Some("MD") => body = Body::Md,
            Some("SIDECAR") => body = Body::Sidecar,
            Some("END") => {
                if let Body::File(path, text) = std::mem::replace(&mut body, Body::None) {
                    res.files
                        .insert(path, text.trim_end_matches('\n').to_string());
                } else {
                    finish(&mut cur, &mut md, &mut sidecar, &mut res);
                }
            }
            _ => {}
        }
    }
    finish(&mut cur, &mut md, &mut sidecar, &mut res);
    res.entries = keep_skill_folders(res.entries);
    res
}

fn rel_below(root: &str, path: &str) -> Option<String> {
    let r = norm(root);
    let p = norm(path);
    p.strip_prefix(&format!("{r}/"))
        .map(str::to_string)
        .filter(|rel| !rel.is_empty())
}

/// Drop entries no agent would load: in a dot-folder, deeper than
/// [`SCAN_DEPTH`], or inside another skill's folder. Used for local scans too.
pub fn keep_skill_folders(mut entries: Vec<ScanEntry>) -> Vec<ScanEntry> {
    entries.retain(|e| {
        let parts: Vec<&str> = e.rel.split('/').collect();
        parts.len() <= SCAN_DEPTH && !parts.iter().any(|p| p.starts_with('.') || p.is_empty())
    });
    entries.sort_by(|a, b| {
        a.root
            .cmp(&b.root)
            .then(a.rel.split('/').count().cmp(&b.rel.split('/').count()))
            .then(a.rel.cmp(&b.rel))
    });
    let mut kept: Vec<ScanEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        let inside = kept
            .iter()
            .any(|k| k.root == e.root && e.rel.starts_with(&format!("{}/", k.rel)));
        if !inside {
            kept.push(e);
        }
    }
    kept
}

/// Unpack a tar stream (on stdin) as `<dir>/<name>`, replacing what's there
/// (a link is removed, not followed) only once the unpack succeeded. The
/// archive's entries are the skill's files at top level.
pub fn untar_into(os: RemoteOs, dir: &str, name: &str, nonce: &str) -> String {
    let dest = join(dir, name);
    let stage = join(dir, &format!(".{name}.muxel-new-{nonce}"));
    let old = join(dir, &format!(".{name}.muxel-old-{nonce}"));
    match os {
        RemoteOs::Unix => {
            let (d, s, o, t) = (
                sh_quote(dir),
                sh_quote(&stage),
                sh_quote(&old),
                sh_quote(&dest),
            );
            format!(
                "set -e; mkdir -p {d}; rm -rf {s}; mkdir {s}; \
                 if ! tar -xf - -C {s}; then rm -rf {s}; exit 1; fi; \
                 if [ -e {t} ] || [ -L {t} ]; then mv {t} {o}; fi; mv {s} {t}; rm -rf {o}"
            )
        }
        RemoteOs::Windows => {
            let tar = join(dir, &format!(".{name}.muxel-{nonce}.tar"));
            let (d, s, o, t, a) = (
                ps_quote(dir),
                ps_quote(&stage),
                ps_quote(&old),
                ps_quote(&dest),
                ps_quote(&tar),
            );
            ps(&format!(
                "New-Item -ItemType Directory -Force -Path {d} | Out-Null; \
                 $in = [Console]::OpenStandardInput(); $f = [System.IO.File]::Create({a}); $in.CopyTo($f); $f.Close(); \
                 if (Test-Path -LiteralPath {s}) {{ Remove-Item -LiteralPath {s} -Recurse -Force }}; \
                 New-Item -ItemType Directory -Path {s} | Out-Null; \
                 & tar.exe -xf {a} -C {s}; $code = $LASTEXITCODE; Remove-Item -LiteralPath {a} -Force; \
                 if ($code -ne 0) {{ Remove-Item -LiteralPath {s} -Recurse -Force; exit 1 }}; \
                 if (Test-Path -LiteralPath {o}) {{ Remove-Item -LiteralPath {o} -Recurse -Force }}; \
                 $cur = Get-Item -LiteralPath {t} -Force -ErrorAction SilentlyContinue; \
                 if ($cur) {{ if ($cur.LinkType) {{ $cur.Delete() }} else {{ Move-Item -LiteralPath {t} -Destination {o} }} }}; \
                 Move-Item -LiteralPath {s} -Destination {t}; \
                 if (Test-Path -LiteralPath {o}) {{ Remove-Item -LiteralPath {o} -Recurse -Force }}",
            ))
        }
    }
}

/// Make `<dir>/<name>` a symlink to `target` (relative to `dir`), replacing
/// what's there. Unix only: Windows hosts get copies ([`copy_into`]).
pub fn symlink(dir: &str, name: &str, target: &str) -> String {
    let link = join(dir, name);
    format!(
        "set -e; mkdir -p {d}; rm -rf {l}; ln -s {t} {l}",
        d = sh_quote(dir),
        l = sh_quote(&link),
        t = sh_quote(target),
    )
}

/// Copy the skill folder `from` to `<dir>/<name>`, replacing what's there.
pub fn copy_into(os: RemoteOs, from: &str, dir: &str, name: &str, nonce: &str) -> String {
    let dest = join(dir, name);
    let stage = join(dir, &format!(".{name}.muxel-new-{nonce}"));
    match os {
        RemoteOs::Unix => format!(
            "set -e; mkdir -p {d}; rm -rf {s}; cp -RL {f} {s}; rm -rf {t}; mv {s} {t}",
            d = sh_quote(dir),
            s = sh_quote(&stage),
            f = sh_quote(from),
            t = sh_quote(&dest),
        ),
        RemoteOs::Windows => {
            let (d, s, f, t) = (
                ps_quote(dir),
                ps_quote(&stage),
                ps_quote(from),
                ps_quote(&dest),
            );
            ps(&format!(
                "New-Item -ItemType Directory -Force -Path {d} | Out-Null; \
                 if (Test-Path -LiteralPath {s}) {{ Remove-Item -LiteralPath {s} -Recurse -Force }}; \
                 Copy-Item -LiteralPath {f} -Destination {s} -Recurse; \
                 {remove}; Move-Item -LiteralPath {s} -Destination {t}",
                remove = ps_remove(&t),
            ))
        }
    }
}

/// Move the skill folder `from` to `<dir>/<name>`, replacing what's there (a
/// link to `from` included).
pub fn move_into(os: RemoteOs, from: &str, dir: &str, name: &str) -> String {
    let dest = join(dir, name);
    match os {
        RemoteOs::Unix => format!(
            "set -e; mkdir -p {d}; if [ -e {t} ] || [ -L {t} ]; then rm -rf {t}; fi; mv {f} {t}",
            d = sh_quote(dir),
            t = sh_quote(&dest),
            f = sh_quote(from),
        ),
        RemoteOs::Windows => {
            let (d, f, t) = (ps_quote(dir), ps_quote(from), ps_quote(&dest));
            ps(&format!(
                "New-Item -ItemType Directory -Force -Path {d} | Out-Null; {remove}; \
                 Move-Item -LiteralPath {f} -Destination {t}",
                remove = ps_remove(&t),
            ))
        }
    }
}

/// PowerShell that deletes a path if present — a link itself rather than what
/// it points at (`Remove-Item -Recurse` on a 5.1 junction empties the target).
fn ps_remove(quoted: &str) -> String {
    format!(
        "$x = Get-Item -LiteralPath {quoted} -Force -ErrorAction SilentlyContinue; \
         if ($x) {{ if ($x.LinkType) {{ $x.Delete() }} else {{ Remove-Item -LiteralPath {quoted} -Recurse -Force }} }}"
    )
}

/// Delete `path` (a link itself, or a folder). The caller checks first that
/// `path` is a skill folder directly inside a known skills folder.
pub fn remove(os: RemoteOs, path: &str) -> String {
    match os {
        RemoteOs::Unix => format!("rm -rf {}", sh_quote(path)),
        RemoteOs::Windows => ps(&ps_remove(&ps_quote(path))),
    }
}

/// Write stdin to `path`, creating its folder.
pub fn write_file(os: RemoteOs, path: &str) -> String {
    let dir = parent(path);
    match os {
        RemoteOs::Unix => format!("mkdir -p {} && cat > {}", sh_quote(dir), sh_quote(path)),
        RemoteOs::Windows => ps(&format!(
            "New-Item -ItemType Directory -Force -Path {} | Out-Null; \
             $stdin = [Console]::OpenStandardInput(); $buf = New-Object System.IO.MemoryStream; \
             $stdin.CopyTo($buf); [System.IO.File]::WriteAllBytes({}, $buf.ToArray())",
            ps_quote(dir),
            ps_quote(path)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ScanRequest, copy_into, for_host, keep_skill_folders, move_into, parse_probe, parse_scan,
        probe, remove, scan, symlink, untar_into, write_file,
    };
    use crate::RemoteOs;
    use crate::skills::inventory::ScanEntry;
    use base64::Engine as _;

    fn decode_ps(cmd: &str) -> String {
        let b64 = cmd.rsplit(' ').next().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn unix_commands_run_under_sh() {
        assert_eq!(
            for_host(RemoteOs::Unix, "for d in a; do echo \"$d\"; done".into()),
            "sh -c 'for d in a; do echo \"$d\"; done'"
        );
        assert_eq!(
            for_host(RemoteOs::Windows, "powershell.exe x".into()),
            "powershell.exe x"
        );
    }

    #[test]
    fn probe_unix_and_parse() {
        let cmd = probe(RemoteOs::Unix, &["claude", "pi"]);
        assert!(cmd.starts_with("export PATH="));
        assert!(cmd.contains("for p in claude pi;"));
        assert!(cmd.contains("${CLAUDE_CONFIG_DIR-}"));
        let p = parse_probe(
            "HOME\t/home/u\nENV\tXDG_CONFIG_HOME\t/x\nENV\tGROK_HOME\t\nPROG\tclaude\n",
        )
        .unwrap();
        assert_eq!(p.home, "/home/u");
        assert_eq!(p.env.len(), 1);
        assert_eq!(p.programs, ["claude"]);
        assert_eq!(parse_probe("PROG\tx\n"), None);
    }

    #[test]
    fn probe_windows_is_encoded_and_quoted() {
        let cmd = probe(RemoteOs::Windows, &["claude"]);
        assert!(cmd.starts_with("powershell.exe -NoProfile -NonInteractive -EncodedCommand "));
        let script = decode_ps(&cmd);
        assert!(script.contains("foreach ($p in @('claude'))"));
        assert!(script.contains("$env:USERPROFILE"));
        let p = parse_probe("HOME\tC:\\Users\\u\r\nPROG\tclaude\r\n").unwrap();
        assert_eq!(p.home, "C:/Users/u");
    }

    #[test]
    fn scan_quotes_every_path() {
        let req = ScanRequest {
            roots: vec!["/h/it's/skills".into()],
            test_dirs: vec!["/h/.claude".into()],
            files: vec!["/h/.agents/.skill-lock.json".into()],
        };
        let cmd = scan(RemoteOs::Unix, &req, "n0nce");
        assert!(cmd.contains("for r in '/h/it'\\''s/skills';"));
        assert!(cmd.contains("T=@@n0nce@@;"));
        assert!(cmd.contains("-maxdepth 4 -name SKILL.md"));
        let win = decode_ps(&scan(RemoteOs::Windows, &req, "n0nce"));
        assert!(win.contains("foreach ($r in @('/h/it''s/skills'))"));
    }

    #[test]
    fn parses_scan_records_and_rejects_forged_frames() {
        let out = "\
@@N@@ DIR\t/h/.claude
@@N@@ FILE\t/h/.agents/.skill-lock.json
{\"version\":3}
@@N@@ END
@@N@@ SKILL\t/h/.claude/skills\t/h/.claude/skills/sec
@@N@@ LINK\t../../.agents/skills/sec
@@N@@ MD
---
name: sec
description: d
@@X@@ SKILL\t/h/.claude/skills\t/h/forged
---
@@N@@ SIDECAR
{\"source\":\"o/r\"}
@@N@@ END
@@N@@ BROKEN\t/h/.pi/agent/skills\t/h/.pi/agent/skills/gone\t/nowhere/gone
@@N@@ SKILL\t/h/.agents/skills\t/h/.agents/skills/sec/refs/inner
@@N@@ MD
---
@@N@@ END
";
        let res = parse_scan(out, "N");
        assert_eq!(res.existing, ["/h/.claude"]);
        assert_eq!(res.files["/h/.agents/.skill-lock.json"], "{\"version\":3}");
        // The nested `inner` is under a different root, so it survives the
        // nesting filter but `sec/refs/inner` is 3 deep: kept.
        let rels: Vec<(&str, &str)> = res
            .entries
            .iter()
            .map(|e| (e.root.as_str(), e.rel.as_str()))
            .collect();
        assert_eq!(
            rels,
            [
                ("/h/.agents/skills", "sec/refs/inner"),
                ("/h/.claude/skills", "sec"),
                ("/h/.pi/agent/skills", "gone"),
            ]
        );
        let sec = &res.entries[1];
        assert_eq!(sec.link_target.as_deref(), Some("/h/.agents/skills/sec"));
        let md = sec.skill_md.as_deref().unwrap();
        assert!(md.contains("name: sec"));
        assert!(md.contains("@@X@@ SKILL"), "forged frames stay content");
        assert_eq!(
            sec.sidecar.as_deref().map(str::trim),
            Some("{\"source\":\"o/r\"}")
        );
        let gone = &res.entries[2];
        assert!(gone.broken);
        assert_eq!(gone.link_target.as_deref(), Some("/nowhere/gone"));
    }

    #[test]
    fn keeps_only_loadable_skill_folders() {
        let e = |root: &str, rel: &str| ScanEntry {
            root: root.into(),
            rel: rel.into(),
            ..Default::default()
        };
        let kept = keep_skill_folders(vec![
            e("/r", "a/examples/b"),
            e("/r", "a"),
            e("/r", ".hidden"),
            e("/r", "x/.git/y"),
            e("/r", "c/d/e/f"),
            e("/r", "cat/skill"),
        ]);
        let rels: Vec<&str> = kept.iter().map(|k| k.rel.as_str()).collect();
        assert_eq!(rels, ["a", "cat/skill"]);
    }

    #[test]
    fn file_operations_replace_safely() {
        let u = untar_into(RemoteOs::Unix, "/h/.agents/skills", "x", "n");
        assert!(u.contains("tar -xf - -C /h/.agents/skills/.x.muxel-new-n"));
        assert!(u.contains("mv /h/.agents/skills/x /h/.agents/skills/.x.muxel-old-n"));
        let w = decode_ps(&untar_into(
            RemoteOs::Windows,
            "C:/u/.agents/skills",
            "x",
            "n",
        ));
        assert!(w.contains("& tar.exe -xf 'C:/u/.agents/skills/.x.muxel-n.tar'"));
        assert!(w.contains("$cur.Delete()"));
        assert_eq!(
            symlink("/h/.claude/skills", "x", "../../.agents/skills/x"),
            "set -e; mkdir -p /h/.claude/skills; rm -rf /h/.claude/skills/x; ln -s ../../.agents/skills/x /h/.claude/skills/x"
        );
        assert!(
            copy_into(RemoteOs::Unix, "/a/x", "/b", "x", "n")
                .contains("cp -RL /a/x /b/.x.muxel-new-n")
        );
        assert!(move_into(RemoteOs::Unix, "/a/x", "/b", "x").ends_with("mv /a/x /b/x"));
        assert_eq!(remove(RemoteOs::Unix, "/b/x y"), "rm -rf '/b/x y'");
        assert!(decode_ps(&remove(RemoteOs::Windows, "C:/b/x")).contains("$x.Delete()"));
        assert_eq!(
            write_file(RemoteOs::Unix, "/h/.agents/.skill-lock.json"),
            "mkdir -p /h/.agents && cat > /h/.agents/.skill-lock.json"
        );
    }
}
