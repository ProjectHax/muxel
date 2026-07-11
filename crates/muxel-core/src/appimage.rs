//! Reaping leftover AppImage squashfuse mounts.
//!
//! A type-2 AppImage mounts its squashfs at `$TMPDIR/.mount_<name>-XXXXXX` via
//! squashfuse and unmounts it when the process exits. If muxel is SIGKILLed or
//! crashes, that unmount never runs and the mount is orphaned; once the
//! squashfuse daemon later dies, the mount goes stale (`statfs` returns
//! `ENOTCONN`, "Transport endpoint is not connected"). Anything that then
//! enumerates filesystems — `df`, which some desktop system monitors run every
//! ~60s — stalls in the kernel FUSE layer on the dead mount, which on a Wayland
//! compositor surfaces as a periodic cursor stutter that worsens as more
//! leftovers accumulate across days of uptime.
//!
//! muxel can't catch SIGKILL, so it reaps these on the next launch. This module
//! is the pure selection half — which mounts are muxel's and not our own; the
//! app crate does the liveness probe and the actual lazy-unmount.

/// Given the contents of `/proc/self/mounts` and this process's own AppImage
/// mount directory (`$APPDIR`; `None` when muxel wasn't launched from an
/// AppImage), return the mountpoints of *other* muxel AppImage squashfuse mounts
/// — leftovers from prior instances. Our own mount is never included.
///
/// The caller probes each returned mount for liveness and lazy-unmounts only the
/// dead ones: a live mount still belongs to another running muxel instance and
/// must be left alone.
pub fn foreign_muxel_appimage_mounts(mounts: &str, self_appdir: Option<&str>) -> Vec<String> {
    mounts
        .lines()
        .filter_map(parse_muxel_mount)
        .filter(|mp| self_appdir != Some(mp.as_str()))
        .collect()
}

/// Parse one `/proc/self/mounts` line, returning the (unescaped) mountpoint iff
/// it is a muxel AppImage squashfuse mount. Lines look like:
///
/// ```text
/// muxel-linux-x86_64.AppImage /tmp/.mount_muxel-CDigJK fuse.muxel-…AppImage ro,… 0 0
/// ```
///
/// Three signals must all hold, so this never matches another app's FUSE mount
/// (`gvfsd-fuse`, `portal`, another AppImage): the fstype is FUSE, the source or
/// fstype names muxel, and the mountpoint is an AppImage mount dir (`.mount_…`).
fn parse_muxel_mount(line: &str) -> Option<String> {
    let mut fields = line.split(' ');
    let source = fields.next()?;
    let mountpoint = fields.next()?;
    let fstype = fields.next()?;

    let is_fuse = fstype == "fuse" || fstype.starts_with("fuse.");
    let names_muxel = fstype.to_ascii_lowercase().contains("muxel")
        || source.to_ascii_lowercase().contains("muxel");
    if !is_fuse || !names_muxel {
        return None;
    }

    let mp = unescape_mount_field(mountpoint);
    let is_appimage_mount = std::path::Path::new(&mp)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".mount_"));

    is_appimage_mount.then_some(mp)
}

/// Decode the octal escapes the kernel writes into `/proc/self/mounts` path
/// fields — space (`\040`), tab (`\011`), newline (`\012`), backslash (`\134`) —
/// so a `$TMPDIR` containing those characters compares correctly. AppImage mount
/// dirs live under `/tmp` and rarely need this, but the transform is cheap and
/// keeps the parser correct.
fn unescape_mount_field(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        // A `\ooo` triple is a valid octal escape only when three octal digits
        // follow; anything else is copied through verbatim.
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && let Ok(code) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(code as char);
            i += 4;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::foreign_muxel_appimage_mounts;

    // A realistic /proc/self/mounts slice: unrelated FUSE mounts, our own muxel
    // mount, and a leftover muxel mount from a prior instance.
    const MOUNTS: &str = "\
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw,nosuid,nodev,relatime,user_id=1000,group_id=1000 0 0
portal /run/user/1000/doc fuse.portal rw,nosuid,nodev,relatime,user_id=1000,group_id=1000 0 0
muxel-linux-x86_64.AppImage /tmp/.mount_muxel-CDigJK fuse.muxel-linux-x86_64.AppImage ro,nosuid,nodev,relatime,user_id=1000,group_id=1000 0 0
muxel-linux-x86_64.AppImage /tmp/.mount_muxel-gMGfBF fuse.muxel-linux-x86_64.AppImage ro,nosuid,nodev,relatime,user_id=1000,group_id=1000 0 0
some.AppImage /tmp/.mount_someAB fuse.some.AppImage ro,relatime 0 0";

    #[test]
    fn returns_foreign_muxel_mounts_excluding_our_own() {
        // Running from gMGfBF: only the leftover CDigJK is a reap candidate —
        // not our own mount, not gvfsd/portal, not the unrelated AppImage.
        let got = foreign_muxel_appimage_mounts(MOUNTS, Some("/tmp/.mount_muxel-gMGfBF"));
        assert_eq!(got, vec!["/tmp/.mount_muxel-CDigJK".to_string()]);
    }

    #[test]
    fn without_own_appdir_returns_all_muxel_mounts() {
        // Not launched from an AppImage ($APPDIR unset): both muxel mounts are
        // leftovers to consider; the liveness probe sorts dead from live.
        let mut got = foreign_muxel_appimage_mounts(MOUNTS, None);
        got.sort();
        assert_eq!(
            got,
            vec![
                "/tmp/.mount_muxel-CDigJK".to_string(),
                "/tmp/.mount_muxel-gMGfBF".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_non_muxel_and_non_fuse_mounts() {
        // No muxel mounts present at all → nothing selected (gvfsd/portal/other
        // AppImage/sysfs are all rejected).
        let other = "\
sysfs /sys sysfs rw 0 0
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0
some.AppImage /tmp/.mount_someAB fuse.some.AppImage ro 0 0";
        assert!(foreign_muxel_appimage_mounts(other, None).is_empty());
    }

    #[test]
    fn requires_appimage_mount_dir_shape() {
        // A muxel-named FUSE mount that isn't a `.mount_…` AppImage dir (e.g. a
        // bind of the project) is not an AppImage leftover and is left alone.
        let odd = "muxel /home/ryan/muxel fuse.muxel rw 0 0";
        assert!(foreign_muxel_appimage_mounts(odd, None).is_empty());
    }

    #[test]
    fn decodes_octal_escaped_mountpoint() {
        // $TMPDIR with a space: the kernel escapes it as \040; the returned path
        // is decoded so the caller can unmount it by its real name.
        let escaped = "muxel.AppImage /tmp/a\\040b/.mount_muxelXY fuse.muxel.AppImage ro 0 0";
        assert_eq!(
            foreign_muxel_appimage_mounts(escaped, None),
            vec!["/tmp/a b/.mount_muxelXY".to_string()]
        );
    }
}
