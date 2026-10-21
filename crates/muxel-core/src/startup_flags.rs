//! Recognize guarded startup flag names without accepting alternate spellings.

use std::ffi::OsStr;

/// Match a dashed flag after ASCII case folding and removal of leading dashes.
/// Include suffixes so malformed forms can be refused rather than reach the GUI.
/// The caller decides which exact spellings it accepts and owns ctl boundaries.
pub fn names_flag(arg: &OsStr, name: &str) -> bool {
    let bytes = arg.as_encoded_bytes();
    let start = bytes.iter().take_while(|byte| **byte == b'-').count();
    let candidate = &bytes[start..];
    let name = name.trim_start_matches('-').as_bytes();
    start > 0
        && !name.is_empty()
        && candidate.len() >= name.len()
        && candidate[..name.len()].eq_ignore_ascii_case(name)
}

#[cfg(test)]
mod tests {
    use super::names_flag;
    use std::ffi::OsStr;

    #[test]
    fn case_and_dash_variants_still_name_the_guarded_flag() {
        for arg in [
            "--user-data-dir",
            "--USER-DATA-DIR",
            "-user-data-dir",
            "---user-data-dir",
            "--User-Data-Dir=dir",
            "--user-data-dir:dir",
        ] {
            assert!(names_flag(OsStr::new(arg), "--user-data-dir"), "{arg}");
        }
    }

    #[test]
    fn unrelated_arguments_do_not_match_guarded_names() {
        for arg in ["--settings", "--user-data", "project", "--user", "", "---"] {
            assert!(!names_flag(OsStr::new(arg), "--user-data-dir"), "{arg}");
        }
    }

    #[test]
    fn ordinary_positional_arguments_and_empty_names_do_not_match() {
        for arg in ["user-data-dir", "version", "versioned-project"] {
            for name in ["--user-data-dir", "--version"] {
                assert!(!names_flag(OsStr::new(arg), name), "{arg}");
            }
        }
        assert!(!names_flag(OsStr::new("--settings"), ""));
    }
}
