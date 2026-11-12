//! Shortcuts to the formats' system folders next to the bundles (`--shortcuts`,
//! `[bundle] shortcuts = true`): a `.lnk` file on Windows, a symbolic link elsewhere.
//! A shortcut is made even when its folder does not exist yet (with a warning).

use std::path::{Path, PathBuf};

/// Makes each `(link, folder)` shortcut, replacing one of the same name; returns the links made
/// (on Windows with `.lnk` added).
pub fn make(links: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    for (_, folder) in links {
        if !folder.is_dir() {
            eprintln!("warning: {} does not exist (the shortcut is made anyway)", folder.display());
        }
    }
    make_links(links)
}

#[cfg(windows)]
fn make_links(links: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    use std::process::Command;
    let made: Vec<(PathBuf, PathBuf)> = links.iter().map(|(link, folder)| (with_lnk(link), folder.clone())).collect();
    if made.is_empty() {
        return Ok(made);
    }
    // One PowerShell for all of them (WScript.Shell writes .lnk files). The paths go through an
    // environment variable, one `link|folder` per line ('|' cannot be in a Windows path).
    let list: Vec<String> = made.iter().map(|(link, folder)| format!("{}|{}", link.display(), folder.display())).collect();
    let script = "$ErrorActionPreference = 'Stop'; $sh = New-Object -ComObject WScript.Shell; \
                  foreach ($line in ($env:FERRITRON_SHORTCUTS -split \"`n\")) { if (-not $line) { continue }; \
                  $p = $line.Split('|'); if (Test-Path -LiteralPath $p[0]) { Remove-Item -LiteralPath $p[0] -Force }; \
                  $l = $sh.CreateShortcut($p[0]); $l.TargetPath = $p[1]; $l.Save() }";
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("FERRITRON_SHORTCUTS", list.join("\n"))
        .output()
        .map_err(|e| format!("cannot run PowerShell to make shortcuts: {e}"))?;
    if !out.status.success() {
        return Err(format!("making shortcuts failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(made)
}

#[cfg(windows)]
fn with_lnk(link: &Path) -> PathBuf {
    crate::formats::with_suffix(link, ".lnk")
}

#[cfg(unix)]
fn make_links(links: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    for (link, folder) in links {
        if std::fs::symlink_metadata(link).is_ok() {
            remove(link)?;
        }
        std::os::unix::fs::symlink(folder, link).map_err(|e| format!("cannot make the link {}: {e}", link.display()))?;
    }
    Ok(links.to_vec())
}

#[cfg(unix)]
fn remove(link: &Path) -> Result<(), String> {
    std::fs::remove_file(link).map_err(|e| format!("cannot replace {}: {e}", link.display()))
}

#[cfg(not(any(windows, unix)))]
fn make_links(_: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    Err("shortcuts are not supported on this system".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_and_replaces_a_shortcut() {
        let dir = std::env::temp_dir().join(format!("ferritron-shortcut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("folder");
        std::fs::create_dir_all(&target).unwrap();
        let links = vec![(dir.join("_Test Plugins"), target.clone())];
        let made = make(&links).unwrap();
        assert_eq!(made.len(), 1);
        assert!(std::fs::symlink_metadata(&made[0].0).is_ok(), "{}", made[0].0.display());
        // Again: the old one is replaced, not an error.
        make(&links).unwrap();
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(&made[0].0).unwrap(), target);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
