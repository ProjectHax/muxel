//! Building command lines for a **remote Windows host** reached over OpenSSH.
//!
//! The hard part is not PowerShell, it is not knowing which shell will see the
//! command first. Windows sshd hands `ssh host -- <string>` to whatever
//! `DefaultShell` names: `cmd.exe` out of the box, PowerShell on plenty of
//! machines people actually use. cmd and PowerShell quote incompatibly, so any
//! quoting muxel emits is parsed by an unknown shell before PowerShell sees it —
//! and a script that survives one is mangled by the other.
//!
//! So muxel never relies on that outer parse. Its own commands go out as
//! `powershell.exe -NoProfile -NonInteractive -EncodedCommand <base64>`, whose
//! payload is `[A-Za-z0-9+/=]` and therefore identical after cmd.exe *or*
//! PowerShell has had a go at it. The quoting problem is removed rather than
//! solved, and `DefaultShell` stops mattering.
//!
//! `-NoProfile` for muxel's own commands is the Windows twin of the reasoning in
//! [`crate::ssh::tmux_path_prelude`]: a profile's startup output would land in
//! results muxel parses. Interactive panes are the opposite case and *do* load the
//! profile — see [`crate::ssh`] — because an agent needs the user's real `PATH`,
//! exactly as `login_shell_command` uses `-ilc` on Unix.

use base64::Engine as _;

/// Which shell an interactive pane on a Windows host runs. Only ever the pane:
/// muxel's own commands always go through `powershell.exe`, whatever this says,
/// because 5.1 is the one shell guaranteed to be present.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowsShell {
    /// Windows PowerShell 5.1 — always present.
    #[default]
    PowerShell,
    /// PowerShell 7+, if the user installed it.
    Pwsh,
    /// `cmd.exe`, for people who want it.
    Cmd,
}

impl WindowsShell {
    /// The executable muxel names on the remote host.
    pub fn exe(self) -> &'static str {
        match self {
            WindowsShell::PowerShell => "powershell.exe",
            WindowsShell::Pwsh => "pwsh.exe",
            WindowsShell::Cmd => "cmd.exe",
        }
    }

    /// Whether this shell understands PowerShell syntax (so `-EncodedCommand`
    /// and `Set-Location` apply). `cmd` gets its own, much smaller, treatment.
    pub fn is_powershell(self) -> bool {
        matches!(self, WindowsShell::PowerShell | WindowsShell::Pwsh)
    }
}

/// A PowerShell single-quoted string literal. Inside `'…'` PowerShell expands
/// nothing — no `$var`, no backtick escapes, no subexpressions — so the only
/// character needing attention is `'` itself, which is escaped by doubling.
///
/// This is why single quotes are used everywhere here rather than double: a path
/// like `C:\Users\me$env` or one holding a backtick is a literal, not an
/// injection point.
pub fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// A PowerShell array literal of quoted strings: `@('a','b')`, or `@()` when
/// empty. Used to pass an agent's argv through without a second round of word
/// splitting.
pub fn ps_string_array(items: &[String]) -> String {
    if items.is_empty() {
        return "@()".to_string();
    }
    let inner: Vec<String> = items.iter().map(|a| ps_quote(a)).collect();
    format!("@({})", inner.join(","))
}

/// `-EncodedCommand`'s argument: the script as UTF-16LE, base64'd.
///
/// PowerShell specifies UTF-16LE here, not UTF-8 — feeding it UTF-8 base64 yields
/// mojibake or a parse error rather than a clean failure, so the encoding is
/// load-bearing. No BOM: PowerShell does not want one and rejects some inputs
/// carrying it.
pub fn encoded_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The remote command string for one of **muxel's own** (non-interactive)
/// commands on a Windows host.
///
/// Always `powershell.exe`, never the host's chosen pane shell: this must work on
/// a stock machine, and 5.1 is the only shell guaranteed to be there.
/// `-NonInteractive` so a cmdlet that would prompt fails instead of hanging a
/// connection muxel is waiting on.
pub fn script_command(script: &str) -> String {
    format!(
        "powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
        encoded_command(script)
    )
}

/// Stop a PowerShell script at the first failure and turn a failing native
/// command into a non-zero exit, so callers can trust the exit status the way
/// they do with `sh`'s `&&` chains.
///
/// PowerShell's default is the opposite of `sh`: a cmdlet error is non-terminating
/// and `$?` from a native binary does not set the script's exit code. Without this
/// prelude a failed `New-Item` would let the next line run and the whole command
/// would still exit 0 — which muxel would read as success.
pub const STRICT_PRELUDE: &str = "$ErrorActionPreference='Stop'";

/// Wrap `body` so any error exits non-zero with the message on stderr. The
/// `exit 1` is what makes a failure visible to the caller's `status.success()`.
pub fn strict_script(body: &str) -> String {
    format!("{STRICT_PRELUDE}; try {{ {body} }} catch {{ Write-Error $_; exit 1 }}")
}

#[cfg(test)]
mod tests {
    use super::{
        WindowsShell, encoded_command, ps_quote, ps_string_array, script_command, strict_script,
    };
    use base64::Engine as _;

    fn decode(b64: &str) -> String {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn ps_quote_wraps_and_doubles_only_single_quotes() {
        assert_eq!(ps_quote("claude"), "'claude'");
        assert_eq!(ps_quote(r"C:\Users\me\proj"), r"'C:\Users\me\proj'");
        assert_eq!(ps_quote("with space"), "'with space'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("''"), "''''''");
    }

    #[test]
    fn ps_quote_leaves_expansion_characters_literal() {
        // Inside single quotes PowerShell expands none of these, so they need no
        // escaping — and must not get any, or the literal would change.
        for raw in [
            "$env:PATH",
            "$(Get-Process)",
            "back`tick",
            r"C:\temp\$x",
            "semi;colon && pipe|",
            "%USERPROFILE%",
        ] {
            let q = ps_quote(raw);
            assert_eq!(q, format!("'{raw}'"), "over-escaped {raw:?}");
        }
    }

    #[test]
    fn ps_string_array_shapes() {
        assert_eq!(ps_string_array(&[]), "@()");
        assert_eq!(ps_string_array(&["-p".to_string()]), "@('-p')");
        assert_eq!(
            ps_string_array(&["-p".to_string(), "be terse".to_string()]),
            "@('-p','be terse')"
        );
        assert_eq!(ps_string_array(&["it's".to_string()]), "@('it''s')");
    }

    #[test]
    fn encoded_command_is_utf16le_base64() {
        // Known vector: "A" is 0x41 0x00 in UTF-16LE → "QQA=".
        assert_eq!(encoded_command("A"), "QQA=");
        // And "hi" → 68 00 69 00.
        assert_eq!(encoded_command("hi"), "aABpAA==");
        assert_eq!(
            decode(&encoded_command("Set-Location 'C:\\x'")),
            "Set-Location 'C:\\x'"
        );
    }

    #[test]
    fn encoded_command_round_trips_non_ascii_and_quotes() {
        for s in [
            "echo 'héllo'",
            "Set-Location -LiteralPath 'C:\\Users\\Ryan\\Проект'",
            "Write-Output '日本語'",
            "$ErrorActionPreference='Stop'; exit 0",
        ] {
            assert_eq!(decode(&encoded_command(s)), s);
        }
    }

    #[test]
    fn encoded_payload_survives_any_outer_shell() {
        // The whole point: the argument carries nothing cmd.exe or PowerShell
        // would treat as syntax, so DefaultShell cannot corrupt it.
        let cmd = script_command(&strict_script("Get-ChildItem -LiteralPath 'C:\\a b'"));
        let payload = cmd.rsplit(' ').next().unwrap();
        assert!(
            payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
            "payload is not bare base64: {payload}"
        );
        // And it decodes back to the script we asked for.
        assert!(decode(payload).contains("Get-ChildItem -LiteralPath 'C:\\a b'"));
    }

    #[test]
    fn script_command_always_uses_powershell_5_and_no_profile() {
        let c = script_command("exit 0");
        assert!(c.starts_with("powershell.exe -NoProfile -NonInteractive -EncodedCommand "));
    }

    #[test]
    fn strict_script_stops_on_error_and_exits_nonzero() {
        let s = strict_script("New-Item -ItemType Directory x");
        assert!(s.starts_with("$ErrorActionPreference='Stop'"));
        assert!(s.contains("catch"));
        assert!(s.contains("exit 1"));
    }

    #[test]
    fn shell_exe_names() {
        assert_eq!(WindowsShell::default(), WindowsShell::PowerShell);
        assert_eq!(WindowsShell::PowerShell.exe(), "powershell.exe");
        assert_eq!(WindowsShell::Pwsh.exe(), "pwsh.exe");
        assert_eq!(WindowsShell::Cmd.exe(), "cmd.exe");
        assert!(WindowsShell::PowerShell.is_powershell());
        assert!(WindowsShell::Pwsh.is_powershell());
        assert!(!WindowsShell::Cmd.is_powershell());
    }
}
