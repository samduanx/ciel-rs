# 后续工作计划 — ciel-rs proot 后端

**当前状态（2026-10-06）**：`oma upgrade` 的根因已定位并修复，真机验证通过。
根因是 proot 整体 bind `/dev`，导致 guest 的 `/dev` 就是 Android 真实的
`root:root` tmpfs，dpkg 解 `aosc-aaa` 时 `mkdir ./dev/shm` 撞 EACCES。
修法见 `src/backend/proot.rs`（`/proc` `/sys` 保持整体 bind，`/dev` 改为逐个
bind 设备节点）。分析与实验数据见 `TERMUX.md` 末尾的 root-cause 章节。

里程碑现状：`ciel shell` 通 → `oma upgrade` 通 → **下一个是 `ciel build` 端到端**。
**M3 ✅（2026-10-06 深夜达成）**：`ciel build -g`（下载）与完整构建均真机通过，
pwgen 产出 .deb（arm64 @ 2.08-0）。过程中的三块拦路石及定案：
① PTY（acbs→pexpect）→ `/dev/ptmx`+`/dev/pts` 成对 bind 固化进 proot.rs；
② argv 换行分隔 bug（harness）→ NUL 分隔；
③ umask 0077 泄漏（harness）→ 头部 `umask 022`。
全程记录见 M3-TEST-PLAN.md 第 7-11 节。当前里程碑 = **M4（promote 落地 Rust）**。
**M3 进展更新（2026-10-06 晚）**：`ciel build` 的头号障碍（acbs-build 走
pexpect，需要 PTY）已实锤并解决——`/dev/ptmx` + `/dev/pts` 成对 bind 已固化进
`proot.rs` 与 `scripts/ciel-proot.sh`（门控：宿主两者都存在才 bind）。
完整分析、实验数据与阶段 C/D/E 操作步骤见 **`M3-TEST-PLAN.md`**（第 7-9 节是
真机实测记录）。下一个动作是阶段 C（`ciel build -g app-utils/pwgen`）。


---

## M3 · 让 `ciel build` 端到端跑通（最高优先级）—— ✅ 2026-10-06 完成

更新跑通不等于构建能跑。`acbs-build` 才是真正的考验，它会大量 fork 编译器、
写 `/debs`、往 `/tree` 落构建产物。

做法：直接撞第一个失败点，不要预设。

```bash
cd ~/buildroots
CIEL=~/ciel-rs/target/release/ciel-rs
$CIEL build <某个最小的包> -i arm64
```

按怀疑度排序的已知风险：

1. **`acbs-build` 的子进程行为** —— 它用 `bash4` 的 job control / PTY？需要
   `/dev/ptmx` + `/dev/pts`（当前**没有** bind，只有 6 个字符设备节点）。
   若报 "no such device" 或 job control 错，加 `CIEL_PROOT_EXTRA_OPTIONS`
   临时验证，别急着改代码。
2. **`/tmp` 与 `ld.so.cache`** —— `acbs-build` 会跑 `ldconfig`；`/tmp` 在
   rootfs 里（seed 自 dist，归用户所有），应该没问题，但值得确认。
3. **`/tree` 写权限** —— proot 无 RO bind，`/tree` 是 RW 挂载，只靠约定保护。
   构建后必须 `git -C TREE status` 验干净。
4. **`/debs` 输出** —— 确认 `.deb` 真的落到 `OUTPUT-stable/debs/`。
5. **性能** —— ptrace 5–10x  slowdown，选小包，别拿 cmake/llvm 试。

验收标准：至少一个包成功产出 `.deb`，且 `git -C TREE status` 干净。

---

## M4 · 把 promote 落地成 Rust（恢复 `ciel update-os`）—— 代码已落地 2026-10-14，待真机验收

shell 版 `update-base` 已在真机验证可行。Rust 侧实现状态：

- ✅ `DirCopyManager::commit()`（`src/dircopy.rs`）：rename 换手已实现——
  `dist → dist.pristine-<ts>`、`instances/<n>/root → dist`，O(1) 零拷贝，
  删除语义天然正确。失败回滚（`mv` 回去；回滚也失败时报出「旧基础层在 X」
  的可操作信息）。提升后重建 `manifest/{backend,seeded}`，实例生命周期不受
  影响（后续 `mount()` 会从新基础层重新播种）。备份路径冲突用数字后缀兜底。
- ✅ `update_os()`（`src/actions/container.rs`）：proot 早退 bail 已删除，
  流程恢复「建临时实例 → 更新 → commit → remove_instance」。
- ✅ 新增 2 个 promote 回归测试（`commit_promotes_instance_root_by_rename`、
  `commit_failure_restores_the_base_layer`）——注意 dircopy 测试只能在真机
  Linux/Android `cargo test` 真正执行。
- ✅ **真机翻车两轮已修（2026-10-14/15）**：第一轮 `error creating new backup file
  '/var/lib/dpkg/status-old': EPERM`，第二轮 `unable to stat './usr/bin/ld': EPERM`。
  同一根因：dpkg 大量使用 link()（status 备份、每文件 .dpkg-tmp 备份、payload
  硬链接如 binutils `ld.bfd→ld`），proot link2symlink 的假链接（用户可见 symlink +
  `.l2s.*` holder，带实例专属绝对路径）被 promote 冻进 dist、seed 又带进新实例。
  只删 `.l2s.*` 不够——用户可见的假 symlink 会悬空。修复 =
  `sanitize_l2s_tree()`：可解析的假链接**物化**成真文件（copy holder 内容）、
  断链的删掉、holder 清掉；挂在 `commit()`（治本）与 `seed_instance()` Pass 3
  （治现有脏 dist）两处。`--link2symlink` 不可关（Android 拒绝 App 目录 link()）。
- ⬜ **真机验收**：重编译后 `ciel update-os` 成功；之后 `ciel add` 出的新实例
  `dpkg-query -W aosc-aaa` 直接就是 14.0.0（不用再跑一次 update）；
  `ciel doctor` 无异常；`dist.pristine-<ts>` 出现且内容正确。

原实现要点备忘（与 `scripts/ciel-proot.sh` `promote` 逐条对应）：

### `src/dircopy.rs:709` — `DirCopyManager::commit()`

实现 rename 换手，而不是 rsync 式差异合并：

```
mv .ciel/container/dist              .ciel/container/dist.pristine-<ts>
mv .ciel/container/instances/<n>/root .ciel/container/dist
```

- O(1)、零拷贝。
- **必须用 rename 而非拷贝，因为拷贝不会带删除操作** —— 被升级卸载的包
  会在下次播种时复活。rename 天然正确。
- 提升后要重建 `instances/<n>/manifest/{backend,seeded}`（root 被搬走了，
  但实例生命周期还要继续）。
- 失败必须回滚：`mv` 回去，且回滚失败要给出「基础层现在在 X」这种可操作信息。
- 原 BuildKit 留在 `dist.pristine-<ts>/`，代价是一份 rootfs 磁盘。

### `src/actions/container.rs:430-434` — 去掉 proot bail

`update_os()` 现有流程是「建临时实例 → 更新 → `commit_container()` →
`remove_instance()`」。去掉 bail 后要注意：**promote 把 root 搬走了**，紧跟
其后的 `remove_instance()` → `destroy()` 会对不存在的路径报错，要么容忍，要么
按 promote 里的方式重建骨架。

验收：`ciel update-os -i arm64` 成功；之后 `ciel add` 出的新实例
`dpkg-query -W aosc-aaa` 直接就是 13.3.2（不用再跑一次 update）。

---

## M5 · `packaging.rs` 的两个独立 bug

跟 `/dev` 无关，趁热一起修。

- **`:198` `3u64.pow(i)` = 3+9+27+81+243 = 363 秒纯空等。** 失败时先睡 6 分钟
  才换 apt 再等一轮。
- **`:203` 首次失败就 `oma = false` 静默降级到 apt。** 而
  `APT_UPDATE_SCRIPT`（`src/actions/mod.rs:21`）**既没有 `--force-unsafe-io`
  又带 `--autoremove --purge`** —— 在无特权环境里这是雪上加霜：用一个更弱的
  路径去重试一个可能不是弱路径导致的失败。

改法方向：

1. 只在**同一个** package manager 上重试，不要静默换 manager。
2. 退避有上限（如 5/15/45 秒），别用 `pow`。
3. 失败要硬失败，并且**区分错误类型**：网络/DNS 失败值得重试；`dpkg` 返回
   非 0 重试无意义（那是环境问题，不是瞬时问题），应该直接报错退出。
4. 报错信息要指出「`oma upgrade` 失败，已重试 N 次」而不是只说「update failed」。

验收：断网时 `ciel build` 在 1–2 分钟内明确失败，而不是等 6 分钟再换 apt 磨一轮。

---

## M6 · 测试

- **现有 4 个 dircopy 回归测试必须在真机上 `cargo test`**（gshadow 0400 源 /
  不可读源 / 0500 目录删除 / 可执行位保留）—— Windows 交叉目标跑不了 unix 测试。
- promote 需要新测试：提升后 `dist` 含更新内容、`/proc /sys /dev /run` 仍在、
  提升失败时正确回滚。

---

## 待观察 / 待定

- **dpkg 延迟 chmod 被 bind 的 `/proc` `/sys` 是否 EPERM。** proot-distro +
  Debian 的先例说明最多是 warning（Debian 的 base-files 同样带 `./proc ./sys`
  目录）。若在 AOSC 上变 fatal，fallback 是包操作期间连这两个也不 bind。
- **`W: ... / - EDSP::Resolve (17: File exists)` 噪音**是否随 unpack 成功而
  消失。若干净运行仍出现，那是 oma/rust-apt 的临时文件处理问题，单独查
  （Web 上没有现成案例，sources.debian.org / salsa 对本机 IP network abuse
  封禁，dpkg 源码取不到）。
- **`/dev/pts` / `/dev/ptmx` / `/dev/fd`** 是否需要补 bind。等 M3 撞到再说，
  别提前加。
- **半滚动落地后，`package_build` 里每包一次的 `oma upgrade` 保留还是短路？**
  我倾向**保留** —— promote 之后它是廉价 no-op，正好是理念要求的「始终最新」，
  而且不引入「实例可能过期」的新状态。

---

## 新 chat 怎么开场

让新 session 先读这几个文件，context 就齐了：

1. 本文件（`NEXT-PLAN.md`）
2. `.workbuddy/memory/MEMORY.md`（项目铁律 + 踩过的坑）
3. `TERMUX.md` 末尾两节（promote 设计 + root cause）
4. `scripts/ciel-proot.sh`（手工 proot harness，argv 镜像 `proot.rs`）

## 本仓库的工作流约束（会浪费时间的部分）

- **仓库内文件写入一律 EPERM**（`Write`/`Edit`/`cat >>` 都算）。流程：
  写到 `C:\Users\Sam\.ciel-tmp\` → 改 → `rm` 原文件 → `cp` 回去。
  仓库内覆盖也必须先 `rm`。
- **Windows 上 cargo check 必须带 feature 组合**：
  `cargo check --no-default-features --features proot --target x86_64-unknown-linux-gnu`，
  配合 `~/.zig-local/zig-cc.cmd`（CC）、rustup toolchain 里的
  `llvm-ar/llvm-ranlib`（AR/RANLIB，注意不在 toolchain 根目录）、
  `~/.fake-pkgconfig/pkg-config.cmd`（PKG_CONFIG + `PKG_CONFIG_ALLOW_CROSS=1`）、
  `CARGO_TARGET_DIR=~/.ciel-build-target`。
  **同步到 scratch 时必须覆盖整个 `src/`，且不要覆盖 scratch 的
  `Cargo.toml`/`Cargo.lock`**（那里有 rustix 1.1.4 的 vendored patch）。
  scratch 在 `%LOCALAPPDATA%\Temp\ciel-check`。
- `Write` 工具对 `~/.ciel-tmp/` 下的**已存在**文件也要求先 Read；直接 `rm` 再
  Write 更省事。
