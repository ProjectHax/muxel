//! Installing tmux on a computer that doesn't have it.
//!
//! muxel runs local agents inside tmux so they outlive the window; without it
//! they still run, only not persistently. On first launch muxel offers to install
//! it. Everything here decides without touching the system: reading
//! `/etc/os-release`, choosing the package manager, the exact commands, and what
//! an elevation helper's exit means. The app probes the disk and runs the result.
//!
//! Linux package managers run as root; the app elevates just the install (pkexec,
//! or sudo with a password muxel asks for). Homebrew is the macOS path, and it
//! refuses to run as root, so it runs as the user and needs no elevation.

use crate::ssh::sh_quote;

/// The fields of `/etc/os-release` that choose a package manager.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OsRelease {
    /// `ID` — `ubuntu`, `fedora`, `arch`, …
    pub id: String,
    /// `ID_LIKE`, split: the distros this one derives from (`ubuntu debian`).
    pub id_like: Vec<String>,
    /// `PRETTY_NAME`, else `NAME`, for the dialog.
    pub name: String,
}

/// Parse `/etc/os-release` (`KEY=value` lines, values optionally quoted).
pub fn parse_os_release(text: &str) -> OsRelease {
    let mut out = OsRelease::default();
    let mut name = String::new();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "ID" => out.id = value.to_ascii_lowercase(),
            "ID_LIKE" => {
                out.id_like = value
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect();
            }
            "PRETTY_NAME" => out.name = value,
            "NAME" => name = value,
            _ => {}
        }
    }
    if out.name.is_empty() {
        out.name = name;
    }
    out
}

/// Strip one layer of matching quotes and the shell escapes os-release allows.
fn unquote(value: &str) -> String {
    let inner = match value.as_bytes() {
        [b'"', .., b'"'] | [b'\'', .., b'\''] if value.len() >= 2 => &value[1..value.len() - 1],
        _ => value,
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// A package manager muxel knows how to install tmux with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Apt,
    Dnf,
    Yum,
    Zypper,
    Pacman,
    Apk,
    Xbps,
    Emerge,
    Eopkg,
    Brew,
}

/// Every Linux manager, in the order tried when the distro isn't recognised.
const LINUX_MANAGERS: [Manager; 9] = [
    Manager::Apt,
    Manager::Dnf,
    Manager::Yum,
    Manager::Zypper,
    Manager::Pacman,
    Manager::Apk,
    Manager::Xbps,
    Manager::Eopkg,
    Manager::Emerge,
];

impl Manager {
    /// The executable to look for.
    pub fn program(self) -> &'static str {
        match self {
            Manager::Apt => "apt-get",
            Manager::Dnf => "dnf",
            Manager::Yum => "yum",
            Manager::Zypper => "zypper",
            Manager::Pacman => "pacman",
            Manager::Apk => "apk",
            Manager::Xbps => "xbps-install",
            Manager::Emerge => "emerge",
            Manager::Eopkg => "eopkg",
            Manager::Brew => "brew",
        }
    }

    /// What the dialog calls it.
    pub fn label(self) -> &'static str {
        match self {
            Manager::Apt => "apt",
            Manager::Brew => "Homebrew",
            Manager::Xbps => "xbps",
            other => other.program(),
        }
    }

    /// The managers a distro family uses, preferred first.
    fn for_family(family: &str) -> &'static [Manager] {
        match family {
            "debian" | "ubuntu" => &[Manager::Apt],
            "fedora" | "rhel" | "centos" => &[Manager::Dnf, Manager::Yum],
            "suse" | "opensuse" | "sles" => &[Manager::Zypper],
            f if f.starts_with("opensuse") => &[Manager::Zypper],
            "arch" => &[Manager::Pacman],
            "alpine" => &[Manager::Apk],
            "void" => &[Manager::Xbps],
            "gentoo" => &[Manager::Emerge],
            "solus" => &[Manager::Eopkg],
            _ => &[],
        }
    }

    /// Everything but the program: steps as argv tails, plus environment.
    fn steps(self) -> (Vec<Step>, &'static [(&'static str, &'static str)]) {
        let step = |args: &[&str], may_fail: bool| Step {
            args: args.iter().map(|a| a.to_string()).collect(),
            may_fail,
        };
        match self {
            // A fresh image has empty package lists and a stale one 404s on the
            // .deb, so refresh first — but a single broken third-party repo fails
            // `update` outright while the rest refreshed fine, so carry on. The
            // lock timeout waits out unattended-upgrades instead of failing.
            Manager::Apt => (
                vec![
                    step(&["update"], true),
                    step(
                        &["install", "-y", "-o", "DPkg::Lock::Timeout=120", "tmux"],
                        false,
                    ),
                ],
                &[("DEBIAN_FRONTEND", "noninteractive")],
            ),
            Manager::Dnf | Manager::Yum | Manager::Eopkg => {
                (vec![step(&["install", "-y", "tmux"], false)], &[])
            }
            Manager::Zypper => (
                vec![step(&["--non-interactive", "install", "tmux"], false)],
                &[],
            ),
            Manager::Pacman => (
                vec![step(&["-S", "--needed", "--noconfirm", "tmux"], false)],
                &[],
            ),
            Manager::Apk => (vec![step(&["add", "tmux"], false)], &[]),
            Manager::Xbps => (vec![step(&["-Sy", "tmux"], false)], &[]),
            Manager::Emerge => (vec![step(&["--ask=n", "app-misc/tmux"], false)], &[]),
            Manager::Brew => (
                vec![step(&["install", "tmux"], false)],
                &[("HOMEBREW_NO_ENV_HINTS", "1")],
            ),
        }
    }
}

/// One command of an install: the manager's args, and whether its failure is
/// tolerated (the next step decides).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Step {
    args: Vec<String>,
    may_fail: bool,
}

/// How muxel can install tmux here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Run a package manager.
    Install(Install),
    /// Nothing muxel can run itself; the dialog says what to do instead.
    Manual(Manual),
}

/// Why muxel can't install tmux itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manual {
    /// macOS without Homebrew.
    NoHomebrew,
    /// NixOS: packages come from the system configuration.
    NixOs,
    /// An image-based system (Silverblue, Kinoite, …) whose `/usr` is read-only.
    Immutable,
    /// No package manager muxel recognises.
    Unsupported,
}

/// A package-manager install of tmux.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub manager: Manager,
    /// The manager's absolute path, as found on disk.
    pub program: String,
}

impl Install {
    /// Whether it must run as root. Homebrew refuses to.
    pub fn needs_root(&self) -> bool {
        self.manager != Manager::Brew
    }

    /// The commands as the dialog shows them, one per line — the manager by name
    /// rather than path, but otherwise exactly what runs.
    pub fn display(&self) -> String {
        let (steps, _) = self.manager.steps();
        steps
            .iter()
            .map(|s| command_line(self.manager.program(), &s.args))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `sh -c` script that performs the install, stopping at the first
    /// failure that isn't tolerated.
    pub fn script(&self) -> String {
        let (steps, env) = self.manager.steps();
        let mut lines = vec!["set -e".to_string()];
        lines.extend(
            env.iter()
                .map(|(k, v)| format!("export {k}={}", sh_quote(v))),
        );
        for step in steps {
            let line = command_line(&self.program, &step.args);
            lines.push(if step.may_fail {
                format!("{line} || true")
            } else {
                line
            });
        }
        lines.join("\n")
    }
}

fn command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(sh_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Linux plan: the distro's own manager when it's recognised and present,
/// else the first manager found at all. `ostree` is whether `/run/ostree-booted`
/// exists; `find` resolves a program name to its absolute path.
pub fn linux_plan(os: &OsRelease, ostree: bool, find: impl Fn(&str) -> Option<String>) -> Plan {
    let families = || std::iter::once(os.id.as_str()).chain(os.id_like.iter().map(String::as_str));
    if families().any(|f| f == "nixos") {
        return Plan::Manual(Manual::NixOs);
    }
    // Checked before the managers: Fedora Atomic ships a `dnf` that can't
    // install onto the host.
    if ostree {
        return Plan::Manual(Manual::Immutable);
    }
    let preferred = families().flat_map(|f| Manager::for_family(f).iter().copied());
    let mut tried = Vec::new();
    for manager in preferred.chain(LINUX_MANAGERS) {
        if tried.contains(&manager) {
            continue;
        }
        tried.push(manager);
        if let Some(program) = find(manager.program()) {
            return Plan::Install(Install { manager, program });
        }
    }
    Plan::Manual(Manual::Unsupported)
}

/// The macOS plan: Homebrew, found at `brew`.
pub fn macos_plan(brew: Option<String>) -> Plan {
    match brew {
        Some(program) => Plan::Install(Install {
            manager: Manager::Brew,
            program,
        }),
        None => Plan::Manual(Manual::NoHomebrew),
    }
}

/// Where Homebrew installs itself: Apple silicon, then Intel. A GUI launch's
/// `PATH` may miss both.
pub const BREW_PATHS: [&str; 2] = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"];

/// Where a Linux package manager lives. Searched directly rather than through
/// `PATH`, which a desktop launch may have trimmed and which omits `sbin` for
/// ordinary users on some distros.
pub const SYSTEM_BIN_DIRS: [&str; 6] = [
    "/usr/bin",
    "/usr/sbin",
    "/bin",
    "/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// What a `pkexec` exit status means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PkexecExit {
    /// The install ran and succeeded.
    Installed,
    /// The user closed the authentication dialog.
    Dismissed,
    /// No authorization: no polkit agent is running, the user isn't an admin, or
    /// the password was refused. sudo may still work.
    Unauthorized,
    /// The install itself failed with this status (or a signal, as `None`).
    Failed(Option<i32>),
}

/// Read `pkexec`'s status: 126 is a dismissed dialog and 127 is any failure to
/// authorize; otherwise it is the program's own. The install's managers are
/// found by absolute path, so the program itself never exits 127 for a missing
/// command.
pub fn pkexec_exit(code: Option<i32>) -> PkexecExit {
    match code {
        Some(0) => PkexecExit::Installed,
        Some(126) => PkexecExit::Dismissed,
        Some(127) => PkexecExit::Unauthorized,
        other => PkexecExit::Failed(other),
    }
}

/// Why `sudo -v` turned a password down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SudoRefusal {
    /// Wrong password — ask again.
    WrongPassword,
    /// The account may not use sudo at all.
    NotAllowed,
    /// Anything else, in sudo's words.
    Other(String),
}

/// Classify `sudo`'s stderr after a failed validation. Run with `LC_ALL=C` so
/// the messages are sudo's English originals.
pub fn sudo_refusal(stderr: &str) -> SudoRefusal {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("incorrect password")
        || lower.contains("sorry, try again")
        || lower.contains("no password was provided")
    {
        SudoRefusal::WrongPassword
    } else if lower.contains("not in the sudoers")
        || lower.contains("may not run sudo")
        || lower.contains("not allowed to execute")
    {
        SudoRefusal::NotAllowed
    } else {
        let text = stderr.trim();
        let last = text.lines().last().unwrap_or(text).trim();
        SudoRefusal::Other(last.trim_start_matches("sudo: ").to_string())
    }
}

/// The last visible line of a chunk of package-manager output: progress bars
/// redraw with `\r`, so only the text after the final one is on screen.
pub fn visible_line(raw: &str) -> Option<&str> {
    let line = raw.trim_end_matches(['\n', '\r']);
    let line = line.rsplit('\r').find(|s| !s.trim().is_empty())?;
    let line = line.trim_end();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::{
        Install, Manager, Manual, OsRelease, PkexecExit, Plan, SudoRefusal, linux_plan, macos_plan,
        parse_os_release, pkexec_exit, sudo_refusal, visible_line,
    };

    fn os(id: &str, like: &[&str]) -> OsRelease {
        OsRelease {
            id: id.into(),
            id_like: like.iter().map(|s| s.to_string()).collect(),
            name: String::new(),
        }
    }

    /// A disk holding exactly these programs, in `/usr/bin`.
    fn disk(programs: &'static [&'static str]) -> impl Fn(&str) -> Option<String> {
        move |p| programs.contains(&p).then(|| format!("/usr/bin/{p}"))
    }

    fn manager(plan: Plan) -> Option<Manager> {
        match plan {
            Plan::Install(i) => Some(i.manager),
            Plan::Manual(_) => None,
        }
    }

    #[test]
    fn parses_quoted_os_release() {
        let text = r#"
NAME="Ubuntu"
VERSION_ID="24.04"
ID=ubuntu
ID_LIKE=debian
PRETTY_NAME="Ubuntu 24.04.1 LTS"
"#;
        let r = parse_os_release(text);
        assert_eq!(r.id, "ubuntu");
        assert_eq!(r.id_like, vec!["debian"]);
        assert_eq!(r.name, "Ubuntu 24.04.1 LTS");
    }

    #[test]
    fn parses_multi_family_id_like_and_falls_back_to_name() {
        let r =
            parse_os_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nNAME='Rocky Linux'\n");
        assert_eq!(r.id, "rocky");
        assert_eq!(r.id_like, vec!["rhel", "centos", "fedora"]);
        assert_eq!(r.name, "Rocky Linux");
    }

    #[test]
    fn unescapes_quoted_values() {
        let r = parse_os_release(r#"PRETTY_NAME="Say \"hi\"""#);
        assert_eq!(r.name, r#"Say "hi""#);
    }

    #[test]
    fn picks_each_family_manager() {
        let everything = disk(&[
            "apt-get",
            "dnf",
            "yum",
            "zypper",
            "pacman",
            "apk",
            "xbps-install",
            "emerge",
            "eopkg",
        ]);
        let cases = [
            (os("debian", &[]), Manager::Apt),
            (os("pop", &["ubuntu", "debian"]), Manager::Apt),
            (os("fedora", &[]), Manager::Dnf),
            (os("almalinux", &["rhel", "centos", "fedora"]), Manager::Dnf),
            (
                os("opensuse-tumbleweed", &["opensuse", "suse"]),
                Manager::Zypper,
            ),
            (os("opensuse-leap", &[]), Manager::Zypper),
            (os("sles", &["suse"]), Manager::Zypper),
            (os("manjaro", &["arch"]), Manager::Pacman),
            (os("alpine", &[]), Manager::Apk),
            (os("void", &[]), Manager::Xbps),
            (os("gentoo", &[]), Manager::Emerge),
            (os("solus", &[]), Manager::Eopkg),
        ];
        for (release, expected) in cases {
            assert_eq!(
                manager(linux_plan(&release, false, &everything)),
                Some(expected),
                "{release:?}"
            );
        }
    }

    #[test]
    fn falls_back_to_yum_without_dnf() {
        let plan = linux_plan(&os("centos", &["rhel", "fedora"]), false, disk(&["yum"]));
        assert_eq!(manager(plan), Some(Manager::Yum));
    }

    #[test]
    fn unknown_distro_uses_whatever_manager_is_present() {
        let plan = linux_plan(&os("mystery", &[]), false, disk(&["pacman"]));
        assert_eq!(manager(plan), Some(Manager::Pacman));
        let plan = linux_plan(&OsRelease::default(), false, disk(&["apt-get"]));
        assert_eq!(manager(plan), Some(Manager::Apt));
    }

    #[test]
    fn family_manager_wins_over_a_stray_one() {
        // A Debian box that happens to have `dnf` installed still uses apt.
        let plan = linux_plan(&os("debian", &[]), false, disk(&["dnf", "apt-get"]));
        assert_eq!(manager(plan), Some(Manager::Apt));
    }

    #[test]
    fn nixos_and_ostree_and_nothing_are_manual() {
        let all = disk(&["apt-get", "dnf"]);
        assert_eq!(
            linux_plan(&os("nixos", &[]), false, &all),
            Plan::Manual(Manual::NixOs)
        );
        assert_eq!(
            linux_plan(&os("fedora", &[]), true, &all),
            Plan::Manual(Manual::Immutable)
        );
        assert_eq!(
            linux_plan(&os("fedora", &[]), false, disk(&[])),
            Plan::Manual(Manual::Unsupported)
        );
    }

    #[test]
    fn apt_script_refreshes_tolerantly_then_installs() {
        let install = Install {
            manager: Manager::Apt,
            program: "/usr/bin/apt-get".into(),
        };
        assert!(install.needs_root());
        assert_eq!(
            install.script(),
            "set -e\n\
             export DEBIAN_FRONTEND=noninteractive\n\
             /usr/bin/apt-get update || true\n\
             /usr/bin/apt-get install -y -o DPkg::Lock::Timeout=120 tmux"
        );
        assert_eq!(
            install.display(),
            "apt-get update\napt-get install -y -o DPkg::Lock::Timeout=120 tmux"
        );
    }

    #[test]
    fn pacman_and_zypper_scripts_are_non_interactive() {
        let pacman = Install {
            manager: Manager::Pacman,
            program: "/usr/bin/pacman".into(),
        };
        assert_eq!(
            pacman.script(),
            "set -e\n/usr/bin/pacman -S --needed --noconfirm tmux"
        );
        let zypper = Install {
            manager: Manager::Zypper,
            program: "/usr/bin/zypper".into(),
        };
        assert_eq!(zypper.display(), "zypper --non-interactive install tmux");
    }

    #[test]
    fn brew_runs_as_the_user_and_quotes_odd_prefixes() {
        let plan = macos_plan(Some("/Users/a b/homebrew/bin/brew".into()));
        let Plan::Install(install) = plan else {
            panic!("expected an install");
        };
        assert!(!install.needs_root());
        assert_eq!(install.manager.label(), "Homebrew");
        assert_eq!(
            install.script(),
            "set -e\nexport HOMEBREW_NO_ENV_HINTS=1\n'/Users/a b/homebrew/bin/brew' install tmux"
        );
        assert_eq!(macos_plan(None), Plan::Manual(Manual::NoHomebrew));
    }

    #[test]
    fn reads_pkexec_status() {
        assert_eq!(pkexec_exit(Some(0)), PkexecExit::Installed);
        assert_eq!(pkexec_exit(Some(126)), PkexecExit::Dismissed);
        assert_eq!(pkexec_exit(Some(127)), PkexecExit::Unauthorized);
        assert_eq!(pkexec_exit(Some(100)), PkexecExit::Failed(Some(100)));
        assert_eq!(pkexec_exit(None), PkexecExit::Failed(None));
    }

    #[test]
    fn classifies_sudo_refusals() {
        assert_eq!(
            sudo_refusal(
                "Sorry, try again.\nsudo: no password was provided\nsudo: 1 incorrect password attempt\n"
            ),
            SudoRefusal::WrongPassword
        );
        assert_eq!(
            sudo_refusal("alice is not in the sudoers file.\n"),
            SudoRefusal::NotAllowed
        );
        assert_eq!(
            sudo_refusal("Sorry, user alice may not run sudo on box.\n"),
            SudoRefusal::NotAllowed
        );
        assert_eq!(
            sudo_refusal("sudo: sorry, you must have a tty to run sudo\n"),
            SudoRefusal::Other("sorry, you must have a tty to run sudo".into())
        );
    }

    #[test]
    fn keeps_only_the_visible_part_of_a_progress_line() {
        assert_eq!(
            visible_line("Get:1 http://x tmux\n"),
            Some("Get:1 http://x tmux")
        );
        assert_eq!(visible_line(" 10%\r 50%\r100%\r\n"), Some("100%"));
        assert_eq!(visible_line("\r\n"), None);
        assert_eq!(visible_line("   \n"), None);
    }
}
