//! proot container backend for Termux/Android.
//!
//! There is no daemon here: a "container" is a single `proot` invocation.
//! Bind mounts become `-b src:dst` path translations, the fake root comes
//! from `--change-id=0:0`, and there is no readiness wait because the guest
//! needs no systemd. Instance state is tracked through a small pidfile.

use anyhow::{anyhow, Result};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use super::{BackendKind, ContainerBackend, CielInstance, InstanceHandle};
use crate::common::CIEL_INST_DIR;
use console::style;
use crate::{info, warn};

/// PATH inside the guest rootfs (AOSC OS layout)
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct ProotBackend;

fn inst_dir(inst: &InstanceHandle) -> PathBuf {
    Path::new(CIEL_INST_DIR).join(&inst.name)
}

fn pidfile_path(inst: &InstanceHandle) -> PathBuf {
    inst_dir(inst).join("run.pid")
}

fn read_pid(inst: &InstanceHandle) -> Option<i32> {
    let data = fs::read_to_string(pidfile_path(inst)).ok()?;
    data.trim().parse::<i32>().ok()
}

fn process_alive(pid: i32) -> bool {
    // signal 0 = existence probe
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The guest is a glibc rootfs: leaking Termux's loader/library environment
/// into it breaks the guest binaries, so the environment is rebuilt from
/// scratch with only what the guest actually needs.
fn sanitize_env(cmd: &mut Command) {
    use std::env;
    cmd.env_clear();
    cmd.env("HOME", "/root");
    cmd.env("PATH", GUEST_PATH);
    cmd.env(
        "TERM",
        env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
    );
    if env::var_os("CIEL_STAGE2").is_some() {
        cmd.env("ABSTAGE2", "1");
    }
    if env::var_os("CIEL_PROOT_NO_SECCOMP").is_some() {
        // Escape hatch for kernels that SIGSYS-kill traced processes
        cmd.env("PROOT_NO_SECCOMP", "1");
    }
}

/// nspawn copies the host resolv.conf into the container automatically,
/// proot does not — find a usable host resolv.conf to bind.
///
/// `pub(crate)` because `ciel doctor` reports it: a guest without DNS cannot
/// install packages, and on Android `/etc/resolv.conf` genuinely does not
/// exist, so this lookup failing is a common and otherwise silent setup gap.
pub(crate) fn host_resolv_conf() -> Option<PathBuf> {
    let etc = Path::new("/etc/resolv.conf");
    if etc.is_file() {
        return Some(etc.to_path_buf());
    }
    if let Ok(prefix) = std::env::var("PREFIX") {
        let candidate = Path::new(&prefix).join("etc/resolv.conf");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn build_argv(inst: &InstanceHandle, args: &[&OsStr]) -> Result<Vec<OsString>> {
    let root = &inst.root;
    if !root.join("usr").is_dir() {
        return Err(anyhow!(
            "Instance root `{}` is not initialized. Try re-creating the instance with `ciel add {}`.",
            root.display(),
            inst.name
        ));
    }

    let mut argv: Vec<OsString> = Vec::new();
    argv.push("--kill-on-exit".into());
    argv.push("--link2symlink".into());
    argv.push("--change-id=0:0".into());
    argv.push(OsString::from(format!("--rootfs={}", root.display())));
    argv.push("-w".into());
    argv.push("/root".into());
    // Guest pseudo-filesystems mostly come from the host (Android).
    //
    // /proc and /sys are bound wholesale: nothing inside the guest ever
    // needs to *create* entries under them (their payload directories
    // exist in the host trees), so the bind is transparent to dpkg.
    //
    // /dev is deliberately NOT bound wholesale. aosc-aaa ships `./dev/shm`
    // in its payload, and dpkg must mkdir it during unpack; with a
    // wholesale bind the guest `./dev` would be Android's real /dev — a
    // root-owned tmpfs the app process cannot write into — so that mkdir
    // dies with EACCES ("error creating directory './dev/shm'") and the
    // whole upgrade aborts. Binding only the individual character devices
    // leaves the guest `/dev` as the rootfs's own directory (seeded from
    // the base layer, owned by us), which dpkg is free to create and
    // chmod, while the nodes still resolve through path translation.
    // (Verified empirically on-device: dropping the /dev bind makes
    // `dpkg --unpack aosc-aaa` succeed; keeping it always fails.)
    for p in ["/proc", "/sys"] {
        argv.push("-b".into());
        argv.push(p.into());
    }
    for node in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
    ] {
        // Some nodes may be absent in exotic environments; skip silently.
        if !Path::new(node).exists() {
            continue;
        }
        argv.push("-b".into());
        argv.push(node.into());
    }

    // PTY support, bound from the host like /proc and /sys above.
    //
    // `acbs-build` starts the build through pexpect
    // (acbs/utils.py:214 start_build_capture -> pexpect.spawn('autobuild')),
    // and pexpect -> ptyprocess -> pty.fork() -> openpty() needs BOTH
    // /dev/ptmx (posix_openpt) and a resolvable /dev/pts/N (the slave).
    // The AOSC rootfs ships neither, so without these binds every build
    // dies with Python's `OSError: out of pty devices`.
    //
    // They are a pair on purpose: binding ptmx alone lets posix_openpt()
    // succeed but leaves the slave side unresolvable, so openpty() fails
    // anyway. Gate on both. Verified on-device 2026-10-06 with the
    // CIEL_PROOT_EXTRA_OPTIONS equivalent of exactly these two binds
    // (docs/M3-TEST-PLAN.md sections 7-9): with them, posix_openpt, openpty,
    // forkpty+exec-with-readback and pexpect.spawn all succeed in the
    // guest; without them, all fail with ENOENT.
    //
    // /dev/pts is Android's devpts instance shared with Termux (pty
    // numbers live in one namespace) — harmless for openpty semantics,
    // and `listdir(/dev/pts)` returning EACCES is normal (Android does
    // not let apps enumerate /dev/pts; openpty never needs to).
    if Path::new("/dev/ptmx").exists() && Path::new("/dev/pts").is_dir() {
        argv.push("-b".into());
        argv.push("/dev/ptmx".into());
        argv.push("-b".into());
        argv.push("/dev/pts".into());
    }

    // Workspace bind mounts (DEFAULT_MOUNTS + config adjustments)
    let (extra_options, mounts) = crate::actions::ensure_host_sanity()?;
    if !extra_options.is_empty() {
        warn!(
            "nspawn-extra-options are not supported by the proot backend, ignoring {} option(s)",
            extra_options.len()
        );
    }
    for (src, dst, _read_write) in &mounts {
        let src_abs = fs::canonicalize(src)
            .map_err(|e| anyhow!("Cannot resolve bind source `{}`: {}", src, e))?;
        argv.push("-b".into());
        argv.push(OsString::from(format!("{}:{}", src_abs.display(), dst)));
    }
    // NOTE: proot has no read-only bind — `/tree` is mounted read-write and
    // protected by convention only.

    if let Some(resolv) = host_resolv_conf() {
        argv.push("-b".into());
        argv.push(OsString::from(format!(
            "{}:/etc/resolv.conf",
            resolv.display()
        )));
    }

    if let Ok(extra) = std::env::var("CIEL_PROOT_EXTRA_OPTIONS") {
        for opt in extra.split_whitespace() {
            // proot is not a shell, and bash does not expand `~` inside
            // double quotes either (`export X="-b ~/foo"` stores a literal
            // `~/foo`). A bind source left as `~/...` would be treated as a
            // relative path and the bind would fail, so expand a leading
            // `~/` against $HOME. Mirrors ciel-proot.sh.
            // (Verified 2026-10-06: both the harness and this path were
            // affected — see docs/M3-TEST-PLAN.md section 7.)
            if let Some(rest) = opt.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    let mut path = PathBuf::from(home);
                    path.push(rest);
                    argv.push(path.into_os_string());
                    continue;
                }
            }
            argv.push(opt.into());
        }
    }

    // NOTE: no `--` separator here. proot uses a hand-rolled option scanner
    // (src/cli/cli.c), not getopt: it walks argv and stops at the first entry
    // that does not start with '-', treating the rest as the command. A bare
    // "--" starts with '-', so it gets matched against the option table, finds
    // nothing, and aborts with `unknown option '--'.`
    //
    // Every command ciel runs is an absolute path (`/bin/bash`,
    // `/bin/acbs-build`, ...), so option scanning always terminates on the
    // first guest argument and the separator is unnecessary.
    match args.first() {
        // An empty command makes proot fall back to /bin/sh (argv[argc_offset]
        // is NULL), which is exactly what we want.
        None => {}
        // proot would parse this as one of its own options instead of the
        // guest command's first word; fail loudly rather than misbehave.
        Some(first) if first.to_string_lossy().starts_with('-') => {
            return Err(anyhow!(
                "Cannot run `{}` in instance {}: proot has no `--` \
                 end-of-options marker, so a command starting with `-` cannot \
                 be told apart from a proot option.",
                first.to_string_lossy(),
                inst.name
            ));
        }
        Some(_) => {}
    }
    argv.extend(args.iter().map(|s| s.to_os_string()));
    Ok(argv)
}

impl ContainerBackend for ProotBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Proot
    }

    fn execute(&self, inst: &InstanceHandle, args: &[&OsStr]) -> Result<i32> {
        let argv = build_argv(inst, args)?;
        let mut cmd = Command::new("proot");
        cmd.args(&argv);
        sanitize_env(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
            anyhow!(
                "Failed to launch proot: {} (is the `proot` package installed?)",
                e
            )
        })?;
        fs::create_dir_all(inst_dir(inst))?;
        fs::write(pidfile_path(inst), child.id().to_string())?;
        let code = child.wait()?.code().unwrap_or(127);
        let _ = fs::remove_file(pidfile_path(inst));
        Ok(code)
    }

    fn stop(&self, inst: &InstanceHandle) -> Result<()> {
        let pid = match read_pid(inst) {
            Some(pid) if process_alive(pid) => pid,
            _ => {
                let _ = fs::remove_file(pidfile_path(inst));
                return Ok(());
            }
        };
        info!("{}: terminating proot process (pid {}) ...", inst.name, pid);
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        for _ in 0..30 {
            if !process_alive(pid) {
                break;
            }
            sleep(Duration::from_millis(100));
        }
        if process_alive(pid) {
            warn!("{}: proot did not exit, sending SIGKILL ...", inst.name);
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = fs::remove_file(pidfile_path(inst));
        Ok(())
    }

    fn inspect(&self, inst: &InstanceHandle) -> Result<CielInstance> {
        let link_mounted = std::env::current_dir()
            .map(|ws| {
                fs::symlink_metadata(ws.join(&inst.name))
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let mounted = link_mounted && inst.root.join("usr").is_dir();
        let running = read_pid(inst).map(process_alive).unwrap_or(false);
        Ok(CielInstance {
            name: inst.name.clone(),
            ns_name: inst.ns_name.clone(),
            mounted,
            running,
            started: running,
            booted: None,
        })
    }

    fn clean_children(&self) {}
}
