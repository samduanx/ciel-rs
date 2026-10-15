//! SquashFS extraction with pluggable external tools.
//!
//! # Why this is not just `unsquashfs`
//!
//! Upstream ciel shells out to `unsquashfs` from squashfs-tools. That binary is
//! not available on every platform this fork targets. Termux, for example,
//! only packages **squashfs-tools-ng**, whose tools are deliberately named
//! differently (`rdsquashfs`, `gensquashfs`, `sqfs2tar`, `sqfsdiff`) so that
//! both packages can be installed side by side. There is no `unsquashfs` and
//! no `-static` build in the Termux repository.
//!
//! So instead of hardcoding one binary we resolve an [`Extractor`] at runtime
//! from [`Extractor::ALL`], in preference order.
//!
//! # Flag semantics
//!
//! Both backends are asked to preserve timestamps and permission bits, and —
//! when unprivileged — to skip the node types that cannot be created without
//! real root:
//!
//! | | `unsquashfs` | `rdsquashfs` |
//! |---|---|---|
//! | destination | `-d <dir>` | `-p <dir>` (with `-u /`) |
//! | timestamps | always | `-T` |
//! | permissions | always | `-C` |
//! | skip dev/sock/fifo | n/a (tolerated) | `-D` `-S` `-F` |
//! | xattrs | `-no-xattrs` when unprivileged | never requested |
//!
//! Deliberately **not** requested from either tool:
//!
//! * **chown** (`-O`) — an unprivileged process cannot change file ownership.
//!   Extracted files simply belong to the invoking user. proot fakes root
//!   inside the container, so ownership inside the container is unaffected.
//! * **xattrs** — Android rejects the `security.*` and `trusted.*` namespaces
//!   outright, and a failure there aborts the whole unpack. Losing
//!   `security.selinux` costs us nothing: the proot backend never runs SELinux.
//!
//! # Error classification
//!
//! [`extract`] treats *any* non-zero exit as fatal, including the per-file
//! errors that an unprivileged run produces for device nodes. That is
//! deliberate: `unsquashfs` reports mknod failures with a non-zero status, so
//! tolerating non-zero exits would reintroduce exactly the silent-empty-`dist/`
//! bug this module exists to fix. The device nodes are instead skipped
//! up-front where the tool supports it, and the caller validates the result
//! with [`verify_extracted_rootfs`].

use anyhow::Result;

use console::style;

use crate::info;
use indicatif::ProgressBar;
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;
use which::which;

/// Environment variable to force a specific backend, bypassing auto-detection.
///
/// Set to `unsquashfs` or `rdsquashfs`. Intended as an escape hatch when a
/// particular image trips up one of the tools.
pub const EXTRACTOR_ENV: &str = "CIEL_SQUASHFS_EXTRACTOR";

/// An external tool able to unpack a `.squashfs` image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extractor {
    /// `unsquashfs` from squashfs-tools (plougher) — upstream ciel's choice.
    Unsquashfs,
    /// `rdsquashfs` from squashfs-tools-ng (AgentD) — the Termux/Android choice.
    RdSquashfs,
}

impl Extractor {
    /// Every backend, in preference order.
    pub const ALL: &'static [Self] = &[Self::Unsquashfs, Self::RdSquashfs];

    /// The binary name looked up in `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            Self::Unsquashfs => "unsquashfs",
            Self::RdSquashfs => "rdsquashfs",
        }
    }

    /// Parse a backend name as accepted by [`EXTRACTOR_ENV`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.binary() == name)
    }

    /// Human-readable name, for `ciel doctor` output.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unsquashfs => "unsquashfs (squashfs-tools)",
            Self::RdSquashfs => "rdsquashfs (squashfs-tools-ng)",
        }
    }

    /// Whether this binary is present in `PATH`.
    pub fn is_installed(self) -> bool {
        which(self.binary()).is_ok()
    }

    /// Resolve the backend to use.
    ///
    /// [`EXTRACTOR_ENV`] wins if it names a backend. An unknown value is an
    /// error rather than a silent fallback: quietly ignoring an explicit
    /// override is how you end up debugging the wrong binary.
    pub fn resolve() -> Result<Self> {
        if let Some(requested) = std::env::var_os(EXTRACTOR_ENV) {
            let requested = requested.to_string_lossy().to_string();
            return match Self::from_name(&requested) {
                Some(e) => Ok(e),
                None => {
                    let known = Self::ALL
                        .iter()
                        .map(|e| e.binary())
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow::bail!(
                        "{EXTRACTOR_ENV}={requested:?} is not a known extractor. \
                         Expected one of: {known}"
                    )
                }
            };
        }

        Self::detect().ok_or_else(no_extractor_found)
    }

    /// Pick the first installed backend.
    pub fn detect() -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.is_installed())
    }
}

/// Report which backends are installed (used by `ciel doctor`).
pub fn available_extractors() -> Vec<&'static str> {
    Extractor::ALL
        .iter()
        .filter(|e| e.is_installed())
        .map(|e| e.binary())
        .collect()
}

/// Error message listing every backend we looked for.
fn no_extractor_found() -> anyhow::Error {
    let wanted = Extractor::ALL
        .iter()
        .map(|e| format!("  - {}", e.binary()))
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::anyhow!(
        "No SquashFS extractor found in PATH, so the rootfs image cannot be \
         unpacked. Install one of:\n{wanted}\n\
         \n\
         On Termux:pkg install squashfs-tools-ng   (rdsquashfs)\n\
         On Debian/Ubuntu/Arch:  apt install squashfs-tools      (unsquashfs)\n\
         \n\
         To pin a specific one, set {EXTRACTOR_ENV}=unsquashfs|rdsquashfs."
    )
}

/// Build the command for a backend.
///
/// `unprivileged` drops the metadata operations that cannot succeed without
/// real root (see the module docs).
fn build_command(extractor: Extractor, image: &Path, dest: &Path, unprivileged: bool) -> Command {
    let mut cmd = match extractor {
        Extractor::Unsquashfs => {
            let mut c = Command::new("unsquashfs");
            // -f: overwrite a pre-existing destination without prompting.
            // -d: destination directory (created by the tool if absent).
            c.arg("-f");
            c.arg("-d").arg(dest);
            if unprivileged {
                // Android rejects security.*/trusted.* xattrs, and unsquashfs
                // treats that failure as fatal for the whole image.
                c.arg("-no-xattrs");
            }
            c
        }
        Extractor::RdSquashfs => {
            let mut c = Command::new("rdsquashfs");
            // -u /: unpack the whole image ...
            c.arg("-u").arg("/");
            // ... into this directory.
            c.arg("-p").arg(dest);
            c.arg("-T"); // restore timestamps
            c.arg("-C"); // restore permission bits
            if unprivileged {
                // These node types cannot be created without CAP_MKNOD.
                c.arg("-D"); // skip device special files
                c.arg("-S"); // skip sockets
                c.arg("-F"); // skip fifos
            }
            c
        }
    };

    // The image path is positional and always comes last.
    cmd.arg(image);
    cmd
}

/// Drain a child pipe, feeding any progress percentage to `on_progress`.
///
/// Both tools redraw their progress bar in place using carriage returns, so
/// `\r` is treated as a line terminator alongside `\n`. Returns everything read
/// so the caller can include it in a diagnostic when the tool fails.
fn pump<R: Read + Send + 'static>(
    stream: R,
    on_progress: impl Fn(u8) + Send + 'static,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut collected = String::new();
        let mut pending: Vec<u8> = Vec::with_capacity(256);

        loop {
            let available = match reader.fill_buf() {
                Ok(buf) => buf,
                Err(_) => break,
            };
            if available.is_empty() {
                break; // EOF
            }

            for &byte in available {
                if byte == b'\r' || byte == b'\n' {
                    flush_chunk(&mut pending, &mut collected, &on_progress);
                } else {
                    pending.push(byte);
                }
            }
            let read = available.len();
            reader.consume(read);

            // Guard against a tool that emits megabytes without a single line
            // break; we only care about progress and the failure tail.
            if pending.len() > 64 * 1024 {
                pending.clear();
            }
        }
        flush_chunk(&mut pending, &mut collected, &on_progress);

        collected
    })
}

/// Emit one completed chunk of tool output.
fn flush_chunk(pending: &mut Vec<u8>, collected: &mut String, on_progress: &impl Fn(u8)) {
    if pending.is_empty() {
        return;
    }
    if let Ok(text) = std::str::from_utf8(pending) {
        if !text.is_empty() {
            collected.push_str(text);
            collected.push('\n');
            if let Some(pct) = parse_percentage(text) {
                on_progress(pct);
            }
        }
    }
    pending.clear();
}

/// Extract a percentage from tool output.
///
/// `unsquashfs` prints `[ 42%]`; `rdsquashfs` prints a similar bar. We look at
/// the last `<digits>%` in the chunk. Anything unparseable is ignored, so a
/// cosmetic format change cannot break extraction — at worst the bar stops
/// moving and the time-based fallback in [`Progress`] takes over.
fn parse_percentage(text: &str) -> Option<u8> {
    let idx = text.rfind('%')?;
    let digits: String = text[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok().map(|v| v.min(100) as u8)
}

/// Progress driver for the extraction.
///
/// Prefers the tool's own percentage output when it prints any.
///
/// When it prints none — the normal case, since neither tool draws a progress
/// bar when stdout is not a TTY — we must **not** invent one. The previous
/// version extrapolated from an assumed 8 MB/s, which on a 1 GB image pinned the
/// bar at 95% after two minutes and then sat frozen for the remaining ten
/// minutes of real work. That reads as a hang, and it is indistinguishable
/// from one: the user has no way to tell a stalled extractor from a bar that
/// stopped lying.
///
/// So the fallback is an indeterminate spinner showing elapsed time, backed by
/// a periodically sampled count of what is actually on disk.
struct Progress {
    pb: ProgressBar,
    total: u64,
    started: Instant,
    saw_percentage: bool,
    /// Bytes found in `dest` at the last sample, and when it was taken.
    sampled: Mutex<(u64, Instant)>,
    dest: PathBuf,
}

impl Progress {
    fn new(pb: &ProgressBar, total: u64, dest: &Path) -> Self {
        Self {
            pb: pb.clone(),
            total,
            started: Instant::now(),
            saw_percentage: false,
            sampled: Mutex::new((0, Instant::now())),
            dest: dest.to_path_buf(),
        }
    }

    /// Record a percentage reported by the tool.
    fn report(&mut self, pct: u8) {
        self.saw_percentage = true;
        self.pb.set_position(self.total * pct as u64 / 100);
    }

    /// Summarise `dir`, returning the total size of the regular files in it.
    ///
    /// Errors are swallowed: an extraction in flight routinely races us (a file
    /// disappears between `read_dir` and `metadata`), and a slightly stale
    /// count is still far more useful than none.
    fn dir_bytes(dir: &Path) -> u64 {
        let mut total = 0u64;
        for entry in WalkDir::new(dir).min_depth(1).into_iter().flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
        total
    }

    /// Called on a timer; reports real on-disk progress when we have none.
    ///
    /// Sampling walks the destination, so it is deliberately coarse: on a
    /// phone a 100k-entry walk is not free, and competing with the extractor
    /// for flash would make both slower.
    fn poll(&self) {
        if self.saw_percentage || self.total == 0 {
            return;
        }
        let mut sampled = self.sampled.lock().unwrap_or_else(|e| e.into_inner());
        if sampled.1.elapsed() < Duration::from_secs(20) {
            return;
        }
        let bytes = Self::dir_bytes(&self.dest);
        *sampled = (bytes, Instant::now());
        drop(sampled);

        let elapsed = self.started.elapsed().as_secs();
        let mib = bytes / (1024 * 1024);
        self.pb.set_message(format!(
            "Unpacking rootfs image: {mib} MiB unpacked, {elapsed}s elapsed \
             (the extractor reports no progress; this is normal)"
        ));
    }

    /// Final state once the child has exited.
    fn finish(&self) {
        let bytes = Self::dir_bytes(&self.dest);
        self.pb.set_position(self.total);
        let _ = bytes;
    }
}

/// Unpack `image` into `dest`.
///
/// `pb`/`total` drive the progress bar.
///
/// Returns `Ok(())` only when the tool exited successfully. Individual per-file
/// failures (device nodes under Android, say) may still cause a non-zero exit
/// on `unsquashfs`; the caller is expected to run
/// [`verify_extracted_rootfs`] rather than blanket-tolerating errors, so that a
/// genuinely aborted extraction cannot masquerade as success.
pub fn extract(
    extractor: Extractor,
    image: &Path,
    dest: &Path,
    pb: &ProgressBar,
    total: u64,
) -> Result<()> {
    let unprivileged = crate::common::allow_unprivileged();

    // Both tools want a destination directory that either does not exist or
    // is an empty directory. A plain file in the way makes them behave
    // differently, so normalise it away here.
    if dest.exists() && !dest.is_dir() {
        std::fs::remove_file(dest).ok();
    }
    std::fs::create_dir_all(dest)
        .map_err(|e| anyhow::anyhow!("Cannot create {}: {e}", dest.display()))?;

    // rdsquashfs resolves `-p` against its own cwd, which is not guaranteed to
    // be ours. Hand both tools absolute paths so the destination is never
    // silently created somewhere else.
    let abs_dest = std::fs::canonicalize(dest)
        .map_err(|e| anyhow::anyhow!("Cannot resolve {}: {e}", dest.display()))?;
    let abs_image = image
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot resolve rootfs image {}: {e}", image.display()))?;

    let mut cmd = build_command(extractor, &abs_image, &abs_dest, unprivileged);

    cmd.env("COLUMNS", "0")
        .env("LINES", "0")
        .env("TERM", "dumb")
        // Never let a tool read from the user: there may be no tty, and a
        // prompt would hang forever.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if unprivileged {
        info!(
            "Unpacking the rootfs image with `{}`. This writes tens of \
             thousands of small files, so it can take a long while on phone \
             storage; the extractor reports no progress while it runs.",
            extractor.binary()
        );
    }

    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "Failed to execute `{}`: {e}. Is it installed and in PATH?",
            extractor.binary()
        )
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let progress = Arc::new(Mutex::new(Progress::new(pb, total, &abs_dest)));

    // unsquashfs reports progress on stdout, rdsquashfs on stderr. Drain both
    // concurrently: a reader that handles only one pipe can fill the other
    // pipe's buffer and deadlock the child. On a 1 GB image that is a real
    // hang, not a theoretical one.
    //
    // The two pipes have distinct types, so they are pumped separately instead
    // of being collected into one array.
    let mut handles: Vec<thread::JoinHandle<String>> = Vec::new();
    if let Some(stdout) = stdout {
        let progress = Arc::clone(&progress);
        handles.push(pump(stdout, move |pct| {
            if let Ok(mut p) = progress.lock() {
                p.report(pct);
            }
        }));
    }
    if let Some(stderr) = stderr {
        let progress = Arc::clone(&progress);
        handles.push(pump(stderr, move |pct| {
            if let Ok(mut p) = progress.lock() {
                p.report(pct);
            }
        }));
    }

    // Keep the user informed while we wait. This reports what is actually on
    // disk; it never extrapolates a rate it cannot know.
    let ticker = {
        let progress = Arc::clone(&progress);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            if let Ok(p) = progress.lock() {
                p.poll();
            }
        })
    };

    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("Waiting for `{}` failed: {e}", extractor.binary()))?;

    // The pipes close at child exit, so the readers finish shortly after. Drop
    // the ticker first so it cannot outlive the progress state.
    drop(ticker);

    let mut report = String::new();
    for handle in handles {
        if let Ok(output) = handle.join() {
            report.push_str(&output);
        }
    }

    if let Ok(p) = progress.lock() {
        p.finish();
    }

    if !status.success() {
        let detail = report.trim();
        return Err(anyhow::anyhow!(
            "`{}` failed with {}{}",
            extractor.binary(),
            status,
            if detail.is_empty() {
                format!(
                    "\nNo output captured. If this is the first run, try the \
                     other backend with {EXTRACTOR_ENV}=<unsquashfs|rdsquashfs>."
                )
            } else {
                format!(":\n{}", indent(detail, 4))
            }
        ));
    }

    Ok(())
}

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Sanity-check that `dist_dir` holds something that looks like a rootfs.
///
/// This is deliberately stricter than "the directory is non-empty". A tool that
/// dies part way through — exactly what an unprivileged run can do — leaves a
/// *partially populated* tree that passes a naive check and then fails much
/// later inside the container with a baffling error. Every real AOSC OS rootfs
/// has all of these entries, so requiring them turns silent corruption into an
/// immediate, localised failure.
pub fn verify_extracted_rootfs(dist_dir: &Path) -> Result<()> {
    if !dist_dir.is_dir() {
        anyhow::bail!(
            "Rootfs extraction produced no directory at {}",
            dist_dir.display()
        );
    }

    const REQUIRED: &[&str] = &["bin", "usr", "etc", "lib"];
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|entry| !dist_dir.join(entry).exists())
        .collect();

    if !missing.is_empty() {
        let found = std::fs::read_dir(dist_dir)
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        anyhow::bail!(
            "Rootfs extraction into {} looks incomplete: {} of {} required \
             top-level entries are missing ({missing:?}), and only {found} \
             entries were extracted.\nThe extractor most likely aborted part \
             way through. Try the other backend with {EXTRACTOR_ENV}=unsquashfs \
             or {EXTRACTOR_ENV}=rdsquashfs.",
            dist_dir.display(),
            missing.len(),
            REQUIRED.len(),
        );
    }

    Ok(())
}
