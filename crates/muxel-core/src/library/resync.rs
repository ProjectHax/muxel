//! Rules that turn the pre-re-sync local-changes check into "start without
//! asking", the counted confirmation or the generic one. The check itself
//! (filesystem + git) lives in the app crate.

/// Result of the local-changes check of a library clone: modified, deleted
/// and untracked files, and local commits not on the upstream branch. Files
/// ignored by git never count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalChanges {
    /// `LIB_DIR/<id>` does not exist, or exists without `.git` and is empty.
    NoClone,
    /// No changed files and no local commits.
    None,
    /// At least one changed file or local commit.
    Changes { files: usize, commits: usize },
    /// The check failed (git missing, git error, time limit reached), or
    /// the local commits cannot be counted (no upstream, detached HEAD).
    Unknown,
}

impl LocalChanges {
    pub fn from_counts(files: usize, commits: usize) -> Self {
        if files == 0 && commits == 0 {
            LocalChanges::None
        } else {
            LocalChanges::Changes { files, commits }
        }
    }
}

/// The confirmation a re-sync needs after the check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncConfirm {
    /// The counted text.
    Counted { files: usize, commits: usize },
    /// The generic text: the check could not tell what would be lost.
    Generic,
}

/// What a finished check leads to, before the library state is consulted:
/// `None` = start the re-sync without a dialog.
pub fn confirmation_for(changes: LocalChanges) -> Option<ResyncConfirm> {
    match changes {
        LocalChanges::NoClone | LocalChanges::None => None,
        LocalChanges::Changes { files, commits } if files == 0 && commits == 0 => None,
        LocalChanges::Changes { files, commits } => Some(ResyncConfirm::Counted { files, commits }),
        LocalChanges::Unknown => Some(ResyncConfirm::Generic),
    }
}

/// Grammatical number of a count in the confirmation text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plural {
    One,
    Other,
}

impl Plural {
    pub fn of(n: usize) -> Self {
        if n == 1 { Plural::One } else { Plural::Other }
    }
}

/// The loss part of the counted confirmation text: which counts are named and
/// in which number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncLoss {
    /// Only `F ≥ 1`: `<F> <FILES>`.
    Files(Plural),
    /// Only `C ≥ 1`: `<C> <COMMITS>`.
    Commits(Plural),
    /// `F ≥ 1` and `C ≥ 1`: `<F> <FILES> and <C> <COMMITS>`.
    Both(Plural, Plural),
}

/// The `<LOSS>` form for these counts; `None` when both are 0.
pub fn resync_loss(files: usize, commits: usize) -> Option<ResyncLoss> {
    match (files, commits) {
        (0, 0) => None,
        (f, 0) => Some(ResyncLoss::Files(Plural::of(f))),
        (0, c) => Some(ResyncLoss::Commits(Plural::of(c))),
        (f, c) => Some(ResyncLoss::Both(Plural::of(f), Plural::of(c))),
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalChanges, Plural, ResyncConfirm, ResyncLoss, confirmation_for, resync_loss};

    #[test]
    fn from_counts_zero_is_none_otherwise_changes() {
        assert_eq!(LocalChanges::from_counts(0, 0), LocalChanges::None);
        assert_eq!(
            LocalChanges::from_counts(1, 0),
            LocalChanges::Changes {
                files: 1,
                commits: 0
            }
        );
        assert_eq!(
            LocalChanges::from_counts(0, 2),
            LocalChanges::Changes {
                files: 0,
                commits: 2
            }
        );
    }

    #[test]
    fn confirmation_for_each_result() {
        assert_eq!(confirmation_for(LocalChanges::NoClone), None);
        assert_eq!(confirmation_for(LocalChanges::None), None);
        // A degenerate `Changes` with nothing in it is "no changes".
        assert_eq!(
            confirmation_for(LocalChanges::Changes {
                files: 0,
                commits: 0
            }),
            None
        );
        assert_eq!(
            confirmation_for(LocalChanges::Changes {
                files: 5,
                commits: 0
            }),
            Some(ResyncConfirm::Counted {
                files: 5,
                commits: 0
            })
        );
        assert_eq!(
            confirmation_for(LocalChanges::Changes {
                files: 0,
                commits: 1
            }),
            Some(ResyncConfirm::Counted {
                files: 0,
                commits: 1
            })
        );
        assert_eq!(
            confirmation_for(LocalChanges::Unknown),
            Some(ResyncConfirm::Generic)
        );
    }

    #[test]
    fn resync_confirm_text_plural_rules() {
        use Plural::{One, Other};
        assert_eq!(resync_loss(0, 0), None);
        assert_eq!(resync_loss(1, 0), Some(ResyncLoss::Files(One)));
        assert_eq!(resync_loss(5, 0), Some(ResyncLoss::Files(Other)));
        assert_eq!(resync_loss(2, 0), Some(ResyncLoss::Files(Other)));
        assert_eq!(resync_loss(0, 1), Some(ResyncLoss::Commits(One)));
        assert_eq!(resync_loss(0, 2), Some(ResyncLoss::Commits(Other)));
        assert_eq!(resync_loss(3, 2), Some(ResyncLoss::Both(Other, Other)));
        assert_eq!(resync_loss(3, 1), Some(ResyncLoss::Both(Other, One)));
        assert_eq!(resync_loss(1, 1), Some(ResyncLoss::Both(One, One)));
        assert_eq!(resync_loss(1, 4), Some(ResyncLoss::Both(One, Other)));
    }

    #[test]
    fn plural_of_is_one_only_for_exactly_one() {
        assert_eq!(Plural::of(1), Plural::One);
        assert_eq!(Plural::of(0), Plural::Other);
        assert_eq!(Plural::of(2), Plural::Other);
        assert_eq!(Plural::of(21), Plural::Other);
    }
}
