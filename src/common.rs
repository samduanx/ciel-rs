use anyhow::{anyhow, Context, Result};
use console::user_attended;
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use digest_io::IoWrapper;
use faster_hex::hex_string;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};
use std::env::consts::ARCH;
use std::fs::{self, File};
use std::os::unix::prelude::MetadataExt;
use std::sync::LazyLock;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use console::style;
use crate::info;
use crate::squashfs;
use crate::warn;

pub const CIEL_MAINLINE_ARCHS: &[&str] = &[
    "amd64",
    "arm64",
    "ppc64el",
    "riscv64",
    "loongarch64",
    "loongarch64_nosimd",
    "loongson3",
];
pub const CIEL_RETRO_ARCHS: &[&str] = &[
    "alpha",
    "armv4",
    "armv5te",
    "armv6hf",
    "armv7hf",
    "ia64",
    "i486",
    "loongson2f",
    "m68k",
    "powerpc",
    "ppc64",
    "sh3",
    "sparc64",
];
pub const CURRENT_CIEL_VERSION: usize = 3;
const CURRENT_CIEL_VERSION_STR: &str = "3";
pub const CIEL_DIST_DIR: &str = ".ciel/container/dist";
pub const CIEL_INST_DIR: &str = ".ciel/container/instances";
pub const CIEL_DATA_DIR: &str = ".ciel/data";
const SKELETON_DIRS: &[&str] = &[CIEL_DIST_DIR, CIEL_INST_DIR, CIEL_DATA_DIR];

static SPINNER_STYLE: LazyLock<indicatif::ProgressStyle> = LazyLock::new(|| {
    indicatif::ProgressStyle::default_spinner()
        .tick_chars("⠋⠙⠸⠴⠦⠇ ")
        .template("{spinner:.green} {wide_msg}")
        .unwrap()
});

#[macro_export]
macro_rules! make_progress_bar {
    ($msg:expr) => {
        concat!(
            "{spinner} [{bar:25.cyan/blue}] ",
            $msg,
            " ({bytes_per_sec}, eta {eta}){msg}"
        )
    };
}

#[inline]
pub fn create_spinner(msg: &'static str, tick_rate: u64) -> indicatif::ProgressBar {
    let spinner = indicatif::ProgressBar::new_spinner().with_style(SPINNER_STYLE.clone());
    spinner.set_message(msg);
    spinner.enable_steady_tick(Duration::from_millis(tick_rate));

    spinner
}

#[inline]
pub fn check_arch_name(arch: &str) -> bool {
    CIEL_MAINLINE_ARCHS.contains(&arch) || CIEL_RETRO_ARCHS.contains(&arch)
}

/// AOSC OS specific architecture mapping table
#[inline]
pub fn get_host_arch_name() -> Option<&'static str> {
    #[cfg(not(any(target_arch = "powerpc64", target_arch = "loongarch64")))]
    match ARCH {
        "x86_64" => Some("amd64"),
        "x86" => Some("i486"),
        "powerpc" => Some("powerpc"),
        "aarch64" => Some("arm64"),
        "mips64" => Some("loongson3"),
        "riscv64" => Some("riscv64"),
        _ => None,
    }

    #[cfg(target_arch = "powerpc64")]
    {
        let mut endian: libc::c_int = -1;
        let result = unsafe { libc::prctl(libc::PR_GET_ENDIAN, &mut endian as *mut libc::c_int) };
        if result < 0 {
            return None;
        }
        match endian {
            libc::PR_ENDIAN_LITTLE | libc::PR_ENDIAN_PPC_LITTLE => Some("ppc64el"),
            libc::PR_ENDIAN_BIG => Some("ppc64"),
            _ => None,
        }
    }

    #[cfg(target_arch = "loongarch64")]
    {
        use core::arch::asm;

        fn loongarch_has_simd() -> bool {
            let mask: u64;
            const CONFIG_PAGE: u64 = 2;
            unsafe {
                asm! {
                    "cpucfg {mask}, {page}",
                    options(pure, nomem, nostack, preserves_flags),
                    page = in(reg) CONFIG_PAGE,
                    mask = lateout(reg) mask,
                }
                (mask >> 6) & 0x1 > 0
            }
        }

        if loongarch_has_simd() {
            Some("loongarch64")
        } else {
            Some("loongarch64_nosimd")
        }
    }
}

/// Calculate the Sha256 checksum of the given stream
pub fn sha256sum<R: Read>(mut reader: R) -> Result<String> {
    let mut hasher = IoWrapper(Sha256::new());
    std::io::copy(&mut reader, &mut hasher)?;

    Ok(hex_string(&hasher.0.finalize()))
}

/// Check if the current process is running in a container
pub fn is_in_ci() -> bool {
    match std::env::var("CI") {
        Ok(v) => match v.as_str() {
            "y" | "yes" | "1" | "t" | "true" | "on" => true,
            _ => false,
        },
        Err(_) => false,
    }
}

/// Termux/Android (and similar unprivileged environments) can never have real
/// root; the proot backend fakes root inside the container instead.
pub fn allow_unprivileged() -> bool {
    std::env::var_os("CIEL_ALLOW_UNPRIVILEGED").is_some()
        || std::env::var_os("TERMUX_VERSION").is_some()
}

/// Whether we are running inside a systemd-nspawn container, where creating
/// `/dev/console` fails because `/dev` is not fully populated.
pub fn in_systemd_nspawn() -> bool {
    std::process::Command::new("systemd-detect-virt")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .is_some_and(|virt| virt.trim() == "systemd-nspawn")
}

/// Extract the given .tar.xz stream and preserve all the file attributes
pub fn extract_tar_xz<R: Read>(reader: R, path: &Path) -> Result<()> {
    let decompress = xz2::read::XzDecoder::new(reader);
    let mut tar_processor = tar::Archive::new(decompress);
    tar_processor.set_unpack_xattrs(true);
    tar_processor.set_preserve_permissions(true);
    tar_processor.unpack(path)?;

    Ok(())
}

/// Extract the given .squashfs
pub fn extract_squashfs(path: &Path, dist_dir: &Path, pb: &ProgressBar, total: u64) -> Result<()> {
    // Resolve the extractor up front. A missing binary is a hard, actionable
    // error — it must never be downgraded to a warning, otherwise `ciel new`
    // happily continues with an empty dist/ and only fails much later with a
    // confusing "no such file or directory".
    let extractor = squashfs::Extractor::resolve()?;
    info!("Using SquashFS extractor: {}", extractor.describe());

    squashfs::extract(extractor, path, dist_dir, pb, total)
}

pub fn extract_system_rootfs(path: &Path, total: u64, use_tarball: bool) -> Result<()> {
    let f = File::open(path)?;
    let progress_bar = indicatif::ProgressBar::new(total);

    progress_bar.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(make_progress_bar!("Extracting rootfs ..."))
            .unwrap(),
    );

    progress_bar.set_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(5));

    let dist_dir = PathBuf::from(CIEL_DIST_DIR);
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir).ok();
    }
    // Always (re)create the destination. Previously this only ran when the
    // directory already existed, so a first run without a pre-existing dist/
    // handed the tools a missing path and failed in a confusing way.
    fs::create_dir_all(&dist_dir)?;

    // Whether per-file extraction failures may be tolerated.
    //
    // Inside systemd-nspawn /dev/console cannot be created, and on Termux no
    // device node can be created at all, so `tar` will report errors for those
    // entries while still unpacking everything else. Those are expected and
    // only affect nodes that the container backend shadows at runtime.
    //
    // This flag must stay scoped to the tarball path. A squashfs failure means
    // the *extractor itself* failed (missing binary, unknown compressor,
    // aborted mid-image) — none of which is a per-node cosmetic issue, and all
    // of which used to be swallowed here, leaving an empty dist/ behind.
    let tolerate_node_errors = allow_unprivileged() || in_systemd_nspawn();

    if use_tarball {
        if let Err(e) = extract_tar_xz(progress_bar.wrap_read(f), &dist_dir) {
            if !tolerate_node_errors {
                return Err(e).context("Failed to extract rootfs tarball");
            }
            warn!(
                "Rootfs extraction reported errors, ignored on container/unprivileged \
                 environments: {e:#}"
            );
        }
    } else {
        // Any error here is fatal. See the comment above.
        extract_squashfs(path, &dist_dir, &progress_bar, total)?;
    }

    progress_bar.finish_and_clear();

    // The extractor may have reported success yet produced an unusable tree
    // (aborted mid-image, skipped everything). Verify before letting anything
    // downstream touch dist/, so the failure surfaces here rather than as a
    // baffling ENOENT from `ciel shell` much later.
    squashfs::verify_extracted_rootfs(&dist_dir)
        .with_context(|| format!("Rootfs extraction into {CIEL_DIST_DIR} did not complete"))
}

pub fn ciel_init() -> Result<()> {
    for dir in SKELETON_DIRS {
        fs::create_dir_all(dir)?;
    }
    let mut f = File::create(".ciel/version")?;
    f.write_all(CURRENT_CIEL_VERSION_STR.as_bytes())?;

    Ok(())
}

/// Find the ciel directory
pub fn find_ciel_dir<P: AsRef<Path>>(start: P) -> Result<PathBuf> {
    let start_dev = fs::metadata(start.as_ref()).ok().map(|m| m.dev());
    let mut current_dir = start.as_ref().to_path_buf();
    let mut warned = false;
    loop {
        if current_dir.join(".ciel").is_dir() {
            return Ok(current_dir);
        }
        let current_canon = current_dir.canonicalize()?;
        let next = current_dir.join("..");
        let next_canon = next.canonicalize()?;
        if next_canon == current_canon {
            return Err(anyhow!("Hit filesystem ceiling!"));
        }
        // Downgraded to a warning: on Android the search may legitimately
        // cross mountpoints (e.g. /data sub-mounts).
        if let (Some(start_dev), Ok(next_dev)) = (start_dev, next_canon.metadata().map(|m| m.dev())) {
            if start_dev != next_dev && !warned {
                warn!("Crossed a filesystem boundary while looking for the ciel workspace");
                warned = true;
            }
        }
        current_dir = next;
    }
}

pub fn is_instance_exists(instance: &str) -> bool {
    Path::new(CIEL_INST_DIR).join(instance).is_dir()
}

pub fn is_legacy_workspace() -> Result<bool> {
    let mut f = fs::File::open(".ciel/version")?;
    // TODO: use a more robust check
    let mut buf = [0u8; 1];
    f.read_exact(&mut buf)?;

    Ok(buf[0] < CURRENT_CIEL_VERSION_STR.as_bytes()[0])
}

pub fn ask_for_target_arch() -> Result<&'static str> {
    // Collect all supported architectures
    let host_arch = get_host_arch_name();
    if !user_attended() {
        return match host_arch {
            Some(v) => Ok(v),
            None => Err(anyhow!("Could not determine host architecture")),
        };
    }
    let mut all_archs: Vec<&'static str> = CIEL_MAINLINE_ARCHS.into();
    all_archs.append(&mut CIEL_RETRO_ARCHS.into());
    let default_arch_index = match host_arch {
        Some(host_arch) => all_archs.iter().position(|a| *a == host_arch).unwrap(),
        None => 0,
    };
    // Setup Dialoguer
    let theme = ColorfulTheme::default();
    let prefixed_archs = CIEL_MAINLINE_ARCHS
        .iter()
        .map(|x| format!("mainline: {x}"))
        .chain(CIEL_RETRO_ARCHS.iter().map(|x| format!("retro: {x}")))
        .collect::<Vec<_>>();
    let chosen_index = FuzzySelect::with_theme(&theme)
        .with_prompt("Target Architecture")
        .default(default_arch_index)
        .items(prefixed_archs.as_slice())
        .interact()?;

    Ok(all_archs[chosen_index])
}
