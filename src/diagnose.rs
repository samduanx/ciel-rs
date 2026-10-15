//! `ciel doctor` — environment checks.
//!
//! Every check here corresponds to a real assumption made somewhere in this
//! crate: a binary we spawn, a kernel facility we depend on, or a workspace
//! file we are about to open. A check that does not map to one of those is
//! noise, and a missing one lets a failure surface somewhere much later and
//! much less legibly — which is how a missing `unsquashfs` used to become an
//! empty `dist/` and then a baffling `No such file or directory` from
//! `ciel shell`.
//!
//! The check list is selected by the active backend (see
//! `crate::backend::detect_backend`). Under proot, `systemd-nspawn`,
//! D-Bus/machined and overlayfs are *not* required and must not be reported as
//! missing; conversely the proot-specific invariants — no nested proot, bind
//! sources for `/dev` `/proc` `/sys`, symlink support for the DirCopy mount, a
//! host `resolv.conf` — are real requirements of *our* code and are checked
//! instead.
//!
//! A message starting with `!` is a warning: printed in yellow, does not fail
//! the run.

use anyhow::{anyhow, bail, Result};
use console::style;
use fs3::statvfs;
use indicatif::HumanBytes;
use std::env;
#[cfg(feature = "nspawn")]
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::mpsc::channel;
use std::{fs, fs::File, time::Duration};
use std::{io::Write, thread};
use tempfile::tempfile_in;
use which::which;
#[cfg(feature = "nspawn")]
use zbus::blocking::Connection;
#[cfg(feature = "nspawn")]
use zbus::proxy;

use crate::error;

const TEST_TEXT: &[u8] = b"An-An was born a rabbit, but found herself a girl with bunny ears and tails when she woke up one day. She couldn't seem to remember why.";

/// Hard floor for any backend: below this nothing works at all.
const DISK_FLOOR: u64 = 10 * 1024 * 1024 * 1024;
/// What the DirCopy backend actually needs. It keeps a *second* full copy of
/// the rootfs (`instances/<n>/root`) because there is no overlayfs to share a
/// single lower layer; docs/TERMUX.md measures both trees at ~14 GB.
const DISK_DIRCOPY_BUDGET: u64 = 30 * 1024 * 1024 * 1024;

#[cfg(feature = "nspawn")]
const TEST_CASES_NSPAWN: &[&dyn Fn() -> Result<String>] = &[
    &test_backend,
    &test_sd_bus,
    &test_required_binaries,
    &test_git,
    &test_io_simple,
    &test_squashfs_extractor,
    &test_fs_support,
    &test_vm_container,
    &test_workspace,
    &test_disk_io,
    &test_disk_space,
    &test_editor,
];

const TEST_CASES_PROOT: &[&dyn Fn() -> Result<String>] = &[
    &test_backend,
    &test_not_nested_proot,
    &test_required_binaries_proot,
    &test_proot_version,
    &test_pseudo_filesystems,
    &test_git,
    &test_io_simple,
    &test_squashfs_extractor,
    &test_symlink_support,
    &test_resolv_conf,
    &test_workspace,
    &test_disk_io,
    &test_disk_space,
    &test_editor,
];

/// Which check list applies to the host we are running on.
fn test_cases() -> &'static [&'static dyn Fn() -> Result<String>] {
    #[cfg(feature = "nspawn")]
    if crate::backend::detect_backend_kind() != crate::backend::BackendKind::Proot {
        return TEST_CASES_NSPAWN;
    }
    TEST_CASES_PROOT
}

// ---------------------------------------------------------------------------
// Checks shared by both backends
// ---------------------------------------------------------------------------

/// Report which backend is in use and, more usefully, *why*.
///
/// A backend picked by auto-detection is indistinguishable from one pinned in
/// `config.toml` unless the rule is spelled out, and "which backend am I
/// actually on" is the first question whenever the environment behaves oddly.
fn test_backend() -> Result<String> {
    use crate::backend::BackendKind;
    let (kind, reason) = crate::backend::detect_backend();
    let name = match kind {
        BackendKind::Proot => "proot",
        BackendKind::Nspawn => "nspawn",
    };
    Ok(format!("Container backend: {name} ({reason})"))
}

/// `ciel` shells out to `git` for `git rebase` when resetting the tree.
fn test_git() -> Result<String> {
    match which("git") {
        Ok(p) => Ok(format!("`git` found: {}", p.display())),
        Err(_) => Err(anyhow!(
            "`git` not found in PATH — ciel runs `git rebase` to reset the tree \
             (on Termux: `pkg install git`)"
        )),
    }
}

/// The rootfs is a `.squashfs`, so an extractor must exist for both backends.
/// Which binary satisfies this is distribution-specific, hence the check lives
/// in `squashfs` rather than in a hardcoded list.
fn test_squashfs_extractor() -> Result<String> {
    // `resolve()` rather than just listing what is installed: this is the exact
    // call `extract_system_rootfs` makes, so a bogus `CIEL_SQUASHFS_EXTRACTOR`
    // is caught here instead of after a 1 GB download has already been unpacked
    // halfway. Its error text also names the package to install per platform.
    let extractor = crate::squashfs::Extractor::resolve()?;
    let others: Vec<&str> = crate::squashfs::available_extractors()
        .into_iter()
        .filter(|name| *name != extractor.binary())
        .collect();
    Ok(if others.is_empty() {
        format!("SquashFS extractor: {}", extractor.describe())
    } else {
        format!(
            "SquashFS extractor: {} (also available: {})",
            extractor.describe(),
            others.join(", ")
        )
    })
}

/// Upstream opened `/proc/1/cmdline`. Android hides most of `/proc/1` from
/// unprivileged apps, so on Termux this reported a permission error on a
/// perfectly healthy system. The point of the check is "can this process do
/// basic I/O", not "can it inspect init" — read our own cmdline instead.
fn test_io_simple() -> Result<String> {
    File::open("/proc/self/cmdline")?;
    Ok("Basic I/O operations seem to be working".to_string())
}

/// Report the state of the workspace we are standing in.
///
/// Almost all of ciel's state lives under `.ciel/`, and
/// `is_legacy_workspace()` opens `.ciel/version` unconditionally — so running
/// ciel outside a workspace dies with a bare ENOENT that names neither the
/// directory nor the command that wanted it. An unusable `dist/` is the other
/// documented way to waste an afternoon (see docs/TERMUX.md), so verify it with the
/// same function the extraction path uses rather than a weaker ad-hoc probe.
fn test_workspace() -> Result<String> {
    let cwd = env::current_dir()?;
    let ciel = cwd.join(".ciel");
    if !ciel.is_dir() {
        return Ok(format!(
            "!{} is not a ciel workspace (no .ciel/) — run `ciel init` here first",
            cwd.display()
        ));
    }

    let version = fs::read_to_string(ciel.join("version")).unwrap_or_default();
    let version = version.trim().to_owned();
    let mut msg = if version.is_empty() {
        format!("ciel workspace at {}", cwd.display())
    } else if crate::common::is_legacy_workspace().unwrap_or(false) {
        format!("!ciel v{version} workspace (legacy layout) — migrate with `ciel init --upgrade`")
    } else {
        format!("ciel v{version} workspace at {}", cwd.display())
    };

    let dist = Path::new(crate::common::CIEL_DIST_DIR);
    if let Err(e) = crate::squashfs::verify_extracted_rootfs(dist) {
        msg.push_str(&format!("; base layer unusable: {e}"));
    }

    let instances = crate::backend::list_instances_simple().unwrap_or_default();
    if instances.is_empty() {
        msg.push_str("; no instances yet (run `ciel add <instance>`)");
    } else {
        let seeded = instances
            .iter()
            .filter(|name| {
                Path::new(crate::common::CIEL_INST_DIR)
                    .join(name)
                    .join("manifest/seeded")
                    .is_file()
            })
            .count();
        msg.push_str(&format!(
            "; {seeded}/{} instance(s) seeded",
            instances.len()
        ));
    }

    Ok(msg)
}

/// Mirrors `config::get_default_editor()`: `VISUAL`, then `EDITOR`, then
/// `editor` from PATH, then the literal `nano`.
///
/// Upstream only looked at `EDITOR` and `editor`, so on Termux — which has no
/// `editor` binary and usually no `EDITOR` — it failed a command that would in
/// fact have worked through the `nano` fallback. The editor is only needed for
/// the interactive "edit sources.list" prompt, so this is never fatal.
fn test_editor() -> Result<String> {
    let editor = crate::config::get_default_editor();
    let program = editor
        .to_string_lossy()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    match which(&program) {
        Ok(p) => Ok(format!("`ciel config` will use editor: {}", p.display())),
        Err(_) => Ok(format!(
            "!(only needed for `ciel config`'s sources.list prompt) no editor \
             found; ciel would fall back to `{program}`, which is not in PATH \
             either"
        )),
    }
}

fn test_disk_io() -> Result<String> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        // Send the real error instead of unwrapping: `rx` is dropped when the
        // recv below times out, and a panic in this detached thread would turn
        // a slow disk into an unexplained crash instead of a diagnosis.
        let _ = tx.send(
            tempfile_in("./")
                .and_then(|mut f| f.write_all(TEST_TEXT))
                .map_err(|e| e.to_string()),
        );
    });

    // 30s, not upstream's 10s: this writes ~170 bytes to the workspace
    // filesystem, which on Android may be FUSE-backed and under load. A false
    // "Disk I/O is not working correctly" is exactly the kind of confident
    // misdiagnosis this fork keeps having to undo.
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(())) => Ok("Disk I/O seems ok".to_string()),
        Ok(Err(e)) => Err(anyhow!(
            "Cannot write a temporary file in the workspace: {e}"
        )),
        Err(_) => {
            error!("The test file is taking too long to write, suspecting I/O stuck.");
            Err(anyhow!("Disk I/O is not working correctly"))
        }
    }
}

/// Disk check with a threshold that matches the active backend.
///
/// The proot backend's budget is not the nspawn one: with no overlayfs,
/// `DirCopyManager` holds a full second copy of the rootfs. Reporting only
/// upstream's 10 GB would wave through a phone that cannot finish seeding, and
/// then fail at the least informative possible moment.
fn test_disk_space() -> Result<String> {
    let stats = statvfs(fs::canonicalize(".")?)?;
    let available = stats.available_space();
    let reported = format!(
        "{} free of {}",
        HumanBytes(available),
        HumanBytes(stats.total_space())
    );

    if available < DISK_FLOOR {
        return Err(anyhow!(
            "Disk space insufficient: {reported}. At least {} is required.",
            HumanBytes(DISK_FLOOR)
        ));
    }

    let dircopy = matches!(
        crate::backend::detect_backend_kind(),
        crate::backend::BackendKind::Proot
    );
    if dircopy && available < DISK_DIRCOPY_BUDGET {
        return Ok(format!(
            "!{reported} — under the ~{} the DirCopy backend needs (it keeps a \
             second full copy of the rootfs; see docs/TERMUX.md)",
            HumanBytes(DISK_DIRCOPY_BUDGET)
        ));
    }

    Ok(format!("Disk space is sufficient ({reported})"))
}

// ---------------------------------------------------------------------------
// proot-specific checks
// ---------------------------------------------------------------------------

/// `ProotBackend::execute` spawns `proot`, nothing else.
fn test_required_binaries_proot() -> Result<String> {
    match which("proot") {
        Ok(p) => Ok(format!("`proot` found: {}", p.display())),
        Err(_) => Err(anyhow!(
            "Required program `proot` is not found (on Termux: `pkg install proot`)"
        )),
    }
}

fn test_proot_version() -> Result<String> {
    let output = std::process::Command::new("proot")
        .arg("--version")
        .output()
        .map_err(|e| anyhow!("Cannot execute proot: {}", e))?;
    if !output.status.success() {
        return Err(anyhow!("proot --version returned a non-zero status"));
    }
    // Some builds print the banner on stderr, so do not insist on stdout.
    let banner = String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .find(|line| line.contains("proot"))
        .unwrap_or("proot")
        .trim()
        .to_owned();
    Ok(banner)
}

/// The one invariant this fork cannot work without.
///
/// proot is a ptrace-based tracer and the kernel allows exactly one tracer per
/// process, so a proot started inside a proot-distro session cannot attach to
/// its child and the guest simply never runs. Nothing about the surrounding
/// shell looks wrong, which is why docs/TERMUX.md states this as a hard rule
/// ("ciel must not run inside proot-distro").
fn test_not_nested_proot() -> Result<String> {
    let status = fs::read_to_string("/proc/self/status")?;
    let tracer = status
        .lines()
        .find_map(|line| line.strip_prefix("TracerPid:"))
        .and_then(|value| value.trim().parse::<i32>().ok())
        .unwrap_or(0);

    if tracer != 0 {
        let l2s = if env::var_os("PROOT_L2S_DIR").is_some() {
            " (PROOT_L2S_DIR is set, so this is almost certainly proot-distro)"
        } else {
            ""
        };
        bail!(
            "Something is already ptrace-tracing this process (TracerPid={tracer}){l2s}. \
             proot has to be the tracer itself, so a container started from here \
             will fail to run. Run ciel from a native Termux shell, not from \
             inside proot-distro."
        );
    }

    Ok("Not running inside another proot".to_string())
}

/// `build_argv` binds `/dev`, `/proc` and `/sys` unconditionally — a host
/// missing any of them fails every guest command at startup, with the error
/// surfacing from inside the container rather than from the bind list.
fn test_pseudo_filesystems() -> Result<String> {
    let missing: Vec<&str> = ["/dev", "/proc", "/sys"]
        .into_iter()
        .filter(|path| !Path::new(path).exists())
        .collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "Host is missing {} — the proot backend binds all of them \
             unconditionally",
            missing.join(", ")
        ));
    }
    Ok("Host provides /dev, /proc and /sys for the guest binds".to_string())
}

/// A host `resolv.conf` for the guest to borrow.
///
/// systemd-nspawn copies the host's `resolv.conf` into the container by
/// itself; proot has to be handed one. On Android `/etc/resolv.conf` does not
/// exist — Termux keeps it under `$PREFIX` — and with neither present the guest
/// has no DNS, so every package download inside the container fails.
fn test_resolv_conf() -> Result<String> {
    match crate::backend::host_resolv_conf() {
        Some(path) => Ok(format!(
            "Host resolv.conf for the guest: {}",
            path.display()
        )),
        None => Ok("!no host resolv.conf found (`/etc/resolv.conf` nor \
             `$PREFIX/etc/resolv.conf`) — the guest will have no DNS, so \
             package downloads inside the container will fail"
            .to_string()),
    }
}

/// `DirCopyManager::mount` materialises a mounted instance as the symlink
/// `<workspace>/<instance>` -> `.ciel/container/instances/<n>/root`.
///
/// Android's shared-storage filesystems (FUSE: `/sdcard`,
/// `/storage/emulated/0`) cannot create symlinks at all, and the resulting
/// `ENOTSUP` appears far away from its cause. docs/TERMUX.md already says "keep the
/// workspace under `$HOME`"; this turns that advice into a check.
fn test_symlink_support() -> Result<String> {
    use std::os::unix::fs::symlink;

    let dir = env::current_dir()?;
    // A named temp file rather than a fixed name: the symlink target has to be
    // a real path we can point at, and must not collide with anything already
    // in the workspace. tempfile removes it on drop, error paths included.
    let target = tempfile::Builder::new()
        .prefix(".ciel-doctor-")
        .tempfile_in(&dir)
        .map_err(|e| anyhow!("Cannot create a temporary file in {}: {e}", dir.display()))?;

    let link = dir.join(".ciel-doctor-symlink");
    let _ = fs::remove_file(&link);
    let created = symlink(target.path(), &link);
    let _ = fs::remove_file(&link);

    match created {
        Ok(()) => Ok(format!("Symlinks work in {}", dir.display())),
        Err(e) => Err(anyhow!(
            "Cannot create a symlink in {}: {e}. The proot backend mounts an \
             instance as the symlink `<workspace>/<instance>` -> \
             `.ciel/container/instances/<n>/root`, so this filesystem cannot \
             host a ciel workspace. Keep the workspace under `$HOME`, never on \
             `/sdcard` or `/storage/emulated/0`.",
            dir.display()
        )),
    }
}

// ---------------------------------------------------------------------------
// nspawn-specific checks
// ---------------------------------------------------------------------------

#[cfg(feature = "nspawn")]
const TEST_PROGRAMS: &[&str] = &["systemd-nspawn", "systemd-run"];

#[cfg(feature = "nspawn")]
#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Systemd1Manager {
    /// Version property
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    /// Virtualization property
    #[zbus(property)]
    fn virtualization(&self) -> zbus::Result<String>;
}

#[cfg(feature = "nspawn")]
fn test_sd_bus() -> Result<String> {
    let conn = Connection::system()?;
    let proxy = Systemd1ManagerProxyBlocking::new(&conn)?;
    let version: String = proxy.version()?;
    Ok(format!(
        "Systemd D-Bus (systemd {}) seems to be working",
        version
    ))
}

#[cfg(feature = "nspawn")]
fn test_required_binaries() -> Result<String> {
    let missing: Vec<&str> = TEST_PROGRAMS
        .iter()
        .copied()
        .filter(|binary| which(binary).is_err())
        .collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "Required programs not found: {}",
            missing.join(", ")
        ));
    }
    Ok("Required binaries are correctly installed".to_string())
}

#[cfg(feature = "nspawn")]
fn test_fs_support() -> Result<String> {
    let f = File::open("/proc/filesystems")?;
    let reader = BufReader::new(f);
    for line in reader.lines() {
        let line = line?;
        let mut fs_type = line.splitn(2, '\t');
        if let Some(fs_type) = fs_type.nth(1) {
            if fs_type == "overlay" {
                return Ok("Filesystem support seems to be sufficient".to_string());
            }
        }
    }

    Err(anyhow!(
        "Kernel does not support overlayfs, try `modprobe overlay`"
    ))
}

#[cfg(feature = "nspawn")]
fn test_vm_container() -> Result<String> {
    let conn = Connection::system()?;
    let proxy = Systemd1ManagerProxyBlocking::new(&conn)?;
    let virt: String = proxy.virtualization()?;
    if virt == "wsl" {
        return Ok("!WSL is not supported".to_string());
    }
    let virt_msg = if virt.is_empty() {
        String::new()
    } else {
        format!("(running in {})", virt)
    };
    Ok(format!("Environment seems sane {}", virt_msg))
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Carry out the diagnostic tests.
///
/// Every check runs, even after one fails: on a phone, fixing one problem per
/// invocation is an expensive loop, and stopping at the first failure hides the
/// second fault behind the first. The exit status still reflects whether
/// anything failed.
///
/// Output is streamed rather than collected and printed at the end, so a slow
/// check cannot leave the user staring at a blank terminal wondering whether
/// ciel hung.
pub fn run_diagnose() -> Result<()> {
    let mut failures = 0usize;
    let mut warnings = 0usize;

    for test in test_cases() {
        match test() {
            Ok(msg) => match msg.strip_prefix('!') {
                Some(rest) => {
                    warnings += 1;
                    println!("{} {}", style("!").yellow(), style(rest).yellow().bold());
                }
                None => println!("{} {}", style("✓").green(), style(msg).green().bold()),
            },
            Err(err) => {
                failures += 1;
                println!("{} {}", style("x").red(), style(err).red().bold());
            }
        }
    }

    if failures > 0 {
        bail!("{failures} environment check(s) failed");
    }
    if warnings > 0 {
        println!(
            "{}",
            style(format!("{warnings} warning(s), no failures")).yellow()
        );
    }

    Ok(())
}
