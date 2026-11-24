//! `trio setup`: put `trio` (and `trio-start`, `trio-app`) where the plugins' hooks can run it.
//!
//!   trio setup [--bin-dir <dir>] [--no-path]
//!
//! - Windows: copies into `~/.trio/bin` and adds that folder to the user PATH. The PATH is read
//!   and written raw (REG_EXPAND_SZ), so entries like `%USERPROFILE%\bin` stay unexpanded.
//! - macOS/Linux: copies into `~/.local/bin` and says so if that folder is not on PATH.
//! - A copy that is running (the login server) is renamed aside first; Windows allows renaming a
//!   running executable but not overwriting it.
//!
//! It never edits Claude Code or Codex configuration; it prints the steps instead, because both
//! ask you to review hooks before they run.

use std::path::{Path, PathBuf};

pub fn cmd_setup(args: &[String]) -> Result<(), String> {
    let store = crate::hook::Store::from_env()?;
    let bin = match crate::flag(args, "--bin-dir") {
        Some(d) => bin_dir_arg(&d)?,
        None => default_bin_dir(&store)?,
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let installed = install(&exe, &bin)?;
    for p in &installed {
        println!("installed {}", p.display());
    }
    if args.iter().any(|a| a == "--no-path") {
        println!("PATH left unchanged (--no-path)");
    } else {
        println!("{}", ensure_on_path(&bin)?);
    }
    println!(
        "\nNext:\n\
         - Claude Code: /plugin marketplace add SanjithTammana/trio, then /plugin install trio@trio\n\
         - Codex: add the two hooks in adapters/codex/hooks.json (see docs/codex.md), then review them in Settings > Hooks\n\
         - Usage-aware routing for Claude: set Claude Code's statusLine command to `trio statusline`\n   \
           (or `trio statusline -- \"<your current command>\"` to keep yours); Codex is read from its session logs\n\
         - A room: trio open <project-dir> (any folder; it picks up the repo and *-posts.md files); in a session, type: trio"
    );
    Ok(())
}

/// `--bin-dir` as an absolute path: a relative one is resolved against the current folder, so a
/// PATH entry never depends on where setup ran. A PATH separator in it is refused.
pub fn bin_dir_arg(d: &str) -> Result<PathBuf, String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    if d.trim().is_empty() || d.contains(sep) {
        return Err(format!("--bin-dir {d:?}: give one folder (no '{sep}')"));
    }
    std::path::absolute(d).map_err(|e| format!("--bin-dir {d:?}: {e}"))
}

fn default_bin_dir(store: &crate::hook::Store) -> Result<PathBuf, String> {
    if cfg!(windows) {
        return Ok(store.home.join("bin"));
    }
    let home = std::env::var_os("HOME").ok_or("no home dir")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// Copy `exe` and its siblings `trio-start` and `trio-app` (when present) into `bin`. Returns
/// what was written.
pub fn install(exe: &Path, bin: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let suffix = std::env::consts::EXE_SUFFIX;
    let mut out = Vec::new();
    for name in COMPANIONS.map(|n| format!("{n}{suffix}")) {
        let src = if name == format!("trio{suffix}") { exe.to_path_buf() } else { exe.with_file_name(&name) };
        if !src.is_file() {
            continue;
        }
        let dest = bin.join(&name);
        if same_file(&src, &dest) {
            out.push(dest);
            continue;
        }
        if std::fs::copy(&src, &dest).is_err() && dest.exists() {
            // Most likely running: move it aside, then copy. Old copies are swept next time.
            let aside = bin.join(format!("{name}.old-{}", std::process::id()));
            std::fs::rename(&dest, &aside).map_err(|e| format!("{}: in use and cannot be moved aside: {e}", dest.display()))?;
            std::fs::copy(&src, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        } else if !dest.is_file() {
            return Err(format!("{}: copy failed", dest.display()));
        }
        out.push(dest);
    }
    sweep_old(bin);
    Ok(out)
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Remove copies moved aside by earlier setups; ones still running stay until they exit.
fn sweep_old(bin: &Path) {
    let Ok(rd) = std::fs::read_dir(bin) else { return };
    for e in rd.flatten() {
        if is_trio_backup(&e.file_name().to_string_lossy()) && e.path().is_file() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The binaries `install` copies, `trio` first.
const COMPANIONS: [&str; 3] = ["trio", "trio-start", "trio-app"];

/// Exactly the names `install` gives a moved-aside copy: `<companion>[.exe].old-<pid>`.
/// Anything else in the folder is someone else's.
pub fn is_trio_backup(name: &str) -> bool {
    let suffix = std::env::consts::EXE_SUFFIX;
    COMPANIONS
        .map(|n| format!("{n}{suffix}.old-"))
        .iter()
        .any(|p| name.strip_prefix(p.as_str()).is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())))
}

/// `raw` PATH with `dir` appended, or None when it is already there (case-insensitive on
/// Windows, trailing separators ignored). Other entries are kept byte for byte.
pub fn path_with(raw: &str, dir: &str, sep: char) -> Option<String> {
    let norm = |s: &str| {
        let t = s.trim().trim_end_matches(['/', '\\']).to_string();
        if sep == ';' { t.to_ascii_lowercase() } else { t }
    };
    let want = norm(dir);
    if raw.split(sep).any(|p| norm(p) == want) {
        return None;
    }
    let base = raw.trim_end_matches(sep);
    Some(if base.is_empty() { dir.to_string() } else { format!("{base}{sep}{dir}") })
}

#[cfg(windows)]
fn ensure_on_path(bin: &Path) -> Result<String, String> {
    let dir = crate::hook::display(bin);
    // Read the raw value: no expansion, so %VARS% survive the rewrite.
    let raw = powershell(
        "$k = Get-Item 'HKCU:\\Environment'; [Console]::Out.Write($k.GetValue('Path', '', 'DoNotExpandEnvironmentNames'))",
        &[],
    )?;
    let Some(new) = path_with(&raw, &dir, ';') else { return Ok(format!("{dir} is already on your PATH")) };
    // Write it back as REG_EXPAND_SZ, then let running programs know (setting a user variable
    // through .NET broadcasts WM_SETTINGCHANGE).
    powershell(
        "New-ItemProperty -Path 'HKCU:\\Environment' -Name Path -PropertyType ExpandString -Value $env:TRIO_NEW_PATH -Force | Out-Null; \
         [Environment]::SetEnvironmentVariable('TRIO_PATH_REFRESH', $null, 'User')",
        &[("TRIO_NEW_PATH", &new)],
    )?;
    Ok(format!("added {dir} to your user PATH; open a new terminal or restart Claude/Codex to pick it up"))
}

#[cfg(windows)]
fn powershell(script: &str, env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| format!("powershell: {e}"))?;
    if !out.status.success() {
        return Err(format!("powershell: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(windows))]
fn ensure_on_path(bin: &Path) -> Result<String, String> {
    let dir = bin.display().to_string();
    let path = std::env::var("PATH").unwrap_or_default();
    Ok(match path_with(&path, &dir, ':') {
        None => format!("{dir} is on your PATH"),
        Some(_) => format!("{dir} is not on your PATH; add this to your shell profile: export PATH=\"{dir}:$PATH\""),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_append_keeps_entries_and_skips_duplicates() {
        let raw = r"%USERPROFILE%\bin;C:\Tools;";
        assert_eq!(path_with(raw, r"C:\Users\u\.trio\bin", ';').unwrap(), r"%USERPROFILE%\bin;C:\Tools;C:\Users\u\.trio\bin");
        assert_eq!(path_with(raw, r"c:\tools\", ';'), None, "case and trailing slash ignored on Windows");
        assert_eq!(path_with("", "C:/b", ';').unwrap(), "C:/b");
        assert_eq!(path_with("/usr/bin:/home/u/.local/bin", "/home/u/.local/bin/", ':'), None);
        assert!(path_with("/usr/bin", "/Usr/bin", ':').is_some(), "case matters on Unix");
    }

    #[test]
    fn install_copies_then_replaces_and_is_idempotent() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("trio-setup-{}-{nanos}", std::process::id()));
        let src = base.join("build");
        std::fs::create_dir_all(&src).unwrap();
        let suffix = std::env::consts::EXE_SUFFIX;
        let exe = src.join(format!("trio{suffix}"));
        std::fs::write(&exe, b"v1").unwrap();
        std::fs::write(src.join(format!("trio-start{suffix}")), b"start").unwrap();
        std::fs::write(src.join(format!("trio-app{suffix}")), b"app").unwrap();
        let bin = base.join("bin");
        let got = install(&exe, &bin).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(std::fs::read(bin.join(format!("trio-app{suffix}"))).unwrap(), b"app");
        assert_eq!(std::fs::read(bin.join(format!("trio{suffix}"))).unwrap(), b"v1");
        std::fs::write(&exe, b"v2").unwrap();
        install(&exe, &bin).unwrap();
        assert_eq!(std::fs::read(bin.join(format!("trio{suffix}"))).unwrap(), b"v2");
        // Running setup from the installed copy itself is a no-op, not a self-overwrite.
        assert_eq!(install(&bin.join(format!("trio{suffix}")), &bin).unwrap().len(), 3);
        // The sweep removes only Trio's own moved-aside copies.
        let mine = bin.join(format!("trio{suffix}.old-123"));
        let theirs = bin.join("notes.old-backup.txt");
        let lookalike = bin.join(format!("trio{suffix}.old-123.keep"));
        for f in [&mine, &theirs, &lookalike] {
            std::fs::write(f, b"x").unwrap();
        }
        install(&exe, &bin).unwrap();
        assert!(!mine.exists());
        assert!(theirs.exists() && lookalike.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn bin_dir_is_absolute_and_single() {
        let got = bin_dir_arg("rel/bin").unwrap();
        assert!(got.is_absolute() && got.ends_with("rel/bin"), "{got:?}");
        let sep = if cfg!(windows) { "a;b" } else { "a:b" };
        assert!(bin_dir_arg(sep).is_err());
        assert!(bin_dir_arg("  ").is_err());
    }
}
