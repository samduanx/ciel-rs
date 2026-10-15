//! DirCopy layer manager — a portable [`LayerManager`] for environments
//! without overlayfs (Termux/Android).
//!
//! Layout (mirroring the roles of the overlay backend):
//!   dist = `.ciel/container/dist`                — pristine base, read-only by convention
//!   root = `.ciel/container/instances/<n>/root`  — real, writable rootfs handed to proot
//!
//! `mount()` materializes the instance at the workspace mountpoint through a
//! symlink `<ws>/<instance>` -> root, so every code path that writes
//! `<ws>/<instance>/...` (e.g. the local deb repo bootstrap) keeps working
//! unchanged.
//!
//! `rollback()` restores `root` to the `dist` state with a selective
//! rsync-like sync (delete extraneous entries, copy entries that differ in
//! size+mtime), then replays the workspace configuration on top. This is the
//! proot equivalent of discarding an overlay upper layer, and it also sweeps
//! non-package-manager residue (build dirs, /tmp, ld.so.cache, apt lists).
//!
//! `commit()` promotes an updated instance root to the base layer through an
//! O(1) rename hand-over: the old base layer is kept on disk as
//! `dist.pristine-<stamp>` (the manual way back), the instance root takes its
//! place, and the instance skeleton (manifest markers) is rebuilt so the
//! instance lifecycle keeps working — a later `mount()` simply re-seeds it
//! from the new base layer. Using rename instead of a copy is load-bearing:
//! deletions an upgrade performed (uninstalled packages) must reach the new
//! base layer, and a copy-based merge would silently resurrect them at the
//! next seed. Mirrors the verified shell implementation in
//! `scripts/ciel-proot.sh` (`promote`).
//!
//! # The unprivileged-host permission problem
//!
//! Nothing here runs as root: no `CAP_CHOWN`, no `CAP_FOWNER`, no `CAP_MKNOD`,
//! no overlayfs, no namespaces, no second proot. The base layer is unpacked by
//! an unprivileged process, so every file in it belongs to the invoking user.
//!
//! That ownership is the load-bearing fact: **we may `chmod` all of `dist/`**
//! without a single capability. Everything below repairs the permission bits
//! baked into the image rather than trying to reproduce them.
//!
//! The bits in the image were authored for a rootfs and are hostile to a
//! host-side copy loop:
//!
//! * `std::fs::copy` propagates the **source** mode onto the destination.
//!   Copying `/etc/gshadow` (`0400`) therefore yields a `0400` destination —
//!   after which restoring its mtime, and *every retry of the copy itself*,
//!   fails with `EACCES`. That failure is self-perpetuating: the poisoned file
//!   stays on disk, so each rerun dies on the same entry.
//! * A source we may not read at all (`0000`, or `0200` after an unprivileged
//!   unpack) cannot even be opened.
//! * Sweeping a tree whose directories are `0500` fails on their children.
//!
//! This is invisible from inside the guest: proot fakes uid 0 over a private
//! copy, so the only bits that matter there are the executable ones, which we
//! preserve verbatim.

use std::ffi::OsStr;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;
use time::macros::format_description;
use time::OffsetDateTime;
use walkdir::WalkDir;

use console::style;

use crate::backend::LayerManager;
use crate::common::{CIEL_DIST_DIR, CIEL_INST_DIR};
use crate::{info, warn};

/// Paths that must survive a rollback: proot bind targets (their content is
/// shadowed at runtime by the binds anyway) — everything else, including
/// /etc, /var, /usr, /root and /tmp, is restored from the base layer.
const SKIP_PATHS: &[&str] = &["tree", "debs", "proc", "sys", "dev", "run"];

/// Written only after a *complete* seeding run. Its absence means the root is
/// a half-written tree that must never be mistaken for a usable instance.
const SEED_MARKER: &str = "manifest/seeded";

/// Top-level entries every usable rootfs must have before a seed counts as
/// successful. Cheap insurance against a silently half-copied instance.
const REQUIRED_IN_ROOT: &[&str] = &["usr", "usr/bin", "etc"];

/// proot's `--link2symlink` extension emulates `link(2)` by moving the data
/// into a hidden holder file and turning every "hard link" into a symlink
/// pointing at it. Those symlinks carry **absolute host paths of the instance
/// they were created in**, so the bookkeeping is instance-private state that
/// must never cross instance boundaries.
///
/// It did anyway: the 2026-10-06 shell-script promote froze the arm64
/// instance's bookkeeping into the base layer. The damage is NOT limited to
/// `var/lib/dpkg` (dpkg backs every overwritten file up with
/// `link(file, file.dpkg-tmp)`, and package payloads ship hard links such as
/// binutils' `ld.bfd -> ld`): after a promote, user-visible paths like
/// `usr/bin/ld` are themselves fake symlinks. Removing the `.l2s.*` holders
/// alone leaves those dangling, and a later `dpkg` unpack dies on them
/// (`unable to stat './usr/bin/ld' (which was about to be installed):
/// Operation not permitted`, aosc 14.0.0 update, 2026-10-15).
///
/// The repair is therefore two-sided: fake links whose holder chain still
/// resolves are **materialized** into real files (a copy — the unprivileged
/// host cannot recreate hard links), and fake links whose chain is broken
/// are dropped; the `.l2s.*` bookkeeping entries are then removed.
const L2S_PREFIX: &str = ".l2s.";

/// How many individual failures to spell out before summarising.
const MAX_REPORTED_FAILURES: usize = 8;

/// Timestamp format of the `dist.pristine-<stamp>` backup directories.
const PRISTINE_STAMP: &[time::format_description::FormatItem] =
    format_description!("[year][month][day]-[hour][minute][second]");

/// Path the outgoing base layer is renamed to during a commit.
///
/// Mirrors `scripts/ciel-proot.sh` (`dist.pristine-<stamp>`): keeping the old
/// base layer on disk makes the promote reversible by hand
/// (`rm -rf <dist> && mv <old> <dist>`) at the cost of one rootfs of disk
/// space until it is deleted. A numeric suffix guards against two commits
/// landing within the same second.
fn pristine_backup_path(dist: &Path) -> Result<PathBuf> {
    let parent = dist
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("base layer `{}` has no parent directory", dist.display()))?;
    let name = dist
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("base layer `{}` has no file name", dist.display()))?;
    let stamp = OffsetDateTime::now_utc()
        .format(&PRISTINE_STAMP)
        .context("formatting the pristine backup timestamp")?;
    for suffix in 0u32.. {
        let candidate = if suffix == 0 {
            parent.join(format!("{}.pristine-{stamp}", name))
        } else {
            parent.join(format!("{}.pristine-{stamp}-{suffix}", name))
        };
        if fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    unreachable!("the loop above only exits by returning")
}

/// Outcome of inspecting one symlink against the l2s fake-link convention.
enum L2sDisposition {
    /// Not an l2s fake — an ordinary symlink worth copying verbatim.
    Ordinary,
    /// The fake link's chain resolves to this holder file; the symlink must
    /// become a real copy of it.
    Holder(PathBuf),
    /// The fake link's chain is broken — its content is unrecoverable and
    /// the symlink has to go.
    Dead,
}

/// Follow an l2s chain (link → intermediate → final) to the holder file.
/// Returns None when the chain is broken or does not end in a regular file.
fn resolve_l2s_holder(mut path: PathBuf) -> Option<PathBuf> {
    for _ in 0..4 {
        let meta = fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink() {
            let next = fs::read_link(&path).ok()?;
            path = if next.is_absolute() {
                next
            } else {
                path.parent()?.join(next)
            };
        } else if meta.is_file() {
            return Some(path);
        } else {
            return None;
        }
    }
    None
}

/// Classify the symlink `link_abs` (living at `rel_dir` inside `base`, with
/// raw target `target`): is it an l2s fake link, and if so, where is its
/// content?
///
/// Fake links made inside this tree use relative targets (`.l2s.NAME.NNNN`
/// intermediate or `.l2s.NAME.NNNN.NNNN` final). Fake links inherited from a
/// promoted tree carry absolute host paths that may be stale (the instance
/// they point into may be gone), so the same directory inside `base` is
/// always tried as a fallback before declaring the link dead.
fn classify_l2s_link(
    link_abs: &Path,
    rel_dir: &Path,
    base: &Path,
    target: &Path,
) -> L2sDisposition {
    let is_l2s = target
        .file_name()
        .map(|n| n.to_string_lossy().starts_with(L2S_PREFIX))
        .unwrap_or(false);
    if !is_l2s {
        return L2sDisposition::Ordinary;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if target.is_absolute() {
        candidates.push(target.to_path_buf());
        if let Some(name) = target.file_name() {
            candidates.push(base.join(rel_dir).join(name));
        }
    } else if let Some(dir) = link_abs.parent() {
        candidates.push(dir.join(target));
    }

    for candidate in candidates {
        if let Some(holder) = resolve_l2s_holder(candidate) {
            return L2sDisposition::Holder(holder);
        }
    }
    L2sDisposition::Dead
}

/// Sanitize `root` of all link2symlink state: materialize every resolvable
/// fake link into a real file, drop every dead one, and remove the `.l2s.*`
/// bookkeeping entries.
///
/// Called before a promote (the instance root is about to become the base
/// layer — its bookkeeping must not be frozen into it) and during seeding
/// (the current base layer is already polluted). The whole tree is walked
/// with the runtime bind targets (`/proc`, `/sys`, `/dev`, `/run`, `tree`,
/// `debs`) pruned.
///
/// Returns (materialized, dropped). Best-effort: a leftover that refuses to
/// die is warned about and left for the next run rather than blocking.
fn sanitize_l2s_tree(root: &Path) -> (usize, usize) {
    let mut fake_links: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();
    let mut holders: Vec<(PathBuf, bool)> = Vec::new();
    for entry in WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let rel = e.path().strip_prefix(root).unwrap_or(e.path());
            !is_skip(rel)
        })
        .flatten()
    {
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if entry.file_name().to_string_lossy().starts_with(L2S_PREFIX) {
            holders.push((entry.path().to_path_buf(), entry.file_type().is_dir()));
        } else if entry.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(entry.path()) {
                let rel_dir = rel
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
                fake_links.push((entry.path().to_path_buf(), rel_dir, target));
            }
        }
    }

    // Materialize/drop fake links BEFORE removing the holders: resolution
    // may still need them (including stale absolute targets into other
    // instances that happen to still exist).
    let mut materialized = 0usize;
    let mut dropped = 0usize;
    for (link, rel_dir, target) in fake_links {
        match classify_l2s_link(&link, &rel_dir, root, &target) {
            L2sDisposition::Ordinary => {}
            L2sDisposition::Holder(holder) => {
                // Unlink the symlink first: copy_file opens the destination
                // with O_TRUNC, which would follow the fake symlink instead
                // of replacing it.
                let unlinked = fs::remove_file(&link).is_ok();
                match copy_file(&holder, &link) {
                    Ok(_) => materialized += 1,
                    Err(e) => {
                        warn!(
                            "  could not materialize link2symlink fake {}: {}",
                            link.display(),
                            e
                        );
                        dropped += 1;
                        let _ = unlinked;
                    }
                }
            }
            L2sDisposition::Dead => {
                if fs::remove_file(&link).is_err() {
                    relax_permissions(&link, 0o600);
                    let _ = fs::remove_file(&link);
                }
                dropped += 1;
            }
        }
    }

    // Children before parents so an (empty by construction) `.l2s.*` directory
    // can be removed with a plain remove_dir.
    holders.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));

    let mut removed = 0usize;
    for (path, is_dir) in holders {
        let remove = || -> io::Result<()> {
            if is_dir {
                fs::remove_dir(&path)
            } else {
                fs::remove_file(&path)
            }
        };
        if remove().is_err() {
            // Same repair-then-retry pattern as the rollback sweep: the base
            // layer ships 0500 directories we must unlock before unlinking.
            relax_permissions(&path, if is_dir { 0o700 } else { 0o600 });
            if let Err(e) = remove() {
                warn!(
                    "  could not remove link2symlink bookkeeping {}: {}",
                    path.display(),
                    e
                );
                continue;
            }
        }
        removed += 1;
    }
    (materialized, removed)
}

pub struct DirCopyManager {
    name: String,
    inst: PathBuf,
    dist: PathBuf,
    root: PathBuf,
}

fn is_skip(rel: &Path) -> bool {
    match rel.components().next() {
        Some(c) => SKIP_PATHS.iter().any(|s| c.as_os_str() == OsStr::new(s)),
        None => false,
    }
}

/// Widen the permission bits of `path` by `extra`, ignoring failures.
///
/// Only ever called on entries we own (everything the unprivileged extractor
/// produced), so this succeeds without any capability. `symlink_metadata` is
/// used deliberately: chmod must not follow a symlink and touch its target.
fn relax_permissions(path: &Path, extra: u32) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    let ftype = meta.file_type();
    if ftype.is_symlink() || (!ftype.is_dir() && !ftype.is_file()) {
        return;
    }
    let mode = meta.mode();
    if mode & extra == extra {
        return;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | extra));
}

/// Make every directory under (and including) `root` owner-traversable and
/// owner-writable, top-down, so a later `contents_first` sweep can actually
/// unlink what it finds.
///
/// The base layer is full of `0500` directories (`/etc/ssl/private`,
/// `/usr/lib/...`). Deleting their children without this fails with `EACCES`
/// and would leave rollback permanently half-applied.
fn make_removable(root: &Path) {
    for entry in WalkDir::new(root).min_depth(1).into_iter().flatten() {
        if entry.file_type().is_dir() {
            relax_permissions(entry.path(), 0o700);
        }
    }
}

/// Copy one regular file, repairing the unprivileged-host permission traps.
///
/// Returns the number of bytes copied.
///
/// Unlike `std::fs::copy` this
///  1. repairs the *source* when it is unreadable,
///  2. repairs a pre-existing read-only *destination* before `O_TRUNC`,
///  3. creates the destination `0600` instead of inheriting the source mode,
///  4. restores mtime through the writer handle it already holds, and
///  5. re-applies the original bits **plus** owner read/write, so the host side
///     (rollback, config replay) can always rewrite what it seeded.
fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut src_meta =
        fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if !src_meta.file_type().is_file() {
        bail!("{} is not a regular file", src.display());
    }
    if src_meta.mode() & 0o400 == 0 {
        // Unreadable source: we own it, so chmod alone is enough.
        fs::set_permissions(src, fs::Permissions::from_mode(src_meta.mode() | 0o600))
            .with_context(|| format!("unlocking {}", src.display()))?;
        src_meta = fs::symlink_metadata(src)?;
    }

    // A destination left read-only by an earlier (buggy, or interrupted) run
    // cannot be opened with O_TRUNC. Unlock it first — this is what turns the
    // old sticky failure back into a resumable one.
    if let Ok(dst_meta) = fs::symlink_metadata(dst) {
        if dst_meta.file_type().is_file() {
            relax_permissions(dst, 0o600);
        }
    }

    let mut reader = File::open(src).with_context(|| format!("reading {}", src.display()))?;
    let mut writer = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dst)
        .with_context(|| format!("writing {}", dst.display()))?;

    let copied = io::copy(&mut reader, &mut writer)
        .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;

    // mtime is the rollback fast-path key, so it must be preserved exactly.
    // Doing it on the handle we already hold avoids a second open, which on a
    // read-only destination would be an EACCES of its own.
    if let Ok(modified) = src_meta.modified() {
        let _ = writer.set_times(FileTimes::new().set_modified(modified));
    }

    // Re-apply the base layer's bits (notably setuid/setgid/executable) while
    // guaranteeing the owner can still read and rewrite the file.
    let mode = (src_meta.mode() & 0o7777) | 0o600;
    fs::set_permissions(dst, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode on {}", dst.display()))?;

    Ok(copied)
}

fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(dst) {
        Ok(m) if m.file_type().is_dir() => fs::remove_dir_all(dst)?,
        Ok(_) => fs::remove_file(dst)?,
        Err(_) => {}
    }
    let target = fs::read_link(src)?;
    std::os::unix::fs::symlink(&target, dst)
        .with_context(|| format!("creating symlink {} -> {:?}", dst.display(), target))?;
    Ok(())
}

/// Outcome of one copy sweep.
#[derive(Default)]
struct CopyStats {
    dirs: AtomicU64,
    files: AtomicU64,
    symlinks: AtomicU64,
    bytes: AtomicU64,
    failures: Mutex<Vec<String>>,
}

impl CopyStats {
    fn record_dir(&self) {
        self.dirs.fetch_add(1, Ordering::Relaxed);
    }
    fn record_symlink(&self) {
        self.symlinks.fetch_add(1, Ordering::Relaxed);
    }
    fn record_file(&self, bytes: u64) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    fn record_failure(&self, path: &Path, err: &anyhow::Error) {
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        if failures.len() < MAX_REPORTED_FAILURES {
            failures.push(format!("{}: {:#}", path.display(), err));
        }
    }
    fn merge(&self, other: &CopyStats) {
        self.dirs
            .fetch_add(other.dirs.load(Ordering::Relaxed), Ordering::Relaxed);
        self.symlinks
            .fetch_add(other.symlinks.load(Ordering::Relaxed), Ordering::Relaxed);
        self.files
            .fetch_add(other.files.load(Ordering::Relaxed), Ordering::Relaxed);
        self.bytes
            .fetch_add(other.bytes.load(Ordering::Relaxed), Ordering::Relaxed);
        if let Ok(extra) = other.failures.lock() {
            let mut mine = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            for failure in extra.iter() {
                if mine.len() < MAX_REPORTED_FAILURES {
                    mine.push(failure.clone());
                }
            }
        }
    }
    /// Print a one-line summary plus every recorded failure.
    ///
    /// Individual copy failures are **not** fatal. This host cannot create
    /// device nodes and cannot reproduce every exotic mode, and the guest does
    /// not need it: proot fakes uid 0 and bind-mounts `/dev`. Aborting the
    /// whole operation because one entry out of ~100k was awkward is exactly
    /// what made seeding unusable.
    fn report(&self, what: &str) {
        let dirs = self.dirs.load(Ordering::Relaxed);
        let files = self.files.load(Ordering::Relaxed);
        let links = self.symlinks.load(Ordering::Relaxed);
        let bytes = self.bytes.load(Ordering::Relaxed);
        info!(
            "{}: {} directories, {} files ({}), {} symlinks",
            what,
            dirs,
            files,
            human_bytes(bytes),
            links
        );
        let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        for failure in failures.iter() {
            warn!("  skipped {}", failure);
        }
        if !failures.is_empty() {
            warn!(
                "{}: {} could not be reproduced without root and {} skipped. \
                 Expected on Termux; harmless inside the container.",
                what,
                if failures.len() == 1 {
                    "1 entry".to_owned()
                } else {
                    format!("{} entries", failures.len())
                },
                failures.len()
            );
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Spawn at most 8 workers: this is I/O bound on the phone's flash, and
/// oversubscribing f2fs makes the journal slower, not faster.
fn copy_pool() -> Option<rayon::ThreadPool> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    rayon::ThreadPoolBuilder::new().num_threads(threads).build().ok()
}

/// Copy `files` (paths relative to `src_root`) into `dst_root` in parallel.
///
/// Parents must already exist — the callers create them in a dedicated
/// pre-order pass — but `copy_file` still creates them defensively.
fn copy_files_parallel(
    src_root: &Path,
    dst_root: &Path,
    files: &[PathBuf],
    pb: &ProgressBar,
) -> CopyStats {
    let stats = CopyStats::default();
    let done = AtomicU64::new(0);
    let total = files.len() as u64;

    let sweep = || {
        files.par_iter().for_each(|rel| {
            let src = src_root.join(rel);
            let dst = dst_root.join(rel);
            match copy_file(&src, &dst) {
                Ok(bytes) => stats.record_file(bytes),
                Err(e) => stats.record_failure(&src, &e),
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 32 == 0 || n == total {
                pb.set_position(n);
            }
        });
    };

    match copy_pool() {
        Some(pool) => pool.install(sweep),
        None => sweep(),
    }
    pb.set_position(total);
    stats
}

fn sweep_progress_bar(label: &'static str, total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "{{spinner:.green}} [{{bar:25.cyan/blue}}] {label} \
                 ({{pos}}/{{total}}, elapsed {{elapsed_precise}})"
            ))
            .unwrap(),
    );
    pb.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
    pb
}

/// Create an instance and seed its rootfs from the pristine dist layer.
pub fn seed_instance(inst_name: &str) -> Result<()> {
    let inst = Path::new(CIEL_INST_DIR).join(inst_name);
    let root = inst.join("root");
    let dist = Path::new(CIEL_DIST_DIR);
    if !dist.join("usr").is_dir() {
        bail!(
            "Base layer `{}` is missing. Run `ciel load-os` first.",
            dist.display()
        );
    }

    fs::create_dir_all(&inst)?;

    // The marker — not the mere existence of `root/usr` — is what says the
    // instance is complete. A run that died half-way leaves a tree that passes
    // an `is_dir()` check but cannot boot a package manager.
    if !inst.join(SEED_MARKER).is_file() && root.exists() {
        warn!(
            "Discarding the incomplete instance root left by a previous run of \
             `ciel add {}`.",
            inst_name
        );
        fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(&root)?;
    info!(
        "Seeding instance `{}` from the base layer (this may take a while) ...",
        inst_name
    );

    // ---- Inventory ---------------------------------------------------------
    // WalkDir yields parents before children, which is exactly the ordering
    // the directory pass below relies on.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut links: Vec<PathBuf> = Vec::new();
    let mut special = 0usize;
    let mut l2s_junk = 0usize;

    for entry in WalkDir::new(dist).into_iter() {
        let entry = entry.with_context(|| format!("walking {}", dist.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        // Never seed another instance's link2symlink bookkeeping: the
        // symlinks point at the instance that created them and would derail
        // dpkg's status backup here (see L2S_PREFIX).
        if entry.file_name().to_string_lossy().starts_with(L2S_PREFIX) {
            l2s_junk += 1;
            continue;
        }
        let rel = entry.path().strip_prefix(dist)?.to_path_buf();
        let ftype = entry.file_type();
        if ftype.is_dir() {
            dirs.push(rel);
        } else if ftype.is_symlink() {
            links.push(rel);
        } else if ftype.is_file() {
            files.push(rel);
        } else {
            // Device nodes / fifos / sockets cannot be created without
            // privileges; /dev is bind-mounted by the backend anyway.
            special += 1;
        }
    }
    if special > 0 {
        warn!(
            "Skipped {} special file(s) in the base layer (device nodes are not \
             creatable without privileges)",
            special
        );
    }
    if l2s_junk > 0 {
        warn!(
            "Skipped {} proot link2symlink bookkeeping file(s) left in the base \
             layer by an earlier promote (fake-hardlink state must not cross \
             instance boundaries)",
            l2s_junk
        );
    }

    let stats = CopyStats::default();

    // ---- Pass 1: directories, pre-order ------------------------------------
    for rel in &dirs {
        let dst = root.join(rel);
        fs::create_dir_all(&dst)
            .with_context(|| format!("creating directory {}", dst.display()))?;
        // Keep the base layer's mode but leave the owner in control: children
        // still have to be creatable and rollback has to be able to delete
        // them again.
        if let Ok(mode) = fs::symlink_metadata(&dst).map(|m| m.mode()) {
            let _ =
                fs::set_permissions(&dst, fs::Permissions::from_mode((mode & 0o7777) | 0o700));
        }
        stats.record_dir();
    }

    // ---- Pass 2: regular files, in parallel -------------------------------
    let pb = sweep_progress_bar("Seeding instance from the base layer", files.len() as u64);
    stats.merge(&copy_files_parallel(dist, &root, &files, &pb));
    pb.finish_and_clear();

    // ---- Pass 3: symlinks --------------------------------------------------
    // A dangling link is legitimate (/etc/mtab -> /proc/self/mounts), so a
    // failure here is recorded rather than raised.
    //
    // l2s fake links (see L2S_PREFIX) get special treatment: the base layer
    // may still carry them from an earlier promote, and their holders are
    // deliberately skipped above. Materialize the resolvable ones (copy the
    // holder's content over the fake symlink) and drop the dead ones — the
    // instance must never see a half-purged fake link.
    let mut l2s_materialized = 0usize;
    let mut l2s_dropped = 0usize;
    for rel in &links {
        let src = dist.join(rel);
        let dst = root.join(rel);
        let disposition = fs::read_link(&src)
            .map(|t| {
                classify_l2s_link(
                    &src,
                    rel.parent().unwrap_or_else(|| Path::new("")),
                    dist,
                    &t,
                )
            })
            .unwrap_or(L2sDisposition::Ordinary);
        match disposition {
            L2sDisposition::Ordinary => {
                if let Err(e) = copy_symlink(&src, &dst) {
                    stats.record_failure(&src, &e);
                } else {
                    stats.record_symlink();
                }
            }
            L2sDisposition::Holder(holder) => match copy_file(&holder, &dst) {
                Ok(bytes) => {
                    stats.record_file(bytes);
                    l2s_materialized += 1;
                }
                Err(e) => stats.record_failure(&dst, &e),
            },
            L2sDisposition::Dead => {
                l2s_dropped += 1;
            }
        }
    }
    if l2s_materialized > 0 || l2s_dropped > 0 {
        warn!(
            "Repaired {} link2symlink fake link(s) from the base layer and \
             dropped {} dead one(s) (left behind by an earlier promote)",
            l2s_materialized, l2s_dropped
        );
    }

    stats.report("seeded");

    // ---- Verify before declaring success -----------------------------------
    let missing: Vec<&str> = REQUIRED_IN_ROOT
        .iter()
        .copied()
        .filter(|entry| !root.join(entry).is_dir())
        .collect();
    if !missing.is_empty() {
        bail!(
            "Seeding instance `{}` did not complete: {} required top-level \
             entries missing after the copy ({missing:?}). Re-run `ciel load-os` \
             to rebuild the base layer.",
            inst_name,
            missing.len()
        );
    }

    fs::create_dir_all(inst.join("manifest"))?;
    fs::write(inst.join("manifest/backend"), "dircopy")?;
    fs::write(inst.join(SEED_MARKER), b"1")?;
    info!("Instance `{}` seeded.", inst_name);

    Ok(())
}

/// Selective restore of `root` from the pristine `dist` layer.
fn rollback_from_dist(dist: &Path, root: &Path) -> Result<()> {
    // ---- Phase 0: make the current tree removable --------------------------
    // Must precede the sweep: the base layer ships `0500` directories, and
    // unlinking their children without owner-write fails outright.
    make_removable(root);

    // ---- Phase 1: remove everything that dist does not have ----------------
    // One pre-order walk both relaxes the directories *above* the entries we
    // are about to unlink and records what has to go; the unlink then runs in
    // reverse, i.e. children before parents.
    let mut doomed: Vec<(PathBuf, bool)> = Vec::new();
    for entry in WalkDir::new(root).into_iter() {
        let entry = entry.with_context(|| format!("sweeping {}", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(root)?;
        if is_skip(rel) {
            continue;
        }
        if dist.join(rel).symlink_metadata().is_ok() {
            continue;
        }
        doomed.push((entry.path().to_path_buf(), entry.file_type().is_dir()));
    }
    for (path, is_dir) in doomed.iter().rev() {
        let result = if *is_dir {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
        if result.is_err() {
            relax_permissions(path, if *is_dir { 0o700 } else { 0o600 });
            let retry = if *is_dir {
                fs::remove_dir(path)
            } else {
                fs::remove_file(path)
            };
            retry.with_context(|| format!("removing {}", path.display()))?;
        }
    }

    // ---- Phase 2: restore differing content from dist ----------------------
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut links: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(dist).into_iter() {
        let entry = entry.with_context(|| format!("restoring from {}", dist.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(dist)?;
        if is_skip(rel) {
            continue;
        }
        let dst = root.join(rel);
        let ftype = entry.file_type();
        if ftype.is_dir() {
            fs::create_dir_all(&dst)?;
            relax_permissions(&dst, 0o700);
            dirs.push(rel.to_path_buf());
        } else if ftype.is_symlink() {
            // Fast path: same link target, nothing to do.
            let same = fs::symlink_metadata(&dst)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
                && fs::read_link(&dst).ok() == fs::read_link(entry.path()).ok();
            if !same {
                links.push(rel.to_path_buf());
            }
        } else if ftype.is_file() {
            let unchanged = match fs::symlink_metadata(&dst) {
                Ok(m) if m.file_type().is_file() => {
                    let dm = fs::symlink_metadata(entry.path())?;
                    m.len() == dm.len() && m.modified().ok() == dm.modified().ok()
                }
                _ => false,
            };
            if !unchanged {
                files.push(rel.to_path_buf());
            }
        }
    }

    let pb = sweep_progress_bar("Rolling back to the base layer", files.len() as u64);
    let stats = CopyStats::default();
    stats.merge(&copy_files_parallel(dist, root, &files, &pb));
    pb.finish_and_clear();

    for rel in &links {
        if let Err(e) = copy_symlink(&dist.join(rel), &root.join(rel)) {
            stats.record_failure(&dist.join(rel), &e);
        } else {
            stats.record_symlink();
        }
    }
    stats.dirs.store(dirs.len() as u64, Ordering::Relaxed);
    stats.report("rolled back");

    Ok(())
}

/// BuildKit ships a systemd-resolved stub symlink for /etc/resolv.conf;
/// replace it with real nameservers because there is no systemd under proot.
fn prepare_dns(root: &Path) {
    let resolv = root.join("etc/resolv.conf");
    if let Ok(m) = fs::symlink_metadata(&resolv) {
        if m.file_type().is_symlink() {
            let _ = fs::remove_file(&resolv);
        }
    }
    if resolv.is_file() {
        return;
    }
    let content = fs::read_to_string("/etc/resolv.conf")
        .ok()
        .or_else(|| {
            std::env::var("PREFIX").ok().and_then(|p| {
                fs::read_to_string(Path::new(&p).join("etc/resolv.conf")).ok()
            })
        })
        .unwrap_or_else(|| "nameserver 8.8.8.8\nnameserver 1.1.1.1\n".to_string());
    if let Some(parent) = resolv.parent() {
        let _ = fs::create_dir_all(parent);
        relax_permissions(parent, 0o700);
    }
    if let Err(e) = fs::write(&resolv, content) {
        warn!("Could not write {}: {}", resolv.display(), e);
    }
}

/// Ensure the standard `/dev` fd symlinks exist in the guest root.
///
/// The base layer payload only ships `dev/shm`; `fd`, `stdin`, `stdout`
/// and `stderr` are normally created at runtime by the OS and are absent
/// from our unprivileged seed.  Anything using bash process substitution
/// (`<(...)`, e.g. autobuild4 `proc/53-spiral.sh` for Python packages)
/// opens `/dev/fd/N`, so create the symlinks on the rootfs: they resolve
/// through the bound `/proc` to the opening process's own descriptors.
fn prepare_dev(root: &Path) {
    let dev = root.join("dev");
    if !dev.is_dir() {
        return;
    }
    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        let link = dev.join(name);
        match fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => {}
            Ok(_) => continue, // something real occupies the name: leave it
            Err(_) => {
                if let Err(e) = std::os::unix::fs::symlink(target, &link) {
                    warn!("Could not create dev symlink {}: {}", link.display(), e);
                }
            }
        }
    }
}

impl DirCopyManager {
    /// A root is only usable once seeding ran to completion.
    fn is_seeded(&self) -> bool {
        self.inst.join(SEED_MARKER).is_file() && self.root.join("usr").is_dir()
    }

    fn ensure_mount_link(&self, to: &Path) -> Result<()> {
        let target = self.root.canonicalize()?;
        match fs::symlink_metadata(to) {
            Ok(m) if m.file_type().is_symlink() => {
                let resolved = fs::canonicalize(to).unwrap_or_default();
                if resolved != target {
                    fs::remove_file(to)?;
                    std::os::unix::fs::symlink(&target, to)?;
                }
            }
            Ok(_) => bail!(
                "`{}` exists but is not a symlink; please remove it before mounting",
                to.display()
            ),
            Err(_) => {
                std::os::unix::fs::symlink(&target, to)?;
            }
        }
        Ok(())
    }
}

impl LayerManager for DirCopyManager {
    fn from_inst_dir<P: AsRef<Path>>(
        dist_path: P,
        inst_path: P,
        inst_name: P,
    ) -> Result<Box<dyn LayerManager>>
    where
        Self: Sized,
    {
        let inst = inst_path.as_ref().join(inst_name.as_ref());
        Ok(Box::new(DirCopyManager {
            name: inst_name.as_ref().to_string_lossy().to_string(),
            dist: dist_path.as_ref().to_path_buf(),
            root: inst.join("root"),
            inst,
        }))
    }

    fn mount(&mut self, to: &Path) -> Result<()> {
        if !self.is_seeded() {
            info!(
                "{}: instance root is missing or incomplete, seeding from the \
                 base layer ...",
                self.name
            );
            seed_instance(&self.name)?;
        }
        self.ensure_mount_link(to)?;
        prepare_dns(&self.root);
        prepare_dev(&self.root);
        Ok(())
    }

    fn is_mounted(&self, target: &Path) -> Result<bool> {
        let is_link = fs::symlink_metadata(target)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        Ok(is_link && self.is_seeded())
    }

    fn rollback(&mut self) -> Result<()> {
        info!(
            "{}: rolling back to the base layer (selective sync) ...",
            self.name
        );
        rollback_from_dist(&self.dist, &self.root)?;
        prepare_dns(&self.root);
        prepare_dev(&self.root);
        // Guest-side patches (proot link2symlink workaround for Python's
        // shutil.copytree + oma no_check_dbus for unattended dependency
        // installation; see src/guestpatch.rs and scripts/guest-patches/).
        // Best-effort: failure degrades to pre-fix behaviour, never aborts.
        if let Err(e) = crate::guestpatch::ensure_installed(&self.dist, &self.root) {
            crate::warn!("{}: failed to install guest patches: {e}", self.name);
        }
        // Replay the workspace configuration (the proot equivalent of the
        // overlay backend's surviving `local` config layer)
        if let Ok(config) = crate::config::read_config() {
            crate::config::apply_config(&self.root, &config)?;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        // A half-written or never-seeded root must never become the base
        // layer: every future `ciel add` would inherit the damage silently.
        if !self.root.join("usr").is_dir() {
            bail!(
                "Instance root `{}` is not a usable rootfs, refusing to promote \
                 it to the base layer.",
                self.root.display()
            );
        }
        if !self.dist.join("usr").is_dir() {
            bail!(
                "Base layer `{}` is missing or unusable. Run `ciel load-os` first.",
                self.dist.display()
            );
        }

        // dpkg backs every overwritten file up with link(file, file.dpkg-tmp)
        // and package payloads ship hard links, all emulated through `.l2s.*`
        // bookkeeping holding absolute host paths. Promoting the root as-is
        // would freeze THIS instance's fake links into the base layer, where
        // they derail the next instance's dpkg (aosc 14.0.0 update failures,
        // 2026-10-14/15). Materialize what resolves, drop what doesn't.
        let (materialized, dropped) = sanitize_l2s_tree(&self.root);
        if materialized > 0 || dropped > 0 {
            info!(
                "{}: link2symlink repair: {} fake link(s) materialized, {} dropped",
                self.name, materialized, dropped
            );
        }

        // The dircopy model makes commit trivial: the instance root IS a full
        // physical copy of dist plus every delta, so "commit" is an O(1)
        // rename hand-over — no rsync-style diff merge, and crucially the
        // deletions an upgrade performed (uninstalled packages) are carried
        // over, which a copy-based merge would resurrect at the next seed.
        let old = pristine_backup_path(&self.dist)?;
        info!(
            "{}: promoting the instance root to the base layer (the old base \
             layer is kept at {})",
            self.name,
            old.display()
        );
        fs::rename(&self.dist, &old)
            .with_context(|| format!("moving {} -> {}", self.dist.display(), old.display()))?;
        if let Err(e) = fs::rename(&self.root, &self.dist) {
            // Never leave the workspace without a base layer: put the old one
            // back before surfacing the error. If even the rollback fails, the
            // old base layer is still intact under `old` — say so, exactly,
            // because every future `ciel add` depends on it.
            match fs::rename(&old, &self.dist) {
                Ok(()) => {
                    return Err(anyhow!(
                        "Commit failed ({e}); the hand-over was rolled back and \
                         the base layer stays pristine at {}.",
                        self.dist.display()
                    ));
                }
                Err(rb) => {
                    return Err(anyhow!(
                        "Commit failed ({e}) AND the rollback failed ({rb}). The \
                         old base layer is still intact at {} — move it back to \
                         {} by hand before doing anything else.",
                        old.display(),
                        self.dist.display()
                    ));
                }
            }
        }

        // Rebuild the instance skeleton: the root moved away, but the
        // instance lifecycle (manifest markers) is expected to keep working —
        // `is_seeded()` now fails on the missing root/usr, so a later
        // `mount()` re-seeds this instance fresh from the new base layer.
        // Mirrors scripts/ciel-proot.sh `promote`.
        fs::create_dir_all(self.inst.join("manifest"))?;
        fs::write(self.inst.join("manifest/backend"), "dircopy")?;
        fs::write(self.inst.join(SEED_MARKER), b"1")?;
        info!(
            "{}: instance promoted to the base layer. Delete {} once the new \
             base layer is confirmed good.",
            self.name,
            old.display()
        );
        Ok(())
    }

    fn unmount(&mut self, target: &Path) -> Result<()> {
        if fs::symlink_metadata(target)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            fs::remove_file(target)?;
        }
        Ok(())
    }

    fn get_config_layer(&mut self) -> Result<PathBuf> {
        Ok(self.root.clone())
    }

    fn set_volatile(&mut self, volatile: bool) -> Result<()> {
        if volatile {
            warn!("volatile-mount is not supported by the proot backend, ignoring");
        }
        Ok(())
    }

    fn destroy(&mut self) -> Result<()> {
        // Drop the workspace mountpoint link if it is still around
        if let Ok(ws) = std::env::current_dir() {
            let link = ws.join(&self.name);
            if fs::symlink_metadata(&link)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                let _ = fs::remove_file(&link);
            }
        }
        if self.inst.exists() {
            fs::remove_dir_all(&self.inst)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression that motivated all of the above: a `0400` source — the
    /// mode of `/etc/gshadow` in the AOSC base layer — must seed, must keep its
    /// timestamp, and must stay re-runnable instead of poisoning itself.
    #[test]
    fn copy_file_handles_read_only_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("gshadow");
        let dst = dir.path().join("root/etc/gshadow");
        fs::write(&src, b"root:*::\n").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o400)).unwrap();
        let stamp = File::open(&src)
            .unwrap()
            .metadata()
            .unwrap()
            .modified()
            .unwrap();

        copy_file(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"root:*::\n");
        assert_eq!(
            fs::symlink_metadata(&dst).unwrap().modified().unwrap(),
            stamp,
            "mtime must be preserved for the rollback fast path"
        );

        // A second pass must be able to overwrite it, and a third must be able
        // to rewrite it in place. The old code failed here forever.
        copy_file(&src, &dst).unwrap();
        fs::write(&dst, b"changed\n").unwrap();
        copy_file(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"root:*::\n");
    }

    /// An unreadable source is repairable because the unprivileged extractor
    /// handed it to us.
    #[test]
    fn copy_file_unlocks_unreadable_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("secret");
        let dst = dir.path().join("root/secret");
        fs::write(&src, b"x").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();

        copy_file(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"x");
    }

    /// The same trap reached through rollback: a `0500` directory must not make
    /// its children undeletable.
    #[test]
    fn make_removable_unlocks_directories() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("usr/lib/private");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("leftover"), b"x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

        make_removable(dir.path());
        fs::remove_file(locked.join("leftover")).unwrap();
        fs::remove_dir(locked).unwrap();
    }

    /// Executable bits from the base layer must survive seeding.
    #[test]
    fn copy_file_preserves_executable_bit() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bash");
        let dst = dir.path().join("root/bin/bash");
        fs::write(&src, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();

        copy_file(&src, &dst).unwrap();
        assert_eq!(fs::symlink_metadata(&dst).unwrap().mode() & 0o7777, 0o755);
    }

    /// M4: commit must be a rename hand-over, not a copy. The new base layer
    /// must carry the update's *deletions* (uninstalled packages) as well as
    /// its additions, the instance root must have moved (O(1)), the old base
    /// layer must be kept as `dist.pristine-<stamp>`, and the instance
    /// skeleton (manifest markers) must be rebuilt.
    #[test]
    fn commit_promotes_instance_root_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("container");
        let inst = container.join("instances/t0");
        let dist = container.join("dist");
        let root = inst.join("root");

        // Base layer: two packages installed.
        fs::create_dir_all(dist.join("usr/bin")).unwrap();
        fs::write(dist.join("usr/bin/old"), b"o").unwrap();
        fs::write(dist.join("usr/bin/kept"), b"k").unwrap();

        // Instance: seeded from dist, then "updated" — `old` uninstalled,
        // `new` installed.
        fs::create_dir_all(&root.join("usr/bin")).unwrap();
        fs::copy(dist.join("usr/bin/old"), root.join("usr/bin/old")).unwrap();
        fs::copy(dist.join("usr/bin/kept"), root.join("usr/bin/kept")).unwrap();
        fs::remove_file(root.join("usr/bin/old")).unwrap();
        fs::write(root.join("usr/bin/new"), b"n").unwrap();

        // proot link2symlink state as dpkg's status backup leaves it behind:
        // a fake symlink pointing at an (instance-specific) holder path plus
        // the holder itself. Must not reach the base layer as-is.
        fs::write(root.join(".l2s.status0001.0001"), b"junk").unwrap();
        std::os::unix::fs::symlink(
            "/somewhere/else/.l2s.status0001.0001",
            root.join(".l2s.status0001"),
        )
        .unwrap();
        // A fake link with a broken chain (holder already gone): must be
        // dropped outright.
        std::os::unix::fs::symlink(".l2s.gone.0001", root.join("usr/bin/ld.bfd"))
            .unwrap();
        // An ordinary symlink must survive untouched.
        std::os::unix::fs::symlink("new", root.join("usr/bin/alias")).unwrap();

        let mut man = DirCopyManager {
            name: "t0".into(),
            inst: inst.clone(),
            dist: dist.clone(),
            root: root.clone(),
        };
        man.commit().unwrap();

        // Deletions and additions both reached the base layer.
        assert!(
            !dist.join("usr/bin/old").exists(),
            "a deletion must survive the promote (copy-based merges resurrect it)"
        );
        assert_eq!(fs::read(dist.join("usr/bin/new")).unwrap(), b"n");
        // The root moved: nothing left behind, nothing duplicated.
        assert!(!root.exists());
        // link2symlink repair: the resolvable fake became a real file with
        // the holder's content, the holder itself is gone, the dead fake is
        // dropped, and ordinary symlinks pass through.
        let m = fs::symlink_metadata(dist.join(".l2s.status0001")).unwrap();
        assert!(
            m.is_file(),
            "the fake link must be materialized into a real file"
        );
        assert_eq!(fs::read(dist.join(".l2s.status0001")).unwrap(), b"junk");
        assert!(!dist.join(".l2s.status0001.0001").exists());
        assert!(!dist.join("usr/bin/ld.bfd").exists());
        let m = fs::symlink_metadata(dist.join("usr/bin/alias")).unwrap();
        assert!(m.file_type().is_symlink());
        assert_eq!(
            fs::read_link(dist.join("usr/bin/alias")).unwrap(),
            Path::new("new")
        );
        // Exactly one pristine backup, and it still holds the old content.
        let backups: Vec<PathBuf> = fs::read_dir(&container)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().contains(".pristine-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1, "exactly one pristine backup expected");
        assert_eq!(fs::read(backups[0].join("usr/bin/old")).unwrap(), b"o");
        // Instance skeleton rebuilt so the lifecycle keeps working.
        assert_eq!(fs::read(inst.join("manifest/backend")).unwrap(), b"dircopy");
        assert!(inst.join("manifest/seeded").is_file());
    }

    /// M4: a failed hand-over must never leave the workspace without a base
    /// layer. The second rename (root -> dist) is made to fail by taking the
    /// write bit off the root's parent directory; the rollback (old -> dist)
    /// targets a different parent and must succeed.
    #[test]
    fn commit_failure_restores_the_base_layer() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("container");
        let inst = container.join("instances/t1");
        let dist = container.join("dist");
        let root = inst.join("root");

        fs::create_dir_all(dist.join("usr")).unwrap();
        fs::write(dist.join("usr/base"), b"b").unwrap();
        fs::create_dir_all(&root.join("usr")).unwrap();
        fs::write(root.join("usr/base"), b"b").unwrap();
        fs::write(root.join("usr/extra"), b"e").unwrap();

        let mut man = DirCopyManager {
            name: "t1".into(),
            inst: inst.clone(),
            dist: dist.clone(),
            root: root.clone(),
        };

        fs::set_permissions(&inst, fs::Permissions::from_mode(0o500)).unwrap();
        let err = man.commit().unwrap_err();
        fs::set_permissions(&inst, fs::Permissions::from_mode(0o700)).unwrap();

        // The error must say the hand-over was rolled back, and the base
        // layer must be back in place, pristine.
        assert!(
            err.to_string().contains("rolled back"),
            "unexpected error: {err:#}"
        );
        assert!(
            dist.join("usr/base").is_file(),
            "the base layer must be restored after a failed commit"
        );
        assert!(!dist.join("usr/extra").exists());
        // The backup slot is free again: the old layer was moved back, and
        // no `dist.pristine-*` directory lingers.
        let leftovers = fs::read_dir(&container)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".pristine-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }
}