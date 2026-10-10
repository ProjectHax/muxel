//! Character cleanup for library text fields.

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Cleans a library text field before it is compared, stored or shown:
/// `\r\n` → `\n`, then drops Cc (except `\n`, `\t`), Cf (except ZWNJ/ZWJ)
/// and the variation selectors U+E0100–U+E01EF. Nothing is trimmed.
pub fn sanitize(input: &str) -> String {
    input
        .replace("\r\n", "\n")
        .chars()
        .filter(|&c| keep(c))
        .collect()
}

fn keep(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return true;
    }
    let control = c.is_control();
    let format =
        c.general_category() == GeneralCategory::Format && c != '\u{200C}' && c != '\u{200D}';
    let supplementary_selector = ('\u{E0100}'..='\u{E01EF}').contains(&c);
    !(control || format || supplementary_selector)
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn removes_cc_controls_and_normalizes_crlf() {
        assert_eq!(
            sanitize("a\u{1b}[31mb\u{7}c\r\nd\te\rf\u{85}g\u{9b}h\u{7f}"),
            "a[31mbc\nd\tefgh"
        );
    }

    #[test]
    fn removes_cf_and_supplementary_selectors_keeps_joiners() {
        assert_eq!(
            sanitize(
                "x\u{E0041}y\u{202E}z\u{E0100}w\u{200D}v\u{FE0F}u\u{200C}t\u{200B}s\u{2066}r\u{FEFF}q"
            ),
            "xyzw\u{200D}v\u{FE0F}u\u{200C}tsrq"
        );
    }

    #[test]
    fn crlf_first_then_lone_cr_removed() {
        assert_eq!(sanitize("\r\r\n"), "\n");
    }

    #[test]
    fn only_bidi_override_becomes_empty() {
        assert_eq!(sanitize("\u{202E}"), "");
    }

    #[test]
    fn keeps_zwnj_zwj_fe0f_spaces_and_tabs_without_trimming() {
        let kept = "  a\u{200C}b\u{200D}c\u{FE0F}\td \t ";
        assert_eq!(sanitize(kept), "  a\u{200C}b\u{200D}c\u{FE0F}\td \t ");
    }

    #[test]
    fn removes_soft_hyphen_word_joiner_and_language_tag() {
        assert_eq!(sanitize("a\u{AD}b\u{2060}c\u{E0001}d"), "abcd");
    }

    #[test]
    fn supplementary_selector_range_bounds() {
        // Range ends removed; the basic selector U+FE00 is kept.
        assert_eq!(sanitize("a\u{E0100}b\u{E01EF}c\u{FE00}d"), "abc\u{FE00}d");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(sanitize("Hello, world! 123"), "Hello, world! 123");
        assert_eq!(sanitize(""), "");
    }
}
