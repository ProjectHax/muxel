//! Quote-aware shell-word splitting for user-typed command lines (a preset's
//! extra args, a custom command). Byte-for-byte port of the iOS companion's
//! `Shell.splitWords` (`ios/Muxel/Util/Shell.swift`) — the two must stay in
//! lockstep (see the protocol-contract table in `ios/README.md`) — and the
//! decoding inverse of [`crate::ssh::sh_quote`]-joined lines.

use crate::ssh::sh_quote;

/// Split a command line into shell words. Space/tab/CR/LF separate (exactly
/// those four — not all Unicode whitespace, matching Rust's behavior the iOS
/// port mirrors); single quotes are literal to the next `'`; double quotes
/// group, with backslash escaping `\` and `"` inside (any other backslash is
/// kept literally); a backslash outside quotes escapes the next character.
/// Adjacent segments concatenate into one word (`a"b c"` → `ab c`) and `''`
/// yields an empty word. `None` on an unbalanced quote or trailing backslash.
///
/// For quote-free input this is exactly `str::split_whitespace` over ASCII.
pub fn split_words(line: &str) -> Option<Vec<String>> {
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
                current.push(chars.next()?);
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

/// Join argv words into a single line that [`split_words`] parses back to the
/// same words (each one [`sh_quote`]d; bare-safe words stay unquoted). Used to
/// render a preset's saved args for editing, so `["-p", "be terse"]` round-trips
/// instead of silently re-splitting into three words.
pub fn join_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| sh_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
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
}
