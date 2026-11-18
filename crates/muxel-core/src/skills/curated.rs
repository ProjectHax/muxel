//! Recommended skills offered as one-click installs.
//!
//! Each entry names a public repository and the skills to take from it. The
//! app crate supplies the translated title and blurb by `id`. Every repository
//! here was checked for a license; one without a LICENSE file is left out.

/// Which heading an entry is listed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CuratedCategory {
    Security,
    Workflow,
    Building,
    Documents,
}

impl CuratedCategory {
    pub const ALL: [CuratedCategory; 4] = [
        CuratedCategory::Security,
        CuratedCategory::Workflow,
        CuratedCategory::Building,
        CuratedCategory::Documents,
    ];
}

/// One recommended install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CuratedSkill {
    /// Stable id, also the app's key for its title and blurb.
    pub id: &'static str,
    /// `owner/repo` on GitHub.
    pub repo: &'static str,
    /// The skills (by frontmatter name) to install; empty means every skill
    /// the repository has.
    pub skills: &'static [&'static str],
    /// SPDX id of the license.
    pub license: &'static str,
    pub category: CuratedCategory,
}

impl CuratedSkill {
    /// The source string the install runs with.
    pub fn source(&self) -> String {
        self.repo.to_string()
    }

    /// The repository's web page.
    pub fn web_url(&self) -> String {
        format!("https://github.com/{}", self.repo)
    }
}

const CLOUDFLARE: &str = "cloudflare/security-audit-skill";
const TRAIL_OF_BITS: &str = "trailofbits/skills";
const SUPERPOWERS: &str = "obra/superpowers";
const ANTHROPIC: &str = "anthropics/skills";

const fn entry(
    id: &'static str,
    repo: &'static str,
    skills: &'static [&'static str],
    license: &'static str,
    category: CuratedCategory,
) -> CuratedSkill {
    CuratedSkill {
        id,
        repo,
        skills,
        license,
        category,
    }
}

use CuratedCategory::*;

/// The catalog, in display order.
pub const CATALOG: &[CuratedSkill] = &[
    entry(
        "security-audit",
        CLOUDFLARE,
        &["security-audit"],
        "MIT",
        Security,
    ),
    entry(
        "differential-review",
        TRAIL_OF_BITS,
        &["differential-review"],
        "CC-BY-SA-4.0",
        Security,
    ),
    entry(
        "supply-chain-risk-auditor",
        TRAIL_OF_BITS,
        &["supply-chain-risk-auditor"],
        "CC-BY-SA-4.0",
        Security,
    ),
    entry(
        "semgrep-rule-creator",
        TRAIL_OF_BITS,
        &["semgrep-rule-creator"],
        "CC-BY-SA-4.0",
        Security,
    ),
    // `using-superpowers` loads the rest, so the set installs as one.
    entry("superpowers", SUPERPOWERS, &[], "MIT", Workflow),
    entry(
        "systematic-debugging",
        SUPERPOWERS,
        &["systematic-debugging"],
        "MIT",
        Workflow,
    ),
    entry(
        "test-driven-development",
        SUPERPOWERS,
        &["test-driven-development"],
        "MIT",
        Workflow,
    ),
    entry(
        "brainstorming",
        SUPERPOWERS,
        &["brainstorming"],
        "MIT",
        Workflow,
    ),
    entry(
        "skill-creator",
        ANTHROPIC,
        &["skill-creator"],
        "Apache-2.0",
        Building,
    ),
    entry(
        "mcp-builder",
        ANTHROPIC,
        &["mcp-builder"],
        "Apache-2.0",
        Building,
    ),
    entry(
        "frontend-design",
        ANTHROPIC,
        &["frontend-design"],
        "Apache-2.0",
        Building,
    ),
    entry(
        "webapp-testing",
        ANTHROPIC,
        &["webapp-testing"],
        "Apache-2.0",
        Building,
    ),
    entry("pdf", ANTHROPIC, &["pdf"], "Apache-2.0", Documents),
    entry("docx", ANTHROPIC, &["docx"], "Apache-2.0", Documents),
    entry("xlsx", ANTHROPIC, &["xlsx"], "Apache-2.0", Documents),
    entry("pptx", ANTHROPIC, &["pptx"], "Apache-2.0", Documents),
];

/// The catalog entry with this id.
pub fn find(id: &str) -> Option<&'static CuratedSkill> {
    CATALOG.iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::{CATALOG, CuratedCategory, find};
    use crate::skills::frontmatter::valid_name;
    use crate::skills::source::parse;

    #[test]
    fn ids_are_unique_and_sources_parse() {
        for (i, c) in CATALOG.iter().enumerate() {
            assert!(CATALOG[..i].iter().all(|o| o.id != c.id), "{}", c.id);
            assert!(parse(&c.source()).is_ok(), "{}", c.repo);
            assert!(c.skills.iter().all(|s| valid_name(s)), "{}", c.id);
            assert_eq!(find(c.id), Some(c));
        }
    }

    #[test]
    fn every_category_has_entries() {
        for cat in CuratedCategory::ALL {
            assert!(CATALOG.iter().any(|c| c.category == cat), "{cat:?}");
        }
        assert_eq!(
            find("security-audit").unwrap().web_url(),
            "https://github.com/cloudflare/security-audit-skill"
        );
    }
}
