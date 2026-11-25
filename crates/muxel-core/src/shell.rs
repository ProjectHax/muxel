//! The POSIX parser [`split_words`] is a byte-for-byte port of `Shell.splitWords`
//! in `ios/Muxel/Util/Shell.swift`. Keep the two in lockstep (see the protocol
//! contract in `ios/README.md`). [`join_words`] uses POSIX shell quoting.
//! The desktop preset editor and Git command dialog also support Windows paths.

use crate::ssh::sh_quote;

/// Split a command line into shell words. Space/tab/CR/LF separate (exactly
/// those four — not all Unicode whitespace, matching Rust's behavior the iOS
/// port mirrors); single quotes are literal to the next `'`; double quotes
/// group, with backslash escaping `\` and `"` inside (any other backslash is
/// kept literally); a backslash outside quotes escapes the next character.
/// Adjacent segments concatenate into one word (`a"b c"` → `ab c`) and `''`
/// yields an empty word. `None` on an unbalanced quote or trailing backslash.
pub fn split_words(line: &str) -> Option<Vec<String>> {
    split(line, false)
}

/// Split a preset's extra arguments on the editor's platform. On Windows
/// both quote styles group words, but backslashes are ordinary path characters
/// except before a double quote outside single quotes. There, each pair gives
/// one backslash and an odd leftover makes the quote literal.
pub fn split_preset_args(line: &str) -> Option<Vec<String>> {
    split(line, cfg!(windows))
}

pub(super) fn split(line: &str, windows_paths: bool) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();

    fn flush(words: &mut Vec<String>, current: &mut String, in_word: &mut bool) {
        if *in_word {
            words.push(std::mem::take(current));
            *in_word = false;
        }
    }

    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' | '\r' => flush(&mut words, &mut current, &mut in_word),
            '\'' => {
                in_word = true;
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '\'' {
                        closed = true;
                        break;
                    }
                    current.push(c);
                }
                if !closed {
                    return None;
                }
            }
            '"' => {
                in_word = true;
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '\\' if windows_paths => {
                            push_windows_backslashes(&mut current, &mut chars);
                        }
                        '\\' => {
                            let escaped = chars.next()?;
                            if escaped == '"' || escaped == '\\' {
                                current.push(escaped);
                            } else {
                                // POSIX keeps the backslash when it doesn't
                                // escape anything special inside double quotes.
                                current.push('\\');
                                current.push(escaped);
                            }
                        }
                        '"' => {
                            closed = true;
                            break;
                        }
                        _ => current.push(c),
                    }
                }
                if !closed {
                    return None;
                }
            }
            '\\' => {
                in_word = true;
                if windows_paths {
                    push_windows_backslashes(&mut current, &mut chars);
                } else {
                    current.push(chars.next()?);
                }
            }
            _ => {
                in_word = true;
                current.push(ch);
            }
        }
    }
    flush(&mut words, &mut current, &mut in_word);
    Some(words)
}

/// The first backslash has already been consumed. Leave an unescaped quote
/// for the caller to open or close a double-quoted segment.
fn push_windows_backslashes(
    word: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) {
    let mut count = 1;
    while chars.next_if_eq(&'\\').is_some() {
        count += 1;
    }
    if chars.peek() == Some(&'"') {
        word.extend(std::iter::repeat_n('\\', count / 2));
        if count % 2 == 1 {
            chars.next();
            word.push('"');
        }
    } else {
        word.extend(std::iter::repeat_n('\\', count));
    }
}

/// Join argv words into a single line that [`split_words`] parses back to the
/// same words (each one [`sh_quote`]d; bare-safe words stay unquoted).
pub fn join_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| sh_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render saved arguments for editing with [`split_preset_args`].
pub fn join_preset_args(words: &[String]) -> String {
    if cfg!(windows) {
        join_windows_words(words)
    } else {
        join_words(words)
    }
}

fn join_windows_words(words: &[String]) -> String {
    let mut line = String::new();
    for word in words {
        if !line.is_empty() {
            line.push(' ');
        }
        if !word.is_empty() && !word.contains([' ', '\t', '\r', '\n', '\'', '"']) {
            line.push_str(word);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for c in word.chars() {
            if c == '"' {
                line.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            line.push(c);
            backslashes = if c == '\\' { backslashes + 1 } else { 0 };
        }
        // Double the final run so the closing quote still closes.
        line.extend(std::iter::repeat_n('\\', backslashes));
        line.push('"');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{join_words, split_words};

    fn words(items: &[&str]) -> Option<Vec<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    // Quote-free input must match `split_whitespace` (and the iOS port).
    #[test]
    fn plain_words_match_split_whitespace() {
        assert_eq!(
            split_words("claude --model opus"),
            words(&["claude", "--model", "opus"])
        );
        assert_eq!(split_words("  a   b\tc  "), words(&["a", "b", "c"]));
        assert_eq!(split_words(""), words(&[]));
        assert_eq!(split_words("   \t "), words(&[]));
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            split_words("echo 'hello world'"),
            words(&["echo", "hello world"])
        );
        assert_eq!(
            split_words("'has \"double\" quotes'"),
            words(&["has \"double\" quotes"])
        );
        // No escapes inside single quotes: backslash is a literal character.
        assert_eq!(split_words(r"'a\nb'"), words(&[r"a\nb"]));
    }

    #[test]
    fn double_quotes_with_escapes() {
        assert_eq!(
            split_words(r#"say "hi there""#),
            words(&["say", "hi there"])
        );
        assert_eq!(
            split_words(r#""a \"quoted\" word""#),
            words(&[r#"a "quoted" word"#])
        );
        assert_eq!(split_words(r#""back\\slash""#), words(&[r"back\slash"]));
        // A backslash that escapes nothing special stays literal (POSIX).
        assert_eq!(split_words(r#""a\nb""#), words(&[r"a\nb"]));
    }

    #[test]
    fn empty_quoted_arg_is_a_word() {
        assert_eq!(split_words("prog ''"), words(&["prog", ""]));
        assert_eq!(split_words("prog \"\""), words(&["prog", ""]));
    }

    #[test]
    fn adjacent_segments_concatenate() {
        assert_eq!(split_words(r#"a"b c"d"#), words(&["ab cd"]));
        assert_eq!(split_words(r"'a'\''b'"), words(&["a'b"]));
    }

    #[test]
    fn backslash_outside_quotes() {
        assert_eq!(
            split_words(r"path\ with\ spaces"),
            words(&["path with spaces"])
        );
    }

    #[test]
    fn unbalanced_input_is_none() {
        assert_eq!(split_words("echo 'unclosed"), None);
        assert_eq!(split_words("echo \"unclosed"), None);
        assert_eq!(split_words("trailing\\"), None);
        assert_eq!(split_words("\"trailing in quotes\\"), None);
    }

    /// `split_words` must invert `join_words` for arbitrary words (spaces,
    /// quotes, unicode) — mirrors the iOS round-trip test.
    #[test]
    fn round_trip_with_join_words() {
        let cases: &[&[&str]] = &[
            &["claude", "--model", "opus"],
            &["echo", "hello world", "it's"],
            &["prog", "", "två ord", "a\"b", r"back\slash"],
        ];
        for case in cases {
            let ws: Vec<String> = case.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                split_words(&join_words(&ws)),
                Some(ws.clone()),
                "round-trip failed for {ws:?}"
            );
        }
    }

    #[test]
    fn windows_preset_quotes_preserve_paths_and_argument_boundaries() {
        assert_eq!(
            super::split(r#"-Command Write-Host \"hi\""#, true),
            words(&["-Command", "Write-Host", r#""hi""#]),
        );
        assert_eq!(
            super::split(r#"pre\\"two words" 'a\"b'"#, true),
            words(&[r"pre\two words", r#"a\"b"#]),
        );
        assert_eq!(
            super::split(
                r#"'be terse' "say \"yes\"" "some dir\\" \\host\share"#,
                true
            ),
            words(&["be terse", r#"say "yes""#, r"some dir\", r"\\host\share"]),
        );
        let argv = words(&[
            "",
            "with space",
            "it's",
            r#"a\"b"#,
            r"two slashes \\",
            r"\\host\shared folder",
        ])
        .unwrap();
        assert_eq!(split_words(&join_words(&argv)).unwrap(), argv);
        let line = super::join_windows_words(&argv);
        assert_eq!(super::split(&line, true).unwrap(), argv);
    }
}
