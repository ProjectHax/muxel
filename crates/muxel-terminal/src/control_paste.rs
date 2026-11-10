//! Provider input encodings for outside-control paste operations.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlPaste {
    Terminal,
    WindowsCodexUnconfigured,
    WindowsCodexUnverified,
    WindowsCodex,
}

impl ControlPaste {
    pub(crate) fn for_launch(program: &str, args: &[String], windows: bool) -> Self {
        let program = program.trim().rsplit(['/', '\\']).next().unwrap_or(program);
        if !windows
            || !["codex", "codex.exe", "codex.cmd", "codex.bat"]
                .iter()
                .any(|name| program.eq_ignore_ascii_case(name))
        {
            return Self::Terminal;
        }
        let mut disabled = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            let setting = if matches!(arg.as_str(), "--config" | "-c") {
                args.next().map(String::as_str)
            } else {
                arg.strip_prefix("--config=").or_else(|| {
                    arg.strip_prefix("-c")
                        .filter(|value| !value.is_empty())
                        .map(|value| value.strip_prefix('=').unwrap_or(value))
                })
            };
            if let Some((key, value)) = setting.and_then(|setting| setting.split_once('='))
                && key.trim() == "disable_paste_burst"
            {
                disabled = value.trim() == "true";
            }
        }
        if disabled {
            Self::WindowsCodexUnverified
        } else {
            Self::WindowsCodexUnconfigured
        }
    }

    pub(crate) fn with_version(self, output: &str) -> Self {
        if self != Self::WindowsCodexUnverified {
            return self;
        }
        if Self::supported_version(output) {
            Self::WindowsCodex
        } else {
            self
        }
    }

    /// Change paste behavior only when the matching native encoder is available.
    /// Literal argv and launches without verified support retain their settings.
    pub(crate) fn configure_launch(
        program: &str,
        args: &mut Vec<String>,
        windows: bool,
        literal: bool,
        version: &str,
    ) -> Self {
        if Self::for_launch(program, args, windows) == Self::Terminal {
            return Self::Terminal;
        }
        if !literal && Self::supported_version(version) {
            let index = args
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(args.len());
            args.splice(
                index..index,
                ["--config".into(), "disable_paste_burst=true".into()],
            );
        }
        Self::for_launch(program, args, windows).with_version(version)
    }

    fn supported_version(output: &str) -> bool {
        let version = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("codex-cli "));
        version
            .and_then(|version| {
                let mut parts = version.split('.');
                Some((
                    parts.next()?.parse::<u32>().ok()?,
                    parts.next()?.parse::<u32>().ok()?,
                ))
            })
            .is_some_and(|(major, minor)| major > 0 || minor >= 167)
    }

    /// Explicit Win32 input records preserve Unicode code units and Shift+Enter
    /// through both older and newer ConPTY parsers. Ordinary text conversion
    /// can drop combining marks, and unknown CSI passthrough varies by version.
    pub(crate) fn encode(self, text: &str) -> Result<Option<String>, String> {
        if self == Self::Terminal {
            return Ok(None);
        }
        if let Some(ch) = text
            .chars()
            .find(|ch| ch.is_control() && !matches!(ch, '\r' | '\n'))
        {
            return Err(format!(
                "Windows Codex cannot insert control character U+{:04X} as prompt text; only line breaks are supported. Nothing was typed",
                ch as u32
            ));
        }
        if self == Self::WindowsCodexUnconfigured {
            return if text.contains(['\r', '\n']) || !text.is_ascii() {
                Err("Reliable multiline or Unicode input requires a verified Codex 0.167+ launch with --config disable_paste_burst=true. Nothing was typed".into())
            } else {
                Ok(None)
            };
        }
        if self == Self::WindowsCodexUnverified && (text.contains(['\r', '\n']) || !text.is_ascii())
        {
            return Err("Multiline or Unicode input could not be verified for this Codex launch; Codex 0.167 or newer is required. Nothing was typed".into());
        }
        Ok(Some(Self::encode_records(text)))
    }

    /// Clipboard paste shares the configured decoder so disabling bursts does
    /// not drop its line breaks. Sanitize ESC as ordinary terminal paste does
    /// and expand tabs (see [`expand_tabs`]), then refuse the whole paste if any
    /// remaining character is unsupported. Runner and TypeIn startup text comes
    /// this way too.
    pub(crate) fn clipboard(self, text: &str) -> Result<Option<String>, String> {
        match self {
            Self::Terminal | Self::WindowsCodexUnconfigured => Ok(None),
            Self::WindowsCodex | Self::WindowsCodexUnverified => {
                self.encode(&expand_tabs(&text.replace('\x1b', "")))
            }
        }
    }

    fn encode_records(text: &str) -> String {
        use std::fmt::Write as _;
        let mut encoded = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    encoded.push_str("\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_");
                }
                '\n' => encoded.push_str("\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_"),
                ch => {
                    // Keep surrogate halves adjacent; the Windows reader pairs
                    // them before producing one character event.
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        let _ = write!(encoded, "\x1b[0;0;{unit};1;0;1_");
                    }
                    // Consecutive identical key-down records can coalesce into
                    // one repeat count that the provider does not expand.
                    // Surrogate pairs must remain adjacent and receive no
                    // releases: their parser produces a press for either kind.
                    if ch.len_utf16() == 1 {
                        let _ = write!(encoded, "\x1b[0;0;{};0;0;1_", ch as u32);
                    }
                }
            }
        }
        encoded
    }
}

/// Columns between tab stops: the editor's default tab width.
const TAB_STOP: usize = 4;

/// Each tab as spaces to its next stop, counting characters from the start of
/// its line. An input record carrying a tab reaches Codex as the Tab key, not
/// as text, so pasted code would otherwise be refused whole.
fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    let mut expanded = String::with_capacity(text.len());
    let mut column = 0;
    for ch in text.chars() {
        match ch {
            '\t' => {
                let pad = TAB_STOP - column % TAB_STOP;
                expanded.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            '\r' | '\n' => {
                expanded.push(ch);
                column = 0;
            }
            ch => {
                expanded.push(ch);
                column += 1;
            }
        }
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::ControlPaste;

    #[test]
    fn clipboard_tabs_reach_codex_as_spaces_to_the_next_stop() {
        let codex = ControlPaste::WindowsCodex;
        assert_eq!(
            codex
                .clipboard("\tif ok {\n\t\treturn\nab\tc\td\t")
                .unwrap(),
            codex
                .clipboard("    if ok {\n        return\nab  c   d   ")
                .unwrap(),
        );
        let unverified = ControlPaste::WindowsCodexUnverified;
        assert_eq!(
            unverified.clipboard("key\tvalue").unwrap(),
            unverified.clipboard("key value").unwrap(),
        );
        assert!(
            codex.encode("a\tb").is_err(),
            "ctl input refuses a tab rather than type text other than the caller's"
        );
    }

    #[test]
    fn native_protocol_requires_a_known_enabled_launch() {
        let enabled = ["--config".into(), "disable_paste_burst=true".into()];
        assert_eq!(
            ControlPaste::for_launch("codex", &enabled, true),
            ControlPaste::WindowsCodexUnverified
        );
        for (program, windows) in [("codex", false), ("ssh", true), ("cmd.exe", true)] {
            assert_eq!(
                ControlPaste::for_launch(program, &enabled, windows),
                ControlPaste::Terminal
            );
        }
        assert_eq!(
            ControlPaste::for_launch("codex", &[], true),
            ControlPaste::WindowsCodexUnconfigured
        );
        let overridden = [
            "-c".into(),
            "disable_paste_burst=true".into(),
            "--config=disable_paste_burst=false".into(),
        ];
        assert_eq!(
            ControlPaste::for_launch("codex", &overridden, true),
            ControlPaste::WindowsCodexUnconfigured
        );
        let literal = ["--".into(), "--config=disable_paste_burst=true".into()];
        assert_eq!(
            ControlPaste::for_launch("codex", &literal, true),
            ControlPaste::WindowsCodexUnconfigured
        );
        assert!(
            ControlPaste::WindowsCodexUnconfigured
                .encode("one\ntwo")
                .is_err()
        );
        assert!(ControlPaste::WindowsCodexUnconfigured.encode("é").is_err());
    }

    #[test]
    fn windows_codex_preserves_line_breaks_and_unicode_input_records() {
        let encoded = ControlPaste::WindowsCodex
            .encode("AA\r\n\n🛰️")
            .unwrap()
            .unwrap();
        assert_eq!(
            encoded,
            concat!(
                "\x1b[0;0;65;1;0;1_\x1b[0;0;65;0;0;1_",
                "\x1b[0;0;65;1;0;1_\x1b[0;0;65;0;0;1_",
                "\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_",
                "\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_",
                "\x1b[0;0;55357;1;0;1_\x1b[0;0;57072;1;0;1_\x1b[0;0;65039;1;0;1_\x1b[0;0;65039;0;0;1_"
            )
        );
        assert_eq!(
            ControlPaste::Terminal.encode("unchanged\t\n").unwrap(),
            None
        );
        assert_eq!(
            ControlPaste::WindowsCodex.clipboard("AA\r\n\n🛰️").unwrap(),
            Some(encoded)
        );
        assert_eq!(
            ControlPaste::WindowsCodex.encode("a\rb").unwrap(),
            ControlPaste::WindowsCodex.encode("a\nb").unwrap()
        );
        assert!(
            ControlPaste::Terminal
                .clipboard("first\nlast")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_launch_flag_alone_cannot_enable_an_unknown_decoder() {
        let unverified = ControlPaste::WindowsCodexUnverified;
        for version in ["", "codex-cli 0.166.0", "unrecognised version"] {
            assert!(unverified.with_version(version).encode("one\ntwo").is_err());
            assert!(unverified.with_version(version).encode("é").is_err());
        }
        assert_eq!(
            unverified.with_version("codex-cli 0.167.0"),
            ControlPaste::WindowsCodex
        );
        assert!(unverified.encode("one line").is_ok());
    }

    #[test]
    fn clipboard_refuses_unsupported_controls_atomically() {
        for control in ['\0', '\u{8}', '\u{7f}', '\u{85}'] {
            let result = ControlPaste::WindowsCodex.clipboard(&format!("prefix{control}suffix"));
            assert!(
                result.is_err(),
                "unsupported clipboard character {control:?} must return no payload"
            );
        }
    }

    #[test]
    fn automatic_setting_always_has_its_codec() {
        for program in [
            "codex",
            "codex.exe",
            "codex.cmd",
            "codex.bat",
            "ssh",
            "cmd.exe",
        ] {
            for windows in [false, true] {
                for literal in [false, true] {
                    for version in ["", "codex-cli 0.166.0", "unrecognized", "codex-cli 0.167.0"] {
                        for original in [
                            vec![],
                            vec!["-c".into(), "disable_paste_burst=false".into()],
                            vec!["--".into(), "literal task".into()],
                        ] {
                            let mut args = original.clone();
                            let codec = ControlPaste::configure_launch(
                                program, &mut args, windows, literal, version,
                            );
                            let native = ControlPaste::for_launch(program, &[], windows)
                                != ControlPaste::Terminal;
                            if !native || literal || version != "codex-cli 0.167.0" {
                                assert_eq!(
                                    args, original,
                                    "unverified or literal launch must retain its arguments"
                                );
                            } else {
                                assert_eq!(codec, ControlPaste::WindowsCodex);
                                assert_ne!(args, original);
                            }
                            if args != original {
                                assert_eq!(
                                    codec,
                                    ControlPaste::WindowsCodex,
                                    "automatically disabling detection requires its replacement encoder"
                                );
                                assert_eq!(
                                    ControlPaste::for_launch(program, &args, windows),
                                    ControlPaste::WindowsCodexUnverified
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compact_literal_setting_gets_its_codec() {
        let original = vec!["-cdisable_paste_burst=true".into()];
        let mut args = original.clone();
        let codec =
            ControlPaste::configure_launch("codex", &mut args, true, true, "codex-cli 0.167.0");
        assert_eq!(args, original, "literal arguments remain untouched");
        assert_eq!(
            codec,
            ControlPaste::WindowsCodex,
            "a supported explicit setting must not fall back to ordinary clipboard input"
        );
    }

    #[test]
    fn explicit_unverified_setting_cannot_lose_clipboard_text() {
        let args = ["--config".into(), "disable_paste_burst=true".into()];
        for version in ["", "codex-cli 0.166.0", "unrecognized"] {
            let codec = ControlPaste::for_launch("codex", &args, true).with_version(version);
            for text in ["first\nsecond", "café"] {
                assert!(
                    codec.clipboard(text).is_err(),
                    "no legacy fallback with disabled detection"
                );
            }
        }
    }

    #[test]
    fn editor_controls_are_refused_before_a_payload_is_returned() {
        for control in ['\0', '\t', '\u{8}', '\u{1b}', '\u{85}'] {
            assert!(
                ControlPaste::WindowsCodex
                    .encode(&format!("prefix{control}suffix"))
                    .is_err()
            );
        }
    }
}
