//! Formatting file pastes for one ordinary shell parse at an unquoted argument
//! boundary. Launch metadata selects syntax, not the current parser. Manual
//! shell switches and other input grammars are outside this contract. Unknown
//! Windows programs receive quoted text, without a shell-safety guarantee.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteShell {
    Posix,
    PowerShell,
    Cmd,
    UnknownWindows,
}

impl Default for PasteShell {
    fn default() -> Self {
        if cfg!(windows) {
            Self::UnknownWindows
        } else {
            Self::Posix
        }
    }
}

impl PasteShell {
    /// Recognize an explicitly launched shell; other programs use the caller's
    /// fallback. Never infer a remote parser from local ssh/tmux.
    pub fn for_program(program: Option<&str>, fallback: Self) -> Self {
        let name = program
            .unwrap_or_default()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default();
        let name = name.to_ascii_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "powershell" | "pwsh" => Self::PowerShell,
            "cmd" => Self::Cmd,
            "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash" => Self::Posix,
            _ => fallback,
        }
    }

    pub fn windows(shell: crate::winshell::WindowsShell) -> Self {
        match shell {
            crate::winshell::WindowsShell::Cmd => Self::Cmd,
            crate::winshell::WindowsShell::PowerShell | crate::winshell::WindowsShell::Pwsh => {
                Self::PowerShell
            }
        }
    }

    /// The configured host shell applies to a shell pane, not an agent's input.
    /// Explicit shell programs override it; other Windows programs receive text.
    pub fn for_remote_program(
        program: Option<&str>,
        os: crate::RemoteOs,
        configured_shell: crate::winshell::WindowsShell,
    ) -> Self {
        let fallback = match (os, program) {
            (crate::RemoteOs::Unix, _) => Self::Posix,
            (crate::RemoteOs::Windows, None) => Self::windows(configured_shell),
            (crate::RemoteOs::Windows, Some(_)) => Self::UnknownWindows,
        };
        Self::for_program(program, fallback)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastePathError {
    InvalidPath,
    UnsupportedCharacters,
    TrailingBackslash,
    NonUnicode,
}

/// Build the complete paste before any PTY write. A bad later path cannot leave
/// an earlier path in the input. Callers must reject non-Unicode paths first.
pub fn format_paths(shell: PasteShell, paths: &[&str]) -> Result<String, PastePathError> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let mut text = String::new();
    for path in paths {
        text.push(' ');
        text.push_str(&quote_path(shell, path)?);
    }
    text.push(' ');
    Ok(text)
}

pub fn quote_path(shell: PasteShell, path: &str) -> Result<String, PastePathError> {
    if path.is_empty()
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err(PastePathError::InvalidPath);
    }
    if matches!(shell, PasteShell::Cmd | PasteShell::UnknownWindows) && path.ends_with('\\') {
        // Closing quotes after a backslash differ between CRT consumers and
        // shell built-ins. Refuse rather than silently changing the path.
        return Err(PastePathError::TrailingBackslash);
    }
    let unsupported = match shell {
        PasteShell::Posix | PasteShell::PowerShell => false,
        PasteShell::Cmd => path.chars().any(|c| "\"%!".contains(c)),
        PasteShell::UnknownWindows => path.contains('"'),
    };
    if unsupported {
        return Err(PastePathError::UnsupportedCharacters);
    }
    Ok(match shell {
        PasteShell::Posix => crate::ssh::sh_quote(path),
        PasteShell::PowerShell => crate::winshell::ps_quote(path),
        PasteShell::Cmd | PasteShell::UnknownWindows => format!("\"{path}\""),
    })
}

#[cfg(test)]
mod tests {
    use super::{PastePathError, PasteShell, format_paths, quote_path};

    #[test]
    fn posix_shell_programs_override_windows_fallbacks() {
        for program in [
            "bash.exe",
            "sh",
            "zsh",
            "dash",
            "ksh",
            "ash",
            "/bin/bash",
            "C:\\Program Files\\Git\\bin\\bash.exe",
            "C:/Program Files/Git/usr/bin/SH.EXE",
            "C:\\portable\\Git\\bin\\BASH.EXE",
        ] {
            for fallback in [
                PasteShell::UnknownWindows,
                PasteShell::PowerShell,
                PasteShell::Cmd,
            ] {
                assert_eq!(
                    PasteShell::for_program(Some(program), fallback),
                    PasteShell::Posix,
                    "{program}"
                );
            }
        }
    }

    #[test]
    fn remote_windows_agent_format_is_independent_of_the_configured_shell() {
        use crate::winshell::WindowsShell;
        let path = "C:\\Users\\user!\\Temp\\100% report.png";
        for configured in [
            WindowsShell::PowerShell,
            WindowsShell::Pwsh,
            WindowsShell::Cmd,
        ] {
            let shell = PasteShell::for_remote_program(None, crate::RemoteOs::Windows, configured);
            assert_eq!(shell, PasteShell::windows(configured));
            for program in ["claude", "codex.exe", "muse", "custom-agent.cmd"] {
                let target = PasteShell::for_remote_program(
                    Some(program),
                    crate::RemoteOs::Windows,
                    configured,
                );
                assert_eq!(target, PasteShell::UnknownWindows);
                assert_eq!(
                    format_paths(target, &[path]).unwrap(),
                    format!(" \"{path}\" ")
                );
            }
            for (program, expected) in [
                ("powershell.exe", PasteShell::PowerShell),
                ("pwsh", PasteShell::PowerShell),
                ("CMD.EXE", PasteShell::Cmd),
                ("C:\\Program Files\\Git\\bin\\bash.exe", PasteShell::Posix),
            ] {
                assert_eq!(
                    PasteShell::for_remote_program(
                        Some(program),
                        crate::RemoteOs::Windows,
                        configured
                    ),
                    expected
                );
            }
            assert_eq!(
                PasteShell::for_remote_program(None, crate::RemoteOs::Unix, configured),
                PasteShell::Posix
            );
            assert_eq!(
                PasteShell::for_remote_program(Some("agent"), crate::RemoteOs::Unix, configured),
                PasteShell::Posix
            );
        }
    }

    #[test]
    fn paste_target_uses_explicit_shell_or_host_fallback() {
        for program in [
            "pwsh",
            "powershell.exe",
            "C:\\Program Files\\PowerShell\\7\\PWSH.EXE",
        ] {
            assert_eq!(
                PasteShell::for_program(Some(program), PasteShell::UnknownWindows),
                PasteShell::PowerShell
            );
        }
        assert_eq!(
            PasteShell::for_program(Some("C:/Windows/System32/CMD.EXE"), PasteShell::PowerShell),
            PasteShell::Cmd
        );
        for program in [
            None,
            Some("ssh"),
            Some("tmux"),
            Some("claude"),
            Some("powershell-proxy.exe"),
        ] {
            assert_eq!(
                PasteShell::for_program(program, PasteShell::Posix),
                PasteShell::Posix
            );
            assert_eq!(
                PasteShell::for_program(program, PasteShell::UnknownWindows),
                PasteShell::UnknownWindows
            );
        }
        assert_eq!(
            PasteShell::windows(crate::winshell::WindowsShell::Pwsh),
            PasteShell::PowerShell
        );
        assert_eq!(
            PasteShell::windows(crate::winshell::WindowsShell::Cmd),
            PasteShell::Cmd
        );
    }

    #[test]
    fn powershell_file_paste_keeps_dollars_backticks_and_apostrophes_literal() {
        for quote in ['\'', '\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'] {
            let path = format!("C:\\tmp\\dollar$and`tick{quote}file.txt");
            assert_eq!(
                quote_path(PasteShell::PowerShell, &path).unwrap(),
                format!("'C:\\tmp\\dollar$and`tick{quote}{quote}file.txt'")
            );
        }
    }

    #[test]
    fn windows_targets_preserve_ordinary_names_and_limit_only_their_own_syntax() {
        for c in ['"', '&', '|', '<', '>', '(', ')', '^', '%', '!'] {
            let path = format!("C:\\tmp\\before{c}after.txt");
            assert_eq!(
                quote_path(PasteShell::PowerShell, &path).unwrap(),
                crate::winshell::ps_quote(&path)
            );
        }
        for shell in [PasteShell::Cmd, PasteShell::UnknownWindows] {
            assert_eq!(
                quote_path(shell, "C:\\tmp\\a\"b"),
                Err(PastePathError::UnsupportedCharacters)
            );
            for path in [
                "C:\\Program Files (x86)\\tool.exe",
                "C:\\tmp\\report (1).pdf",
                "C:\\tmp\\Chris’s notes.png",
                "C:\\tmp\\a&b^c(d)'e.txt",
                "C:\\tmp\\$(echo literal)`‘’‚‛“”„‟.txt",
            ] {
                assert_eq!(quote_path(shell, path).unwrap(), format!("\"{path}\""));
            }
        }
        for path in ["C:\\tmp\\100%.txt", "C:\\tmp\\report!.txt"] {
            assert_eq!(
                quote_path(PasteShell::Cmd, path),
                Err(PastePathError::UnsupportedCharacters)
            );
            assert_eq!(
                quote_path(PasteShell::UnknownWindows, path).unwrap(),
                format!("\"{path}\"")
            );
        }
    }

    #[test]
    fn file_paste_refusal_is_atomic_across_the_batch() {
        assert_eq!(
            format_paths(PasteShell::Cmd, &["C:\\safe.txt", "C:\\bad%value%.txt"]),
            Err(PastePathError::UnsupportedCharacters)
        );
        assert_eq!(format_paths(PasteShell::Cmd, &[]).unwrap(), "");
        assert_eq!(
            format_paths(PasteShell::Cmd, &["C:\\a.txt", "C:\\b.txt"]).unwrap(),
            " \"C:\\a.txt\" \"C:\\b.txt\" "
        );
    }

    #[test]
    fn file_paste_rejects_control_empty_and_ambiguous_trailing_separator() {
        for shell in [
            PasteShell::Posix,
            PasteShell::PowerShell,
            PasteShell::Cmd,
            PasteShell::UnknownWindows,
        ] {
            for path in [
                "",
                "a\nb",
                "a\rb",
                "a\0b",
                "a\x1bb",
                "a\tb",
                "a\u{2028}b",
                "a\u{2029}b",
            ] {
                assert_eq!(quote_path(shell, path), Err(PastePathError::InvalidPath));
            }
            if matches!(shell, PasteShell::Cmd | PasteShell::UnknownWindows) {
                for path in ["C:\\", "C:\\space folder\\"] {
                    assert_eq!(
                        quote_path(shell, path),
                        Err(PastePathError::TrailingBackslash)
                    );
                }
            }
        }
        for path in ["C:\\", "C:\\space folder\\"] {
            assert_eq!(
                quote_path(PasteShell::PowerShell, path).unwrap(),
                crate::winshell::ps_quote(path)
            );
        }
    }

    #[test]
    fn posix_file_paste_keeps_existing_literal_quoting() {
        for path in [
            "/tmp/plain.txt",
            "/tmp/$(echo marker).txt",
            "`echo marker`.txt",
            "a'b & c",
        ] {
            assert_eq!(
                quote_path(PasteShell::Posix, path).unwrap(),
                crate::ssh::sh_quote(path)
            );
        }
    }

    // Reuse this test executable as a native argv recorder. --skip accepts a
    // value without affecting the selected fixture; no extra binary or Node is
    // required. The parent alone enables output, so a normal test run is inert.
    #[cfg(windows)]
    #[test]
    fn native_argv_fixture() {
        if std::env::var_os("MUXEL_PATH_PASTE_ARGV_FIXTURE").is_none() {
            return;
        }
        let args = std::env::args().collect::<Vec<_>>();
        let values = args
            .windows(2)
            .filter(|pair| pair[0] == "--skip")
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>();
        println!(
            "MUXEL_NATIVE_ARGS={}",
            serde_json::to_string(&values).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_parser_matrix_preserves_file_arguments() {
        use base64::Engine as _;
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        let exe = std::env::current_exe()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let fixture = "--exact path_paste::tests::native_argv_fixture --nocapture";
        let ps_paths = [
            "C:\\tmp\\plain.txt",
            "C:\\tmp\\space file.txt",
            "C:\\tmp\\dollar$literal.txt",
            "C:\\tmp\\back`tick.txt",
            "C:\\tmp\\O'Brien.txt",
            "C:\\tmp\\quote‘file.txt",
            "C:\\tmp\\quote’file.txt",
            "C:\\tmp\\quote‚file.txt",
            "C:\\tmp\\quote‛file.txt",
            "C:\\tmp\\double“”„‟file.txt",
            "C:\\tmp\\日本語.txt",
            "C:\\Program Files (x86)\\tool.exe",
            "C:\\tmp\\report (1).pdf",
            "C:\\tmp\\Chris’s notes.png",
            "C:\\tmp\\a&b^c(d)%!e.txt",
            "C:\\tmp\\$(echo literal).txt",
            "C:\\",
            "D:\\",
        ];
        let cmd_paths = [
            "C:\\tmp\\plain.txt",
            "C:\\tmp\\space file.txt",
            "C:\\tmp\\a&b^c(d)'e.txt",
            "C:\\tmp\\日本語.txt",
            "C:\\Program Files (x86)\\tool.exe",
            "C:\\tmp\\report (1).pdf",
            "C:\\tmp\\Chris’s notes.png",
            "C:\\tmp\\$(echo literal)`‘’‚‛“”„‟.txt",
        ];
        let arguments = |style, paths: &[&str]| {
            paths
                .iter()
                .map(|path| format!(" --skip {}", quote_path(style, path).unwrap()))
                .collect::<String>()
        };
        let decode = |output: &std::process::Output| -> Vec<String> {
            let text = String::from_utf8_lossy(&output.stdout);
            let encoded = text
                .split_once("MUXEL_NATIVE_ARGS=")
                .unwrap_or_else(|| {
                    panic!(
                        "missing native fixture output: {text}; stderr={}",
                        String::from_utf8_lossy(&output.stderr)
                    )
                })
                .1
                .lines()
                .next()
                .unwrap();
            serde_json::from_str(encoded).unwrap()
        };
        let trailing_space_path = "C:\\space folder\\";
        let encoded_trailing =
            base64::engine::general_purpose::STANDARD.encode(trailing_space_path);
        for parser in ["powershell.exe", "pwsh.exe"] {
            let mut paths = ps_paths.to_vec();
            // The literal is correct in both PowerShell versions. PS5.1's
            // later native argv marshalling corrupts a space + trailing '\\';
            // assert that native case on pwsh7 only. Drive roots work on both.
            if parser == "pwsh.exe" {
                paths.push(trailing_space_path);
            }
            let style = PasteShell::PowerShell;
            let script = format!(
                "if (({}) -cne ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_trailing}')))) {{ exit 2 }}; & {} {fixture}{}; exit $LASTEXITCODE",
                quote_path(style, trailing_space_path).unwrap(),
                crate::winshell::ps_quote(&exe),
                arguments(style, &paths)
            );
            let result = Command::new(parser)
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-EncodedCommand",
                    &crate::winshell::encoded_command(&script),
                ])
                .env("MUXEL_PATH_PASTE_ARGV_FIXTURE", "1")
                .creation_flags(0x08000000)
                .output();
            let output = match result {
                Err(error)
                    if parser == "pwsh.exe" && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    continue;
                }
                result => result.unwrap(),
            };
            assert!(
                output.status.success(),
                "{parser}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(decode(&output), paths, "{parser} / {style:?}");
        }
        for delayed in ["off", "on"] {
            let style = PasteShell::Cmd;
            let line = format!(
                "\"{} {fixture}{}\"",
                quote_path(PasteShell::Cmd, &exe).unwrap(),
                arguments(style, &cmd_paths)
            );
            let output = Command::new("cmd.exe")
                .args(["/d", &format!("/v:{delayed}"), "/s", "/c"])
                .raw_arg(line)
                .env("MUXEL_PATH_PASTE_ARGV_FIXTURE", "1")
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "cmd/{delayed}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(decode(&output), cmd_paths, "cmd/{delayed} / {style:?}");
        }
        let git_bash = crate::AgentPreset::git_bash().program.unwrap();
        let bash_paths = [
            "C:\\tmp\\report-$(printf MUXEL_SUBSTITUTION).txt",
            "C:\\tmp\\report-`printf MUXEL_BACKTICK`.txt",
            "C:\\tmp\\O'Brien & report (1).txt",
            "C:\\Program Files (x86)\\notes.txt",
            "C:\\tmp\\日本語.txt",
        ];
        let style = PasteShell::for_program(Some(&git_bash), PasteShell::UnknownWindows);
        let script = format!(
            "{} {fixture}{}",
            crate::ssh::sh_quote(&exe.replace('\\', "/")),
            arguments(style, &bash_paths)
        );
        let result = Command::new(&git_bash)
            .args(["--noprofile", "--norc", "-c", &script])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env("LC_ALL", "C.UTF-8")
            .env("MUXEL_PATH_PASTE_ARGV_FIXTURE", "1")
            .creation_flags(0x08000000)
            .output();
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Git Bash is not installed; native Bash fixture skipped");
            }
            result => {
                let output = result.unwrap();
                assert!(
                    output.status.success(),
                    "Git Bash: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(decode(&output), bash_paths, "Git Bash / {style:?}");
            }
        }
    }
}
