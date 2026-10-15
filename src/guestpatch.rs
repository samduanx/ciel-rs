//! Guest-side Python compatibility patch for the proot backend.
//!
//! proot's link2symlink extension (mandatory here: the host fs does not
//! support hard links) emulates `link()` by storing the real data in a
//! hidden `.l2s.tmp_*` file and turning the original name into a symlink,
//! while faking `lstat` so the guest still sees a regular file.  However
//! `getdents64` keeps reporting `DT_LNK` and `readlink()` is deliberately
//! failed with `EINVAL`, so any code that combines `d_type`-based symlink
//! detection with `readlink()` blows up.  CPython's `shutil._copytree`
//! does exactly that, which broke acbs's git fetcher on every package
//! whose spec uses a `git::` source (43% of ABBS).
//!
//! The fix ships as a guest-side `sitecustomize.py` (embedded below via
//! `include_str!`, canonical copy at `scripts/guest-patches/`): Python
//! loads it automatically at interpreter startup, and it replaces
//! `shutil.copytree` with an l2s-aware version.  Full analysis lives in
//! `docs/M3-TEST-PLAN.md` §14.
//!
//! Installed into both the pristine base layer (`dist`) and the live
//! instance root right after each rollback sync, so every build starts
//! with the patch in place.  Idempotent: files are rewritten only when
//! their content differs.  Best-effort: a failure here degrades to the
//! pre-fix behaviour (tarball-sourced packages still build), it never
//! aborts the build.
//!
//! ## oma `no_check_dbus`
//!
//! The same two layers also get `/etc/oma.toml` with `no_check_dbus`
//! set to `true`.  Without it, every oma invocation that autobuild4's
//! `proc/11-pm-deps.sh` makes during "Installing dependencies from
//! repository..." stops with an interactive "Do you still wish to
//! continue?" prompt (oma cannot see a session/system bus inside the
//! container), which breaks unattended `ciel build` runs.  ciel's own
//! update script already passes `--no-check-dbus`; the config file is
//! the only way to cover oma calls made by other programs (autobuild4
//! does not pass the flag).  This is the exact remedy oma's own hint
//! recommends for permanently containerised systems.  The file is
//! merged, not clobbered, so a distro-shipped oma.toml keeps its other
//! settings.

use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use console::style; // the info! macro expands `style()` at the call site
use crate::info;

const SITECUSTOMIZE_PY: &str = include_str!("../scripts/guest-patches/sitecustomize.py");
const PATCH_NAME: &str = "sitecustomize.py";

const OMA_TOML_REL: &str = "etc/oma.toml";
const OMA_TOML_MINIMAL: &str = "[general]\nno_check_dbus = true\n";

/// Install the guest Python patch into both layers, idempotently.
pub fn ensure_installed(dist: &Path, root: &Path) -> Result<()> {
    for layer in [dist, root] {
        if let Some(sp) = find_site_packages(layer) {
            let target = sp.join(PATCH_NAME);
            let stale = match fs::read_to_string(&target) {
                Ok(old) => old != SITECUSTOMIZE_PY,
                Err(_) => true,
            };
            if stale {
                fs::write(&target, SITECUSTOMIZE_PY)
                    .with_context(|| format!("writing {}", target.display()))?;
                info!("Installed guest Python patch: {}", target.display());
            }
        }
    }
    for layer in [dist, root] {
        let target = layer.join(OMA_TOML_REL);
        if ensure_oma_no_check_dbus(&target)? {
            info!("Set no_check_dbus=true in {}", target.display());
        }
    }
    Ok(())
}

/// Make sure `no_check_dbus = true` is set under `[general]` in an
/// `oma.toml`, merging into any existing content instead of clobbering
/// it.  Returns whether the file was created or modified.
fn ensure_oma_no_check_dbus(path: &Path) -> Result<bool> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::write(path, OMA_TOML_MINIMAL)
                .with_context(|| format!("writing {}", path.display()))?;
            return Ok(true);
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Case 1: the key already exists — just fix its value if needed.
    let key_pos = lines.iter().position(|l| {
        let t = l.trim();
        t.starts_with("no_check_dbus")
            && t["no_check_dbus".len()..]
                .trim_start()
                .starts_with('=')
    });
    if let Some(idx) = key_pos {
        let value = lines[idx]
            .split_once('=')
            .map(|(_, v)| v.trim())
            .unwrap_or("");
        if value.starts_with("true") {
            return Ok(false);
        }
        lines[idx] = "no_check_dbus = true".to_string();
        fs::write(path, join_toml_lines(&lines))
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(true);
    }

    // Case 2: a `[general]` section exists — add the key to it.  Note
    // that appending a second `[general]` table would make the file
    // unparseable (duplicate table), so we must insert instead.
    if let Some(idx) = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case("[general]"))
    {
        lines.insert(idx + 1, "no_check_dbus = true".to_string());
    } else {
        // Case 3: neither key nor section — append a new section.
        if !lines.is_empty() && lines.last().map_or(true, |l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("[general]".to_string());
        lines.push("no_check_dbus = true".to_string());
    }
    fs::write(path, join_toml_lines(&lines))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn join_toml_lines(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        (dir, path)
    }

    #[test]
    fn test_oma_missing_file_created() {
        let (_d, path) = scratch("oma.toml");
        assert!(ensure_oma_no_check_dbus(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), OMA_TOML_MINIMAL);
    }

    #[test]
    fn test_oma_idempotent_when_true() {
        let (_d, path) = scratch("oma.toml");
        fs::write(&path, OMA_TOML_MINIMAL).unwrap();
        assert!(!ensure_oma_no_check_dbus(&path).unwrap());
    }

    #[test]
    fn test_oma_flips_false_to_true() {
        let (_d, path) = scratch("oma.toml");
        fs::write(&path, "[general]\nno_check_dbus = false\n").unwrap();
        assert!(ensure_oma_no_check_dbus(&path).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[general]\nno_check_dbus = true\n"
        );
    }

    #[test]
    fn test_oma_inserts_into_existing_general() {
        let (_d, path) = scratch("oma.toml");
        fs::write(&path, "[general]\nyn_mode = true\n[network]\nnetwork_threads = 8\n").unwrap();
        assert!(ensure_oma_no_check_dbus(&path).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[general]\nno_check_dbus = true\nyn_mode = true\n[network]\nnetwork_threads = 8\n"
        );
    }

    #[test]
    fn test_oma_appends_general_when_absent() {
        let (_d, path) = scratch("oma.toml");
        fs::write(&path, "[network]\nnetwork_threads = 8\n").unwrap();
        assert!(ensure_oma_no_check_dbus(&path).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[network]\nnetwork_threads = 8\n\n[general]\nno_check_dbus = true\n"
        );
    }

    #[test]
    fn test_oma_empty_file() {
        let (_d, path) = scratch("oma.toml");
        fs::write(&path, "").unwrap();
        assert!(ensure_oma_no_check_dbus(&path).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[general]\nno_check_dbus = true\n"
        );
    }

    #[test]
    fn test_oma_similar_key_not_matched() {
        let (_d, path) = scratch("oma.toml");
        fs::write(&path, "[general]\nno_check_dbus_legacy = false\n").unwrap();
        assert!(ensure_oma_no_check_dbus(&path).unwrap());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("no_check_dbus_legacy = false"));
        assert!(content.contains("[general]\nno_check_dbus = true"));
    }
}

/// Locate `<layer>/usr/lib/python3.x/site-packages`, preferring the
/// highest interpreter version if several are present.
fn find_site_packages(layer: &Path) -> Option<PathBuf> {
    let usr_lib = layer.join("usr").join("lib");
    let mut best: Option<(String, PathBuf)> = None;
    for entry in fs::read_dir(&usr_lib).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let sp = entry.path().join("site-packages");
        if name.starts_with("python3") && sp.is_dir() {
            if best
                .as_ref()
                .map_or(true, |(ver, _)| name.as_ref() > ver.as_str())
            {
                best = Some((name.into_owned(), sp));
            }
        }
    }
    best.map(|(_, p)| p)
}
