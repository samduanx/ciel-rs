//! Container backend abstraction.
//!
//! ciel originally talks to systemd-nspawn + machined exclusively. This module
//! introduces a backend seam so the same orchestration code can drive either
//! the upstream nspawn backend (Linux hosts with systemd) or a `proot` backend
//! (Termux/Android, where `mount(2)`, namespaces and D-Bus are unavailable).
//!
//! The proot model collapses the "container" into a single process invocation:
//! there is no daemon, no machine registration and no cross-namespace mount
//! helper — bind mounts become plain `proot -b src:dst` arguments.

use anyhow::{anyhow, Result};
use console::style;
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::LazyLock,
};
use tabwriter::TabWriter;
use which::which;

use crate::common::{is_legacy_workspace, CIEL_DIST_DIR, CIEL_INST_DIR};
use crate::logging::color_bool;
use crate::warn;

#[cfg(feature = "nspawn")]
mod nspawn;
pub(crate) mod proot;

/// Instance status information (shared by all backends)
#[derive(Debug)]
#[allow(dead_code)] // `ns_name` is only consumed by the nspawn backend
pub struct CielInstance {
    pub name: String,
    pub ns_name: String,
    pub mounted: bool,
    pub running: bool,
    pub started: bool,
    pub booted: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Nspawn,
    Proot,
}

/// Handle addressing one instance from the backend's point of view
#[derive(Debug, Clone)]
pub struct InstanceHandle {
    pub name: String,
    pub ns_name: String,
    /// The directory the container should treat as its rootfs
    pub root: PathBuf,
}

pub trait ContainerBackend: Send + Sync {
    #[allow(dead_code)]
    fn kind(&self) -> BackendKind;
    /// Execute a command inside the instance, blocking until it exits
    fn execute(&self, inst: &InstanceHandle, args: &[&OsStr]) -> Result<i32>;
    /// Stop any process previously started for this instance
    fn stop(&self, inst: &InstanceHandle) -> Result<()>;
    fn inspect(&self, inst: &InstanceHandle) -> Result<CielInstance>;
    /// Reap exited children (the nspawn backend leaves zombies behind)
    fn clean_children(&self);
}

/// Layer management abstraction (formerly owned by overlayfs.rs)
pub trait LayerManager {
    /// Create a new layer manager from the given distribution directory
    /// dist: distribution directory, inst: instance name (not directory)
    fn from_inst_dir<P: AsRef<Path>>(
        dist_path: P,
        inst_path: P,
        inst_name: P,
    ) -> Result<Box<dyn LayerManager>>
    where
        Self: Sized;
    /// Mount the filesystem to the given path
    fn mount(&mut self, to: &Path) -> Result<()>;
    /// Return if the filesystem is mounted
    fn is_mounted(&self, target: &Path) -> Result<bool>;
    /// Rollback the filesystem to the distribution state
    fn rollback(&mut self) -> Result<()>;
    /// Commit the current state of the instance filesystem to the distribution state
    fn commit(&mut self) -> Result<()>;
    /// Un-mount the filesystem
    fn unmount(&mut self, target: &Path) -> Result<()>;
    /// Return the directory where the configuration layer is located
    /// You may temporary mount this directory if your backend does not expose this directory directly
    fn get_config_layer(&mut self) -> Result<PathBuf>;
    /// Set the volatile state of the instance filesystem
    fn set_volatile(&mut self, volatile: bool) -> Result<()>;
    /// Destroy the filesystem of the current instance
    fn destroy(&mut self) -> Result<()>;
}

static BACKEND: LazyLock<Box<dyn ContainerBackend>> = LazyLock::new(|| {
    match detect_backend_kind() {
        #[cfg(feature = "nspawn")]
        BackendKind::Nspawn => Box::new(nspawn::NspawnBackend),
        _ => Box::new(proot::ProotBackend),
    }
});

fn get_backend() -> &'static dyn ContainerBackend {
    &**BACKEND
}

/// Decide which backend to use, together with a human-readable reason.
///
/// The reason is not cosmetic: `ciel doctor` reports it so a surprising backend
/// choice can be traced back to the rule that produced it, instead of leaving
/// the user to guess whether an env var or a stale `config.toml` won.
///
/// Resolution order:
/// 1. `CIEL_BACKEND` environment variable (`nspawn` / `proot`)
/// 2. the `backend` key of the workspace config
/// 3. Auto-detect: Termux (or a missing systemd-nspawn) means proot
pub fn detect_backend() -> (BackendKind, String) {
    let forced = std::env::var("CIEL_BACKEND").ok().or_else(|| {
        crate::config::read_config().ok().and_then(|c| c.backend)
    });
    if let Some(v) = forced {
        match v.as_str() {
            "proot" => {
                return (
                    BackendKind::Proot,
                    "requested explicitly (CIEL_BACKEND or config.toml)".to_owned(),
                )
            }
            "nspawn" => {
                // Asking for a backend that was not compiled in must not
                // silently hand back the other one. On a proot-only build
                // nspawn is unreachable, and quietly running proot anyway is
                // how you end up debugging overlayfs errors on a phone.
                if cfg!(feature = "nspawn") {
                    return (
                        BackendKind::Nspawn,
                        "requested explicitly (CIEL_BACKEND or config.toml)".to_owned(),
                    );
                }
                warn!("`nspawn` was requested but this binary was built without the `nspawn` feature");
                return (
                    BackendKind::Proot,
                    "built without the `nspawn` feature, so proot is the only option".to_owned(),
                );
            }
            other => {
                warn!("Unknown backend `{}`, ignoring", other);
            }
        }
    }
    // Without the feature there is nothing to fall back to, so do not pretend
    // the host might qualify.
    if !cfg!(feature = "nspawn") {
        return (
            BackendKind::Proot,
            "built without the `nspawn` feature".to_owned(),
        );
    }
    if std::env::var_os("TERMUX_VERSION").is_some() {
        return (
            BackendKind::Proot,
            "running inside Termux (TERMUX_VERSION is set)".to_owned(),
        );
    }
    if which("systemd-nspawn").is_err() {
        return (
            BackendKind::Proot,
            "`systemd-nspawn` is not installed".to_owned(),
        );
    }
    (
        BackendKind::Nspawn,
        "`systemd-nspawn` is installed and this is not Termux".to_owned(),
    )
}

/// The active backend, discarding the reason.
pub fn detect_backend_kind() -> BackendKind {
    detect_backend().0
}

// ---------------------------------------------------------------------------
// Instance naming (kept in sync with machine.rs, which stays byte-identical
// to upstream to keep the fork mergeable)
// ---------------------------------------------------------------------------

/// Used for getting the instance name from Ciel 1/2
///
/// Bionic (Android) does not expose `ftok` through the `libc` crate (the
/// symbol itself only exists since API 26), and Ciel 1/2 legacy workspaces
/// cannot exist on Termux — so the legacy naming is unsupported there.
#[cfg(not(target_os = "android"))]
fn legacy_container_name(path: &Path) -> Result<String> {
    let key_id;
    let current_dir = std::env::current_dir()?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid container path: {:?}", path))?;
    let mut path = current_dir.as_os_str().as_bytes().to_owned();
    path.push(0); // add trailing null terminator
    unsafe {
        // unsafe because of the `ftok` invocation
        key_id = libc::ftok(path.as_ptr() as *const libc::c_char, 0);
    }
    if key_id < 0 {
        return Err(anyhow!("ftok() failed."));
    }

    Ok(format!(
        "{}-{:x}",
        name.to_str()
            .ok_or_else(|| anyhow!("Container name is not valid unicode."))?,
        key_id
    ))
}

#[cfg(target_os = "android")]
fn legacy_container_name(_path: &Path) -> Result<String> {
    Err(anyhow!(
        "Legacy (Ciel 1/2) workspaces are not supported on Android."
    ))
}

/// Used for getting the instance name from Ciel 3+
fn new_container_name(path: &Path) -> Result<String> {
    // New container name is calculated using the following formula:
    // $name-adler32($PWD)
    let hash = adler32::adler32(path.as_os_str().as_bytes())?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid container path: {:?}", path))?;

    Ok(format!(
        "{}-{:x}",
        name.to_str()
            .ok_or_else(|| anyhow!("Container name is not valid unicode."))?,
        hash
    ))
}

/// Get the container name (ns_name) of the instance
pub fn get_container_ns_name<P: AsRef<Path>>(path: P, legacy: bool) -> Result<String> {
    let current_dir = std::env::current_dir()?;
    let path = current_dir.join(path);
    if legacy {
        warn!("You are working in a legacy workspace. Use `ciel init --upgrade` to upgrade.");
        warn!("Please make sure to save your work before upgrading.");
        return legacy_container_name(&path);
    }

    new_container_name(&path)
}

/// Resolve the effective rootfs of an instance under the active backend
fn instance_root(instance: &str) -> Result<PathBuf> {
    match detect_backend_kind() {
        BackendKind::Proot => Ok(Path::new(CIEL_INST_DIR).join(instance).join("root")),
        BackendKind::Nspawn => Ok(std::env::current_dir()?.join(instance)),
    }
}

pub fn instance_handle(instance: &str) -> Result<InstanceHandle> {
    let legacy = is_legacy_workspace()?;
    Ok(InstanceHandle {
        name: instance.to_owned(),
        ns_name: get_container_ns_name(instance, legacy)?,
        root: instance_root(instance)?,
    })
}

// ---------------------------------------------------------------------------
// Backend dispatch free functions
// (signatures follow the upstream machine.rs API so the actions layer only
// needs mechanical import changes)
// ---------------------------------------------------------------------------

/// The host `resolv.conf` the guest should borrow, if the host has one.
///
/// Re-exported from the proot backend because it is a real environment
/// requirement (a guest with no DNS cannot install packages) and `ciel doctor`
/// needs to check it through exactly the same lookup the backend will use.
pub(crate) use proot::host_resolv_conf;

/// Execute a command inside the instance
pub fn execute_container_command<S: AsRef<OsStr>>(instance: &str, args: &[S]) -> Result<i32> {
    let handle = instance_handle(instance)?;
    let args_ref: Vec<&OsStr> = args.iter().map(|x| x.as_ref()).collect();
    get_backend().execute(&handle, &args_ref)
}

/// Terminate the instance (graceful if the backend supports it)
pub fn terminate_container_by_name(instance: &str) -> Result<()> {
    let handle = instance_handle(instance)?;
    get_backend().stop(&handle)
}

/// Get the information of the instance specified
pub fn inspect_instance(instance: &str) -> Result<CielInstance> {
    let handle = instance_handle(instance)?;
    get_backend().inspect(&handle)
}

/// Reap all the exited child processes
pub fn clean_child_process() {
    get_backend().clean_children();
}

/// Prepare a container for command execution (backend dispatch).
///
/// The proot backend needs no long-lived container: bind mounts are passed
/// per-invocation, so this is a no-op there.
pub fn spawn_container(
    ns_name: &str,
    instance: &str,
    extra_options: &[String],
    mounts: &[(String, &str, bool)],
    read_write_permitted: bool,
) -> Result<()> {
    match detect_backend_kind() {
        #[cfg(feature = "nspawn")]
        BackendKind::Nspawn => crate::machine::spawn_container(
            ns_name,
            instance,
            extra_options,
            mounts,
            read_write_permitted,
        ),
        _ => {
            let _ = (ns_name, instance, mounts, read_write_permitted);
            if !extra_options.is_empty() {
                warn!("nspawn-extra-options are not supported by the proot backend, ignoring {} option(s)", extra_options.len());
            }
            Ok(())
        }
    }
}

/// Mount (or materialize) the instance filesystem at the workspace mountpoint
pub fn mount_layers(manager: &mut dyn LayerManager, name: &str) -> Result<()> {
    let target = std::env::current_dir()?.join(name);
    if !manager.is_mounted(&target)? {
        manager.mount(&target)?;
    }

    Ok(())
}

/// Get the layer manager for the active backend
pub fn get_layer_manager(inst_name: &str) -> Result<Box<dyn LayerManager>> {
    match detect_backend_kind() {
        #[cfg(feature = "nspawn")]
        BackendKind::Nspawn => crate::overlayfs::get_overlayfs_manager(inst_name),
        _ => crate::dircopy::DirCopyManager::from_inst_dir(CIEL_DIST_DIR, CIEL_INST_DIR, inst_name),
    }
}

/// Create (and seed, if the backend needs it) a new instance
pub fn create_instance(inst_name: &str) -> Result<()> {
    match detect_backend_kind() {
        #[cfg(feature = "nspawn")]
        BackendKind::Nspawn => {
            crate::overlayfs::create_new_instance_fs(CIEL_INST_DIR, inst_name)
        }
        _ => crate::dircopy::seed_instance(inst_name),
    }
}

/// List all the instances under the current directory
pub fn list_instances() -> Result<Vec<CielInstance>> {
    let mut instances: Vec<CielInstance> = Vec::new();
    for entry in (fs::read_dir(CIEL_INST_DIR)?).flatten() {
        if entry.file_type().map(|e| e.is_dir())? {
            instances.push(inspect_instance(&entry.file_name().to_string_lossy())?);
        }
    }

    Ok(instances)
}

/// List all the instances under the current directory, returns only instance names
pub fn list_instances_simple() -> Result<Vec<String>> {
    let mut instances: Vec<String> = Vec::new();
    for entry in (fs::read_dir(CIEL_INST_DIR)?).flatten() {
        if entry.file_type().map(|e| e.is_dir())? {
            instances.push(entry.file_name().to_string_lossy().to_string());
        }
    }

    Ok(instances)
}

/// Print all the instances under the current directory
pub fn print_instances() -> Result<()> {
    let instances = list_instances()?;
    let mut formatter = TabWriter::new(std::io::stderr());
    writeln!(&mut formatter, "NAME\tMOUNTED\tRUNNING\tBOOTED")?;
    for instance in instances {
        let mounted = color_bool(instance.mounted);
        let running = color_bool(instance.running);
        let booted = {
            if let Some(booted) = instance.booted {
                color_bool(booted)
            } else {
                // dim
                "\x1b[2m-\x1b[0m"
            }
        };
        writeln!(
            &mut formatter,
            "{}\t{}\t{}\t{}",
            instance.name, mounted, running, booted
        )?;
    }
    formatter.flush()?;

    Ok(())
}
