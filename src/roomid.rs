//! A room's identity: a hash of its canonical path. The server reports it, the hooks' pokes
//! carry it, and the native window checks it, so a port that now belongs to another room's
//! server is never mistaken for this one. Shared with `app/` by path, so both compute it alike.

use std::path::Path;

/// Paths without the `\\?\` prefix canonicalize adds on Windows. For text only (UI, messages,
/// the room id's hash input); filesystem and process calls keep the canonical `PathBuf`.
/// A verbatim UNC path (`\\?\UNC\server\share`) becomes `\\server\share`, still absolute.
pub fn display(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// FNV-1a of the canonical path, as 16 hex digits.
pub fn room_id(root: &Path) -> String {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let h = display(&canon).bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verbatim_prefix_is_stripped_whole() {
        assert_eq!(display(Path::new(r"\\?\C:\rooms\one")), r"C:\rooms\one");
        assert_eq!(display(Path::new(r"\\?\UNC\server\share\room")), r"\\server\share\room");
        assert_eq!(display(Path::new("relative/one")), "relative/one");
    }
}
