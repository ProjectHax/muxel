//! A minimal POSIX ustar writer, for sending a skill folder to an SSH host in
//! one stream (`tar -xf -` on the far side; `tar.exe` on Windows 10+).
//!
//! Only what a skill needs: regular files and the folders holding them, paths
//! up to 255 bytes (ustar's prefix + name split), no links or special files.

/// One file to archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarFile {
    /// `/`-separated, relative, no `.` or `..` components.
    pub path: String,
    pub data: Vec<u8>,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TarError {
    /// A path ustar can't hold, or one that isn't a plain relative path.
    BadPath(String),
}

const BLOCK: usize = 512;

/// The archive: a folder entry for every parent folder (first), then each file.
/// `mtime` is the Unix time stamped on every entry.
pub fn ustar(files: &[TarFile], mtime: u64) -> Result<Vec<u8>, TarError> {
    let mut dirs: Vec<String> = Vec::new();
    for f in files {
        check(&f.path)?;
        let mut acc = String::new();
        let parts: Vec<&str> = f.path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if !dirs.contains(&acc) {
                dirs.push(acc.clone());
            }
        }
    }
    let mut out = Vec::new();
    for d in &dirs {
        out.extend_from_slice(&header(&format!("{d}/"), 0, 0o755, b'5', mtime)?);
    }
    for f in files {
        let mode = if f.executable { 0o755 } else { 0o644 };
        out.extend_from_slice(&header(&f.path, f.data.len() as u64, mode, b'0', mtime)?);
        out.extend_from_slice(&f.data);
        let pad = (BLOCK - f.data.len() % BLOCK) % BLOCK;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    Ok(out)
}

fn check(path: &str) -> Result<(), TarError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..");
    if bad {
        return Err(TarError::BadPath(path.to_string()));
    }
    Ok(())
}

fn octal(field: &mut [u8], value: u64) {
    // Digits fill all but the final NUL.
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&s.as_bytes()[s.len() - digits..]);
    field[digits] = 0;
}

fn header(path: &str, size: u64, mode: u32, kind: u8, mtime: u64) -> Result<[u8; BLOCK], TarError> {
    let (prefix, name) = split(path).ok_or_else(|| TarError::BadPath(path.to_string()))?;
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    // The checksum is computed with its own field as spaces.
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let s = format!("{sum:06o}");
    h[148..154].copy_from_slice(s.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    Ok(h)
}

/// Split into ustar's `prefix` (≤ 155) and `name` (≤ 100) at a `/`.
fn split(path: &str) -> Option<(&str, &str)> {
    if path.len() <= 100 {
        return Some(("", path));
    }
    // A folder's trailing `/` stays with the name; it's not a split point.
    let body = path.trim_end_matches('/');
    body.match_indices('/')
        .map(|(i, _)| (&path[..i], &path[i + 1..]))
        .find(|(prefix, name)| prefix.len() <= 155 && name.len() <= 100)
}

#[cfg(test)]
mod tests {
    use super::{TarError, TarFile, ustar};

    fn file(path: &str, data: &[u8]) -> TarFile {
        TarFile {
            path: path.into(),
            data: data.to_vec(),
            executable: false,
        }
    }

    #[test]
    fn writes_dirs_files_and_padding() {
        let tar = ustar(&[file("SKILL.md", b"hi"), file("refs/a.md", b"x")], 1).unwrap();
        // dir header + 2 × (header + 1 data block) + 2 end blocks.
        assert_eq!(tar.len(), 512 * 7);
        assert_eq!(&tar[..5], b"refs/");
        assert_eq!(tar[156], b'5');
        assert_eq!(&tar[512..520], b"SKILL.md");
        assert_eq!(&tar[512 + 124..512 + 136], b"00000000002\0");
        assert_eq!(&tar[1024..1026], b"hi");
        // Checksum: unsigned sum with the field read as spaces.
        let h = &tar[512..1024];
        let mut sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        sum = sum - h[148..156].iter().map(|&b| u32::from(b)).sum::<u32>() + 8 * 32;
        let stored = std::str::from_utf8(&h[148..154]).unwrap();
        assert_eq!(u32::from_str_radix(stored, 8).unwrap(), sum);
    }

    #[test]
    fn long_paths_use_the_prefix() {
        let dirs = format!("{}/{}", "d".repeat(60), "e".repeat(60));
        let long = format!("{dirs}/{}", "f".repeat(90));
        let tar = ustar(&[file(&long, b"")], 0).unwrap();
        // Two folder headers, then the file's.
        let file_header = &tar[1024..1536];
        assert_eq!(&file_header[..90], "f".repeat(90).as_bytes());
        assert_eq!(&file_header[345..345 + dirs.len()], dirs.as_bytes());
        // One component longer than ustar's name field can't be stored.
        let too_long = format!("a/{}", "f".repeat(101));
        assert!(matches!(
            ustar(&[file(&too_long, b"")], 0),
            Err(TarError::BadPath(_))
        ));
    }

    #[test]
    fn refuses_unsafe_paths() {
        for bad in ["/etc/passwd", "../x", "a/../b", "a//b", "", "a\\b"] {
            assert!(ustar(&[file(bad, b"")], 0).is_err(), "{bad}");
        }
    }
}
