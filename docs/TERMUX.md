# ciel-rs proot backend (Termux / Android)

This fork adds a **portable `proot` container backend** so ciel can run
natively inside Termux on Android, where `systemd-nspawn`, D-Bus/machined,
kernel mounts (`mount(2)`), overlayfs and FUSE are all unavailable.

## Why this design

| ciel concept (nspawn) | proot backend |
|---|---|
| `systemd-nspawn -D <rootfs>` long-lived PID 1 | **nothing** — a "container" is a single `proot` invocation |
| `systemd-run -M <ns> -- <cmd>` | the same proot invocation runs the command directly |
| `sd_bus_open_system_machine()` readiness wait | **nothing** — no systemd to wait for |
| D-Bus `BindMountMachine` | `proot -b <host>:<guest>` path translation (per-invocation) |
| overlayfs upper/lower + `rollback` | **DirCopy layer manager** (see below) |
| `trusted.overlay.*` xattr diff + `commit` | not supported yet (`update-os`/`commit` bail on proot) |

Key platform facts that forced this design:

- **proot cannot nest** (one ptrace tracer per process), so ciel must run as a
  native Termux process — *not* inside proot-distro — and spawn `proot` itself.
- Android apps have no `CAP_SYS_ADMIN`, no unprivileged user namespaces, and
  `/dev/fuse` is blocked by AOSP sepolicy → no overlayfs, no fuse-overlayfs.
- Termux ships no `systemd`/`libsystemd`; the nspawn-only dependencies are
  gated behind a cargo feature.

## Rollback without overlayfs (DirCopy)

Disk budget: ~30 GB (two full rootfs trees + the buildkit image).

```
.ciel/container/dist/                  pristine BuildKit base (read-only by convention)
.ciel/container/instances/<n>/root/    real writable rootfs handed to proot
<workspace>/<instance>                 SYMLINK -> instances/<n>/root  (mount state)
```

`rollback` performs a selective rsync-like sync of `root` from `dist`
(delete entries missing in dist, copy entries whose size+mtime differ),
skipping only the proot bind targets (`/tree /debs /proc /sys /dev /run`),
then replays the workspace configuration (`ab4cfg.sh`, `sources.list`,
`forest.conf`) and rewrites a working `/etc/resolv.conf` (BuildKit ships a
systemd-resolved stub that cannot work without systemd). This also sweeps
non-package-manager residue: build dirs, `/tmp`, `ld.so.cache`, apt lists.

`commit` is intentionally unsupported on this backend, which means **`ciel
update-os` is unavailable** — use a recent BuildKit release, otherwise `oma
update` re-applies the whole update delta on every package build.

## The permission model (read this before touching `dircopy.rs`)

There is no root, no `CAP_CHOWN`, no `CAP_FOWNER`, no `CAP_MKNOD`, no overlayfs
and no second proot. Every fix in this backend has to work with `chmod` alone.

**The load-bearing fact: `dist/` is owned by the invoking user.** The image is
unpacked by an unprivileged process, so *nothing* in it can be `chown`ed, and
therefore **everything in it can be `chmod`ed**. That is what makes the whole
design workable, and it is the thing to check first if a permission change ever
seems to require privileges.

The permission bits baked into the image were authored for a real rootfs and are
hostile to a host-side copy loop:

| Trap | Symptom | Repair |
|---|---|---|
| `std::fs::copy` propagates the **source** mode to the destination | `copying .../etc/gshadow -> ...` `EACCES` | create the destination `0600`, then re-apply `src_mode \| 0600` after the copy |
| the destination was left read-only by an earlier run | the error is **permanent** and repeats on every retry | `relax_permissions(dst, 0o600)` before `O_TRUNC` |
| a source the host may not read (`0000`, `0200`) | cannot even be opened | `chmod` the source — we own it |
| directories in the image are `0500` | `rollback` cannot unlink their children | `make_removable()` top-down before the sweep |

### Why the gshadow failure looked unrecoverable

`/etc/gshadow` is `0400`. The old `copy_file` called `std::fs::copy`, which
made the destination `0400` too, then tried to `open(dst, O_WRONLY)` to restore
mtime → `EACCES`. The half-written `0400` file stayed on disk, and every rerun
then died on the *same* line, because `fs::copy` needs `O_TRUNC` on a `0400`
file. The bug manufactured its own sticky state, which is why "just try again"
never worked. `dircopy.rs` carries four regression tests for exactly this.

### Rules

* Never use `std::fs::copy` here. Use `copy_file`.
* Never let a source mode reach the destination *before* the copy finishes.
* Individual copy failures are **not** fatal — record and continue. This host
  cannot create device nodes, and the guest does not need it: proot fakes uid 0
  and bind-mounts `/dev`. Aborting a 100k-file seed over one awkward entry is
  what made the command unusable.
* Seeding completeness is tracked by the `manifest/seeded` marker, **not** by
  `root/usr` existing. A half-written tree passes `is_dir()` and then cannot
  boot a package manager.
* `is_skip` (`SKIP_PATHS`) must stay in sync between seeding and rollback.

## Building on Termux

```bash
pkg install rust proot squashfs-tools-ng xz git libgit2 pkg-config
cargo build --no-default-features --features proot --release
```

### Why `squashfs-tools-ng` and not `squashfs-tools`

The AOSC OS rootfs is a `.squashfs` image, so ciel needs an extractor. Termux's
official repo only ships **squashfs-tools-ng**, whose binaries are deliberately
named differently (`rdsquashfs`, `gensquashfs`, `sqfs2tar`, `sqfsdiff`) so that
it can coexist with the classic `squashfs-tools`. There is no `unsquashfs` and
no `-static` variant in the Termux repo.

`src/squashfs.rs` therefore resolves an extractor at runtime, in this order:

| Backend | Provided by | Flags used |
|---|---|---|
| `unsquashfs` | `squashfs-tools` (Debian/Arch/…) | `-f -d <dest>` (+ `-no-xattrs` unprivileged) |
| `rdsquashfs` | `squashfs-tools-ng` (Termux) | `-u / -p <dest> -T -C` (+ `-D -S -F` unprivileged) |

The buildkit images are **xz**-compressed (verified by reading the superblock:
compression id 4), which both backends support.

Both preserve timestamps and permission bits. Neither does `chown` or `setxattr`
under `CIEL_ALLOW_UNPRIVILEGED` — Android rejects the `security.*`/`trusted.*`
xattr namespaces outright (a failure there aborts the whole unpack) and
unprivileged processes cannot change ownership anyway. The resulting files
simply belong to the invoking user, which is what proot expects.

If neither binary is found, `ciel` fails **immediately** with an actionable
message instead of warning and continuing.

On a Linux host the default feature set (nspawn) builds unchanged:
`cargo build --release`.

### Error handling: fatal vs tolerable

This distinction is the whole point of the rewrite. Previously *every* extraction
error was downgraded to a warning whenever `CIEL_ALLOW_UNPRIVILEGED` was set, so
a missing extractor produced an empty `dist/` and the run only failed much later
with a baffling `No such file or directory` from `ciel shell`.

Now:

* **squashfs errors are always fatal.** A non-zero exit means the *extractor*
  failed — missing binary, unknown compressor, aborted mid-image. None of these
  is a per-node cosmetic issue. Device nodes are instead skipped up front
  (`-D -S -F`) where the tool supports it.
* **tarball errors are still tolerated** under nspawn / unprivileged, because
  `tar` reports device-node failures while unpacking everything else.
* **The result is verified** after extraction: `dist/` must contain `bin`,
  `usr`, `etc` and `lib`. This catches a half-unpacked tree, which a naive
  "directory is non-empty" check would happily accept.

### Verifying the extractor on-device

```bash
# 1. Is a binary present at all?
command -v rdsquashfs unsquashfs

# 2. Does it support the image's compressor? (expect xz listed)
rdsquashfs --help 2>&1 | head -20

# 3. What will ciel actually pick?
ciel doctor

# 4. Force one backend if the other misbehaves
CIEL_SQUASHFS_EXTRACTOR=rdsquashfs ciel new
CIEL_SQUASHFS_EXTRACTOR=unsquashfs ciel new
```

If `ciel` still reports `unsquashfs binary does not exist`, you are running an
**old binary** — that message no longer exists in the source. Rebuild and
reinstall, **with the proot feature set** (a bare `cargo build --release`
pulls in the default `nspawn` feature and fails on Termux for lack of
libsystemd):

```bash
cargo build --no-default-features --features proot --release
```

Then make sure your shell resolves the new one with `command -v ciel`.

Backend selection order: `CIEL_BACKEND` env → `backend` key in
`.ciel/data/config.toml` → auto-detect (`TERMUX_VERSION` present or
`systemd-nspawn` missing ⇒ proot).

## What `ciel doctor` actually checks

`ciel doctor` selects its check list from the active backend, so on Termux it
does **not** ask for `systemd-nspawn`, D-Bus/machined or overlayfs — those are
unreachable here and reporting them as missing would be noise. What it checks
instead maps 1:1 onto what this backend's code actually requires:

| Check | Why this backend needs it |
|---|---|
| active backend + the rule that chose it | an auto-detected backend is otherwise indistinguishable from a pinned one |
| **not running inside another proot** | `TracerPid != 0` in `/proc/self/status` (plus `PROOT_L2S_DIR`) — proot must *be* the tracer, so a nested proot silently never runs. This is the fork's hardest rule and it is invisible from the shell |
| `proot` present + version | the only binary `ProotBackend` spawns |
| `/dev`, `/proc`, `/sys` present | `build_argv` binds all three unconditionally; a missing one fails every guest command |
| host `resolv.conf` | nspawn copies it in automatically, proot must be handed one; on Android `/etc/resolv.conf` does not exist and Termux keeps it under `$PREFIX`. No DNS ⇒ no package installs |
| symlink support in the workspace | `DirCopyManager::mount` *is* a symlink (`<ws>/<instance>` → `instances/<n>/root`); `/sdcard` cannot do this at all |
| `git` present | `network.rs` runs `git rebase` to reset the tree |
| SquashFS extractor | resolved through `Extractor::resolve()` — the same call the extraction path makes, so a bad `CIEL_SQUASHFS_EXTRACTOR` is caught here and not halfway through a 1 GB unpack |
| workspace state | `.ciel/version` (and whether it is a legacy layout), base layer via the same `verify_extracted_rootfs` the extractor path uses, and `n/m instances seeded` from the `manifest/seeded` markers |
| disk I/O + space | thresholds are backend-aware: 10 GB floor, but the DirCopy budget is ~30 GB because it keeps a *second* full copy of the rootfs |
| editor | informational only, and it checks the *resolved* program from `config::get_default_editor()` (VISUAL → EDITOR → editor → nano) rather than upstream's EDITOR/editor-only pair, which failed on hosts that would have worked |

Two behavioural changes worth knowing about:

* **Every check runs.** Upstream stopped at the first failure, which on a phone
  means one problem per invocation and hides the second fault behind the first.
  The exit status still reflects whether anything failed.
* **Output is streamed**, not printed at the end, so a slow check cannot look
  like a hang.
* Warnings (yellow `!`) do not fail the run. The notable ones: legacy workspace
  layout, no instances yet, a `dist/` that fails verification, free space under
  the DirCopy budget, no guest DNS.

The manual harness keeps its own doctor for the argv-level view:
`./ciel-proot.sh doctor`.

## Environment variables

| Variable | Effect |
|---|---|
| `CIEL_BACKEND=proot\|nspawn` | force a backend |
| `CIEL_SQUASHFS_EXTRACTOR=unsquashfs\|rdsquashfs` | force a specific squashfs extractor instead of auto-detecting |
| `CIEL_ALLOW_UNPRIVILEGED=1` | allow running without real root (implied on Termux); also tolerates mknod/xattr errors when extracting a **tarball** |
| `CIEL_PROOT_EXTRA_OPTIONS="..."` | extra argv appended to the proot invocation (e.g. `--sysvipc`) |
| `CIEL_PROOT_NO_SECCOMP=1` | sets `PROOT_NO_SECCOMP=1` inside proot (SIGSYS escape hatch) |
| `CIEL_OFFLINE` | warn-only on proot (no network isolation available) |
| `nspawn-extra-options` (config.toml) | ignored by the proot backend (warns) |

## Milestone verification (single-package spike)

```bash
# M0 — build + doctor
ciel version && ciel doctor

# M1 — workspace + buildkit (arm64)
ciel new            # choose arm64, defaults are fine
du -sh .ciel/container/dist .ciel/container/instances/*/root   # ~14G each

# M2 — shell
ciel shell -i main -- 'cat /etc/os-release && id && ls /tree | head'

# M3 — package manager
ciel shell -i main -- 'oma refresh && oma update -y --no-check-dbus'
ciel rollback -i main

# M4 — build one package
ciel build -i main <category>/<package>
ls OUTPUT-*/debs/

# M5 — rollback integrity
ciel shell -i main -- 'dpkg -l | wc -l'   # compare with baseline
cd TREE && git status --porcelain         # /tree is RW-bound; verify no stray writes
```

## Distinguishing "empty dist/" from a shell quirk

`ciel` extracts the rootfs into `.ciel/container/dist` — note the
`container/` component. A probe like

```bash
ls -la .ciel/dist/ 2>/dev/null || ls -la */dist/ 2>/dev/null
```

can never succeed (wrong path, and the glob matches nothing), and under
ble.sh's `[ble: exit 2]` prompt indicator that looks like a ciel failure while
saying nothing about the actual state. Check the real path instead:

```bash
ls -la .ciel/container/dist | head
```

`extract_system_rootfs` now also verifies that `dist/` contains `bin`, `usr`,
`etc` and `lib` after extraction, so a failed or half-finished unpack aborts
with a clear, localised error rather than surfacing later as a confusing
"no such file or directory" from `ciel shell`.

## Known limitations / on-device checklist

- `/tree` is bind-mounted **read-write** (proot has no RO bind) — protected by
  convention only; verify with `git status` after a build.
- proot is ptrace-based: expect a 5–10x slowdown on compile-heavy packages.
- Device nodes cannot be created unprivileged; skipped during extraction and
  shadowed by the `/dev` bind at runtime.
- Cross-arch builds (qemu-user-static inside proot) are out of scope for now.
- `find_ciel_dir` no longer hard-fails on filesystem boundaries (Android
  mountpoints) — keep the workspace under `$HOME`, never on `/sdcard`.

## Keeping the build environment current (semi-rolling, no `commit`)

AOSC OS is a **semi-rolling** distribution: at packaging time the build
environment is supposed to be as close to "latest" as possible. The stock
nspawn backend satisfies this with `ciel update-os`, which upgrades a
throwaway instance and then **commits** the result into the base layer, so
every later instance starts already updated.

The proot backend has no overlayfs, hence no xattr diff and no `commit`, and
`update-os` currently bails out. The tempting shortcut — set
`local_repo = false` and skip the update — is **wrong**, because it pins the
build environment to whatever BuildKit snapshot was unpacked, which rots.
The philosophy has to be preserved, so the base layer itself has to become
mutable.

### The base layer is already a real directory

This is the fact the design was missing. Under DirCopy, `dist/` is *not* an
immutable lower layer — it is an ordinary directory tree that an unprivileged
process created by unpacking a squashfs, and therefore owns. The instance
`root` is likewise a complete, self-contained copy of it.

So once an instance has been upgraded, `instances/<n>/root` **is** "the base
layer plus the whole update delta", materialized. Promoting it is therefore
not a merge at all — it is a **rename**:

```
.ciel/container/dist.pristine-<ts>/      the untouched BuildKit tree
.ciel/container/dist/                    the current, upgraded base layer
```

`mv dist dist.pristine-<ts> && mv instances/<n>/root dist` is O(1), needs no
rsync pass, and — critically — it carries **deletions** automatically. That
last property is why a copy-based "sync the delta back" commit would be
wrong: packages removed by the upgrade would keep resurrecting themselves on
the next seed, because a naive copy only ever adds and overwrites.

This is the proot analogue of `commit`, and it makes `update-os` work.

### Consequences

- `ciel update-os` becomes available on proot: seed a throwaway instance,
  run the update, promote it, drop the instance. The semantics match nspawn
  exactly.
- `oma upgrade` inside `package_build_inner` (`packaging.rs`) degrades from
  "replay the entire delta for every single package" to a cheap no-op that
  only picks up whatever landed since the promotion. That is the
  semi-rolling requirement satisfied *without* skipping the update.
- The BuildKit snapshot is retained as `dist.pristine-<ts>/` so the
  promotion is reversible; it costs one extra rootfs of disk. Delete it once
  the updated base is known good.
- Promotion must be atomic-ish and never happen on a half-applied upgrade:
  check the exit status of `oma upgrade` first, and refuse to promote a
  failed update (a half-updated base is worse than a stale one).
- Because `dist/` is now mutable, `SKIP_PATHS` in `dircopy.rs:67` stops
  being merely an optimization: entries must be excluded from *promotion*
  too, or `/proc`, `/sys`, `/dev`, `/run` and the `/tree` `/debs` bind
  targets would be baked into the base layer where they do not belong.

### Do not confuse this with skipping the update

There is a second, unrelated reason a proot build may need help, and it is
worth stating separately because the symptom looks identical: the guest
dpkg failing with

```
Problem unlinking the file / - FileFd::Open (21: Is a directory)
Could not open file '/' - EDSP::Resolve (17: File exists)
```

is a **dpkg/proot syscall-semantics problem**, not a staleness problem.
Promoting the base layer does not fix it. `scripts/ciel-proot.sh` exists to
settle it experimentally (`syscall-probe`, `dpkg-probe`) without rebuilding
ciel.

## Manual proot harness

`scripts/ciel-proot.sh` mirrors the argv construction in
`src/backend/proot.rs` (including the `env_clear()` equivalent and the
absence of any `--` separator) so the container can be driven by hand:

```bash
./ciel-proot.sh doctor          # environment check + the exact argv
./ciel-proot.sh shell           # interactive guest shell
./ciel-proot.sh run /bin/bash -ec 'dpkg --audit'
./ciel-proot.sh syscall-probe   # errno semantics: host vs proot, then diff
./ciel-proot.sh dpkg-probe      # aosc-aaa unpack matrix across proot flags
./ciel-proot.sh update          # oma upgrade inside the instance only
./ciel-proot.sh update-base     # update, verify, then promote to base layer
./ciel-proot.sh promote <inst>  # promotion only
```

Keep it in sync with `proot.rs` when changing bind mounts.


### Usage

The script needs only two things — **`proot` in Termux and an already
initialised workspace**. It does *not* need the `ciel` binary; that is the
whole point, so that experiments do not require a rebuild.

```bash
pkg install proot            # once
git clone <this repo>        # or just copy scripts/ciel-proot.sh over
```

Then, with a workspace at `~/buildroots` and the binary at
`~/ciel-rs/target/release/ciel-rs`:

```bash
cd ~/buildroots
CIEL=~/ciel-rs/target/release/ciel-rs

# one-time setup, if you have not done it yet
$CIEL load-os
$CIEL add -i arm64

# then, at any point, drive the container by hand:
S=~/ciel-rs/scripts/ciel-proot.sh
$S -C ~/buildroots doctor              # environment + exact argv
$S -C ~/buildroots shell arm64         # interactive guest shell
$S -C ~/buildroots run arm64 /bin/bash -ec 'dpkg --audit'
```

`-C/--workspace` is the only option you really need; the script also finds
the workspace by walking upwards from `$PWD`, so `cd ~/buildroots` first and
`-C` becomes optional. Global options must precede the subcommand.

The instance name is a positional argument to the subcommand, not `-i`, so
that each probe can be pointed at a different instance without re-typing the
global flags.

Recommended order when reproducing the `aosc-aaa` failure:

```bash
$S -C ~/buildroots doctor# 1. is everything wired up
$S -C ~/buildroots update arm64        # 2. reproduce the failure
$S -C ~/buildroots dpkg-probe arm64    # 3. which flag/force changes it
$S -C ~/buildroots syscall-probe arm64 # 4. errno semantics, host vs proot
$S -C ~/buildroots update-base arm64   # 5. only after 2-4 succeed
```

Step 5 promotes the instance to the new base layer (see above). It refuses
to promote a failed upgrade — a half-updated base is worse than a stale one.

`dpkg-probe` really does run `dpkg --unpack` and really does modify the
instance. Rebuild it afterwards (`$CIEL del -i arm64 && $CIEL add -i arm64`).


## Root cause of the `oma upgrade` failure (confirmed on-device, 2026-10-06)

The `dpkg-probe` matrix settled it. The failing step is:

```
dpkg: error processing archive .../aosc-aaa_..._arm64.deb (--unpack):
 error creating directory './dev/shm': Permission denied
```

and the matrix reads:

| variant | result |
|---|---|
| baseline (`-b /dev -b /proc -b /sys`) | **FAIL** — `mkdir ./dev/shm` EACCES |
| no-pseudo (no `/dev` `/proc` `/sys` binds) | **SUCCESS** at every force level |
| no-link2symlink | FAIL — `status-old` backup link (link2symlink is required) |
| no-change-id | FAIL — "requires superuser privilege" (change-id is required) |

**Mechanism.** The wholesale `-b /dev` makes the guest `/dev` *be* Android's
real `/dev` — a tmpfs owned by `root:root` mode 0755. `aosc-aaa` ships
`./dev/shm` in its payload, Android's `/dev` has no `shm`, so dpkg's mkdir
lands on the real Android `/dev` and returns EACCES for the app uid. Debian
survives this in proot-distro only because `base-files` ships no `./dev/shm`
— this is an AOSC-specific trap.

**Fix (src/backend/proot.rs, applied).** `/proc` and `/sys` stay bound
wholesale (nothing ever creates entries under them — their payload dirs
exist in the host trees, so dpkg's mkdir is a no-op there). `/dev` is no
longer bound wholesale: the individual character devices (`null`, `zero`,
`full`, `random`, `urandom`, `tty`) are bound instead. The guest `/dev`
then remains the rootfs's own directory — seeded from `dist/`, owned by the
invoking user — which dpkg can freely mkdir/chmod, while the device nodes
still resolve through proot's path translation.

Supporting fact (misread earlier): `is_skip()` in `dircopy.rs` applies to
**rollback only**; `seed_instance` copies `dist/dev`, `dist/proc`,
`dist/sys`, `dist/run` (directories; device nodes were already skipped at
extraction). So fresh instances already have a real, writable `root/dev`.

**Residual risk to watch in the verification run:** dpkg's deferred
directory-metadata pass may chmod the bound `/proc` and `/sys` and get
EPERM. The proot-distro/Debian precedent says this is at most a warning;
if it turns fatal, the fallback is to also drop the `/proc`/`/sys`
wholesale binds during package operations.

**The `W: ... / - EDSP::Resolve (17: File exists)` warnings** are
libapt/rust-apt noise from oma's recovery path after dpkg failed, not the
cause (the fatal error is the mkdir above). Expect them to disappear once
the unpack succeeds; if they persist on a clean run, investigate oma's
temp-file handling separately.

Also confirmed on-device: `--link2symlink` and `--change-id=0:0` are both
required and correct; only the `/dev` bind was wrong.

## Verification steps after rebuilding ciel

```bash
cd ~/buildroots
CIEL=~/ciel-rs/target/release/ciel-rs
$CIEL del -i arm64 && $CIEL add -i arm64   # instance dpkg state is now
                                            # polluted by the probe runs
./ciel-proot.sh -C ~/buildroots update arm64   # must pass aosc-aaa now
./ciel-proot.sh -C ~/buildroots update-base arm64  # then promote
```
