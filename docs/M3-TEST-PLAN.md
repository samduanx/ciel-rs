# M3 · `ciel build` 端到端测试方案

> 基于对 `AOSC-Dev/acbs`（分支 `production`）与 `AOSC-Dev/autobuild4` 源码的实读，
> 以及对 `AOSC-Dev/aosc-os-abbs`（分支 `stable`，4001 个包）的筛选。
> 日期：2026-10-06

---

## 0. 结论先行：NEXT-PLAN 的风险排序需要修正

NEXT-PAN.md 写的第1 条风险是「`acbs-build` 的子进程行为 —— 它用 `bash4` 的 job
control / PTY？」。**方向对了，但主体搞错了，而且它不是「可能」，是「一定」。**

真正需要 PTY 的是 **acbs 自己**：

```python
# acbs/utils.py:214start_build_capture()
process = pexpect.spawn('autobuild', logfile=f, env=cast(os._Environ, env))
term_size = shutil.get_terminal_size()
process.setwinsize(rows=term_size.lines, cols=term_size.columns)
process.interact()
```

而 `app-devel/acbs` 的 `PKGDEP` 里硬写着 `pexpect ptyprocess`：

```
PKGDEP="autobuild4 file libarchive pexpect git mercurial wget pyparsing \
        pycryptodome ptyprocess"
```

所以 BuildKit 里 **一定装了 pexpect** → `utils.py` 顶部 `build_logging = True` →
走的一定是 pexpect 分支（而不是第 339 行的 `subprocess.check_call(['autobuild']`)
退化分支）。

`pexpect.spawn()` 要 `openpty()` → 需要 **`/dev/ptmx` + `/dev/pts/*`**。
当前 `src/backend/proot.rs:123-137` 只 bind 了 6 个字符设备节点，**没有 ptmx/pts**。

**这就是 M3 的第一个必然失败点。**

---

## 1. 选包：为什么是 `app-utils/pwgen`

筛选条件（从 4001 个包里滤出 1919 个候选，再按源码包体积排序）：

| 条件 | 理由 |
|---|---|
| `autobuild/` 下**只有 `defines`** | 没有 patch / 没有自定义 `build` 脚本 →减少变量 |
| **无子目录** | 没有 `patches/` `overrides/` 等 |
| **`BUILDDEP` 为空** | ★ autobuild4 会跑 `apt-get build-dep -y`（`proc/11-pm-deps.sh:16`）。有 BUILDDEP 就多一层网络+dpkg 变量 |
| 非 cmake / meson / rust | 这三个会各自拉一大堆工具链 |
| 源码包 **< 1 MB** | proot 下 ptrace 5-10x slowdown |

### 首选：`app-utils/pwgen`

```
# app-utils/pwgen/autobuild/defines —— 全文，仅 5 行
PKGNAME=pwgen
PKGSEC=utils
PKGDEP="glibc"
PKGDES="Automatic password generator"

# app-utils/pwgen/spec
VER=2.08
SRCS="tbl::https://download.sf.net/pwgen/pwgen-2.08.tar.gz"   # 54,884 字节
CHKSUMS="sha256::dab03dd30ad5a58e578c5581241a6e87e184a18eb2c3b2e0fffa8a9cf105c97b"
```

- 源码包 **54 KB**（实测 content-length）
- `BUILDDEP` 空 → 不会触发 `apt-get build-dep`
- `autobuild/` 目录只有 `defines`（80 字节）
- `ABTYPE` 未设 → autobuild4 `proc/30-build-probe.sh` 自动探测（autotools）
- 纯 C + ncurses-free，`PKGDEP` 只有 glibc

### 备选（按顺序，用于逐级加难度）

1. `runtime-common/chan` —— 纯 C 库，无 BUILDDEP，GitHub tarball。**注意**：`CHKSUMS="SKIP"`，且 SRCS 用 `git::commit=` 走 git 源（会测到 git 拉取路径）
2. `app-utils/bzip3` —— 纯 C，`PKGDEP="glibc"`，源码包稍大但仍是单一 tarball

### 明确避开

| 包 | 原因 |
|---|---|
| `app-doc/pdfgrep` | `PKGDEP="poppler"` —— 得先装 poppler |
| `runtime-gis/rttopo` | `PKGDEP="geos"` |
| `app-utils/scrot` | `PKGDEP="imlib2"` |
| `app-utils/jo` | `BUILDDEP__ARM64="pandoc"` —— 你正好在 arm64 |
| `runtime-common/cjson` / `runtime-doc/md4c` | `ABTYPE=cmakeninja` |
| `runtime-common/tllist` | `BUILDDEP="meson ninja"` |
| 任何 `lang-python/*` | 会撞上同一个 pexpect 问题，且引入 pip/PEP517 变量 |

---

## 2. 分阶段测试：不要直接跑 `ciel build`

`ciel build` 一次跑完 = mount + `repo::init_repo` + `oma upgrade` + acbs。
一旦失败你分不清是哪一层。**按下面 5 个阶段逐个撞，每个阶段单独确认。**

---

### 阶段 A · 环境体检（不碰构建）

```bash
cd ~/buildroots
CIEL=~/ciel-rs/target/release/ciel-rs

# 1) 后端确认 —— 必须是 proot
$CIEL doctor 2>&1 | grep -iA2 backend

# 2) 确认实例是干净的（之前 probe 可能污染过）
$CIEL del -i arm64 2>/dev/null
$CIEL add -i arm64

# 3) harness 体检
./scripts/ciel-proot.sh -C ~/buildroots doctor
```

**通过判据**：`doctor` 里 backend = proot，实例 `arm64` 显示「已就绪」。

---

### 阶段 B · ★ 决定性实验：PTY 到底缺不缺

这是本轮最重要的一步，**先做它，别跳过**。已经封装成一键子命令：

```bash
./scripts/ciel-proot.sh -C ~/buildroots pty-probe arm64
```

它会在 guest 里跑一份独立的 Python 探测脚本，分六段输出：

| 段 | 检查什么 | 对应 acbs 的哪个行为 |
|---|---|---|
| 1 | `/dev/ptmx`、`/dev/pts` 是否存在、是不是字符设备/目录 | 前置条件 |
| 2 | `os.posix_openpt(O_RDWR)` | pexpect → ptyprocess 实际调用的 libc 函数 |
| 3 | `os.openpty()` | `pexpect.spawn()` 底层 |
| 4 | `fork()` + `setsid()` + `TIOCSCTTY` + `exec /bin/echo` | 完整 fork+exec under pty |
| 5 | `import pexpect` 并真的 `pexpect.spawn('/bin/echo')` | **acbs/utils.py:214 的原样调用** |
| 6 | tl;dr 判定 | — |

> **为什么是独立脚本而不是 `python3 -c "..."`？**
> 宿主 bash → proot → guest `/bin/bash -c` → `python3 -c` 是**四层引号嵌套**，
> 而 Python 代码里必然有换行和双引号。2026-10-06 实际翻车过一次：
> `unexpected EOF while looking for matching '"'` —— 报错指向引号，
> 真因却是 `build_argv` 用换行分隔 argv，把一段多行 Python 拆成了 4 个
> 独立 argv 元素（详见第 6 节）。写成文件后代码完全不需要转义。

**三种结果对应三条不同的路：**

| 结果 | 含义 | 下一步 |
|---|---|---|
| `posix_openpt OK` | proot 不知怎么已经能开 pty（可能 Android宿主 `/dev/ptmx` 被 `-b` 意外覆盖到） | 直接进阶段 C |
| `posix_openpt ENOENT` | 确认缺 ptmx（**预期结果**，当前只 bind 了 6 个字符设备节点） | 走下面的对照实验确认 acbs 因此挂 |
| `posix_openpt EPERM`/`EACCES` | ptmx 存在但内核拒绝（SELinux / devpts 未挂载） | 光bind 设备节点不够，需要真 devpts |

脚本末尾还会跑一遍宿主（Termux）侧的 forkpty 对照，用来区分
「Android 宿主根本没有 devpts」和「proot 没把它 bind 进去」—— 这两种情况修法完全不同。

**对照实验（不改任何代码，10 分钟内出结论）：**

```bash
# 临时让 pexpect import 失败 → acbs 走 subprocess.check_call 分支，不需要 PTY
./scripts/ciel-proot.sh -C ~/buildroots run arm64 \
  /bin/bash -c 'mv /usr/lib/python3*/site-packages/pexpect /usr/lib/python3*/site-packages/pexpect.disabled 2>/dev/null; python3 -c "import pexpect" 2>&1 | tail -1'
```

> 该改动只在实例里，`ciel rollback -i arm64`即可撤销。
> **不要**把它 promote 成基础层（M4 之后会变成永久状态）。

屏蔽掉之后 acbs 会打印
`WARNING: Build logging not available due to pexpect not installed.`
然后走`subprocess.check_call(['autobuild'])`。如果这样能跑通，
就**确定性地证明 PTY 是唯一障碍**，接下来只需要解决 PTY，别的都不用查。

> ⚠️按项目铁律，**改 acbs 源码前必须先读 `.ciel/container/dist/` 里那份**
> （BuildKit 实际安装的版本）。上面的行号来自上游 `production` 分支，
> 仅用于定位；真要改之前用 `.ciel/container/dist/usr/lib/python3*/site-packages/acbs/utils.py`
> 核对。

---

### 阶段 C · 只下载源码（不编译）

用 `ciel build -g`（FETCH，`cli.rs:133`）单独验证「网络 + wget + bsdtar + SRCS bind」这一层：

```bash
$CIEL build -g app-utils/pwgen -i arm64
```

背后是 `packaging.rs:289`：
```rust
let mut cmd = vec!["/bin/acbs-build", "-g", "--"];
```

**通过判据**：
```bash
sha256sum SRCS/0ba92080422607d88128703a417826bed0cb5eedd09b7ab4e18a7a7e011e9749
# 应输出 dab03dd30ad5a58e578c5581241a6e87e184a18eb2c3b2e0fffa8a9cf105c97b（spec 的 CHKSUMS）
#
# ★ 验证命令勘误（2026-10-06 阶段C 实测）：acbs 的缓存文件名不是上游文件名，
#   而是 **URL 的 sha256**（本例 sha256("https://download.sf.net/pwgen/pwgen-2.08.tar.gz")
#   = 0ba92080...，与宿主 SRCS/ 下实际文件名逐字符一致）。所以 `ls SRCS/ | grep pwgen`
#   永远是空的——那是测试计划写错了，不是下载失败。
#   实测已双确认：内容 sha256 = spec 的 dab03dd3...（真 pwgen）；字节数 54884 = 上游 content-length。
```

若失败，先定位是哪一层：
- `wget` 失败 → 网络 / DNS / `resolv.conf` bind
- `bsdtar --no-xattrs -xf` 失败 → SRCS bind 写权限，或 Android 上 tar 缺 xattr 支持
- acbs 本身 import 失败 → Python 依赖缺失

---

### 阶段 D · 手工跑一次完整 acbs-build（绕过 ciel 的循环）

```bash
./scripts/ciel-proot.sh -C ~/buildroots run arm64 \
  /bin/bash -c 'cd /root && NOPARALLEL=1 /bin/acbs-build -- app-utils/pwgen 2>&1 | tail -60'
```

**为什么要手工跑**：绕开 `packaging.rs:187-210` 那个 5 次重试 + 363 秒空等的循环
（M5 要修的东西），让错误直接暴露。

**为什么加 `NOPARALLEL=1`**：`autobuild4/proc/12-parallel.sh` 默认
`MAKEFLAGS=-j$ABTHREADS`。proot 下 ptrace 5-10x slowdown 叠N 并行，
手机上很容易 OOM 或被调度器打死。第一个包必须串行。

**通过判据**：
```bash
ls -la OUTPUT-*/debs/ | grep pwgen     # 应有 pwgen_2.08-*.deb
```

---

### 阶段 E · 走完整的 `ciel build`

前四步都通了再跑这个：

```bash
$CIEL build app-utils/pwgen -i arm64
```

**完整验收清单**（对应 NEXT-PLAN 的验收标准）：

```bash
# 1) .deb 真的产出了
ls -la OUTPUT-*/debs/*.deb

# 2) /tree 干净（proot 无 RO bind，只靠约定保护）
git -C TREE status --porcelain          # 必须为空

# 3) SRCS 没留垃圾
ls SRCS/

# 4) 本地仓库刷新了
head -20 OUTPUT-*/debs/Packages
```

### 阶段 F · 加难度（可选，确认不是侥幸成功）

```bash
$CIEL build runtime-common/chan -i arm64      # 纯 C 库 + git 源 + CHKSUMS=SKIP
$CIEL build app-utils/bzip3 -i arm64# 稍大一点的单一 tarball
```

这两个过了，说明 acbs 流水线（fetch → 依赖解析 → autobuild4 → dpkg-deb →
本地 repo 刷新）在 proot 下是真的通了。

---

## 3. 撞到 PTY 缺口的修法（按优先级，别提前做）

按 NEXT-PLAN「别提前加，等 M3 撞到再说」的原则，这里只列，**等阶段 B 的结果再动手**。

### 修法 1 · 加 `CIEL_PROOT_EXTRA_OPTIONS`（零代码，先验证）

`proot.rs:164` 已经留了这个口子：

```bash
export CIEL_PROOT_EXTRA_OPTIONS="-b /dev/ptmx"
./scripts/ciel-proot.sh -C ~/buildroots pty-probe arm64   # 直接复跑探测
```

`ciel-proot.sh` 与 `proot.rs:164` 都读这个环境变量，所以**同一条命令能同时验证
harness 和 Rust 侧**。加了之后 `pty-probe` 第 2 段（`posix_openpt`）应该从
ENOENT 变成 OK；如果变成 EPERM/EACCES，说明是 SELinux / devpts 未挂载，
bind 设备节点不够。

注意 `/dev/pts` 是个**目录**（devpts 挂载点），proot 的 `-b` 对目录语义不同，
可能需要 `-b /dev/pts:/dev/pts`。**Android 宿主有没有 `/dev/pts`、SELinux 放不放行，
都要实测**——proot-distro 在 Android 上能用 PTY 不代表这里能。
`pty-probe` 末尾会自动跑一遍宿主侧的 forkpty 对照来区分这两种情况。

### 修法 2 · 让 acbs 不走 pexpect（改 BuildKit 里的 acbs）

在 `start_build_capture()` 调用前加一个环境变量判断。代价：丢 build log 与 winsize，
但换来一个能跑的构建流水线。**适合先让 M3 过，不适合长期方案。**

### 修法 3 · proot.rs 正式加 ptmx/pts

等修法 1 验证过再落地到 `src/backend/proot.rs:123-137` 那个设备节点列表里。
**注意这条与2026-10-06 的 `/dev` 教训不冲突**：整体 bind `/dev` 会让 guest 的
`/dev` 变Android 的只读 tmpfs（aosc-aaa 的 `./dev/shm` mkdir EACCES 就是这么来的），
但 `-b /dev/ptmx` 这种**单个节点** bind 是安全的。

---

## 4. 与 NEXT-PLAN 的差异汇总

| NEXT-PLAN 原文 | 本轮修正 |
|---|---|
| 风险 1：「`acbs-build` 用 `bash4` 的 job control / PTY？」 | 主体是 **acbs 的 pexpect**，不是 bash4。且**不是「若报错的話」，是必然**——`app-devel/acbs` 硬依赖 pexpect |
| 风险 1 修法：「加 `CIEL_PROOT_EXTRA_OPTIONS` 临时验证」 | ✅ 方向对，`proot.rs:164` 已有这个口子 |
| 风险 5：「性能 —— ptrace 5–10x slowdown，选小包」 | ✅ 对，但还要**显式禁并行**（`NOPARALLEL=1`），否则 `-j$ABTHREADS` 会打爆手机 |
| 未提及 | ★ BUILDDEP 非空会触发 `apt-get build-dep -y`，是独立的一层网络+dpkg 变量 → 首包必须 BUILDDEP 空 |
| 未提及 | ★ 应该先用 `ciel build -g`（只下载）分层，再跑完整 build |
| 未提及 | ★ 应该先用 `ciel-proot.sh` 手工跑 acbs-build，绕开 packaging.rs 的 363s 空等循环 |
| 未提及 | ABBS 默认分支是 `stable`，acbs 仓库默认分支是 `production`（都不是 master） |

---

## 5. 命令速查

```bash
# 阶段 A
cd ~/buildroots && CIEL=~/ciel-rs/target/release/ciel-rs
$CIEL del -i arm64; $CIEL add -i arm64
$CIEL doctor
./scripts/ciel-proot.sh -C ~/buildroots doctor

# 阶段 B（★ 先做这个，一键）
./scripts/ciel-proot.sh -C ~/buildroots pty-probe arm64

# 阶段 C：只下载
$CIEL build -g app-utils/pwgen -i arm64
sha256sum SRCS/0ba92080422607d88128703a417826bed0cb5eedd09b7ab4e18a7a7e011e9749  # = dab03dd3... 即验证通过

# 阶段 D：手工完整构建（串行，绕过重试循环）
./scripts/ciel-proot.sh -C ~/buildroots run arm64 \
  /bin/bash -c 'NOPARALLEL=1 /bin/acbs-build -- app-utils/pwgen'
ls -la OUTPUT-*/debs/*.deb

# 阶段 E：走 ciel
$CIEL build app-utils/pwgen -i arm64
git -C TREE status --porcelain      # 必须为空

# 阶段 F：加难度
$CIEL build runtime-common/chan -i arm64
$CIEL build app-utils/bzip3 -i arm64
```

---

## 6. 排障：`unexpected EOF while looking for matching '"'`

**症状**：通过 `ciel-proot.sh run <inst> /bin/bash -c '<多行代码>'` 传一段
带换行的 Python 时报：

```
/bin/bash: -c: line 1: unexpected EOF while looking for matching '"'
```

**这不是引号写错了。** 真因是 `build_argv` 曾经用**换行**分隔 argv：

```bash
printf '%s\n' "${argv[@]}"      # 传输用换行
mapfile -t argv < <(...)         # 接收也按换行切
```

于是一个含换行的 argv 元素被**静默拆成多个**。实测对照：

| 版本 | argc | 结果 |
|---|---|---|
| 换行分隔（旧） | **7** | 一段 6 行 Python 被拆成 4 个碎片，guest 只收到第一行 |
| NUL 分隔（现） | **4** | 完整 1 个元素，`\n` 原样保留 |

**已修**：`build_argv` 改用 `printf '%s\0'`，`run_guest` 改用 `mapfile -d '' -t`。
显示 argv 的两处（`doctor`、`argv` 子命令）加了 `| tr '\0' '\n'` ——
**传输用的分隔符和显示用的分隔符刻意不同**。

> **注意 Rust 侧不受影响**：`proot.rs` 直接把 `OsString` 交给 `Command::args()`，
> 没有这个中间层。这个 bug 只存在于 shell harness。
> 但它也意味着「harness 能跑通」不等于「Rust 侧行为一致」——
> 涉及 argv 传递的对照实验，两边都要验。

**通用建议**：在这个 harness 里**不要用 `python3 -c '<多行代码>'`**。
四层引号嵌套（宿主 bash → proot → guest bash → python）本身就脆弱，
叠加上传输层问题，报错信息还会指向错误的行。写成独立 `.py` 文件、
拷进 `$ROOT/tmp/`、用 guest 视角绝对路径跑（`syscall-probe` / `pty-probe`
都是这个套路）。
---

## 7. 阶段 B 实测结果（2026-10-06，真机 arm64）

`pty-probe arm64` 的输出存档在 `~/Desktop/pty-probe.log`。**PTY 障碍从假设变成了实锤。**

### 7.1 铁证链（四段互相印证，无猜测空间）

```
== 1 ==  stat(/dev/ptmx)      ERR 2 (ENOENT)      ← guest 没有 ptmx
         stat(/dev/pts)       ERR 2 (ENOENT)      ← 也没有 devpts
== 2 ==  posix_openpt(O_RDWR) ERR 2 (ENOENT)      ← pexpect→ptyprocess 实际调的 libc
== 3 ==  os.openpty()         ERR 2 (ENOENT)
== 5 ==  import pexpect       OK   pexpect 4.9.0  ← 所以 acbs 必走 pexpect 分支
         import ptyprocess    OK   ptyprocess 0.7.0
         pexpect.spawn(...)   OSError: out of pty devices
```

traceback 闭合：`ptyprocess.py:230 pty.fork()` → `pty.py:78 openpty()` →
`pty.py:34 _open_terminal()` → `pty.py:58 raise OSError('out of pty devices')`。
`os.openpty()` 已 ENOENT（第 3 段独立证实），Python 退回 BSD 式 `/dev/ptyXX`
也全部失败。**`acbs/utils.py:214` 的 `pexpect.spawn('autobuild', ...)` 在 guest
里必然抛这个异常。**

### 7.2 意外收获：proot 下 `os.fork()` 是好的

第 4 段报 `TypeError('cannot unpack non-iterable int object')` —— 这是探测脚本
自身的 bug（`pid, fd = os.fork()`，`os.fork()` 返回裸 int），但**fork 本身成功了**，
于是父/子两个进程都继续跑完了脚本剩余部分。这就是为什么日志里第 4 段之后
每行都出现两遍、第 34-85 行是两份 traceback 交织。

**副产物结论：proot 处理 fork/clone 没有问题，只有 pty 设备分配失败。**
这排除了「proot 处理不了 clone」这整类嫌疑，也把问题范围收窄到
「/dev/ptmx 不存在」这一个点上。

> 探测脚本 bug 已修（`os.fork()` → `os.forkpty()`，子进程路径改为 exec 或
> `_exit` 双保险），顺带把宿主侧对照从「只截第 4 段」改成跑**完整** probe ——
> 上一版截取的那段恰好是当时唯一有 bug 的地方，等于宿主侧什么数据都没拿到。

### 7.3 还缺的一个数据点：宿主 devpts 行不行

修法 A（bind `/dev/ptmx`）能否成功，取决于 **Android/Termux 宿主有没有能用的
devpts**。间接证据很强（tmux/ssh/script 在 Termux 都能用，它们都要 PTY），
但还没直接测过。修好的 `pty-probe` 末尾会跑宿主完整 probe，直接给出答案。

### 7.4 下一步（按顺序，别跳）

**Step 1 · 同步修好的脚本到真机，重跑：**

```bash
# 本机 → 手机
scp scripts/ciel-proot.sh <phone>:~/ciel-proot.sh
# 手机上
~/ciel-proot.sh -C ~/buildroots pty-probe arm64
```

重点看**宿主段**的 `posix_openpt` / `os.openpty` 两行。

**Step 2 · 若宿主 openpty OK → 走修法 A：**

```bash
export CIEL_PROOT_EXTRA_OPTIONS="-b /dev/ptmx -b /dev/pts:/dev/pts"
~/ciel-proot.sh -C ~/buildroots pty-probe arm64
```

- guest `posix_openpt` 从 ENOENT 变 **OK** → PTY 解决，直接进阶段 C/D/E
- 变 **EPERM/EACCES** → SELinux 拦了，光 bind 节点不够，走 Step 3

**Step 3 · 若 Step 2 失败 → 走修法 B（stub pexpect）：**

让 `import pexpect` 抛 ImportError，逼 acbs 退到 `subprocess.check_call`
（`utils.py:339`，不需要 PTY）。做法是 bind 单个文件盖住 `__init__.py`：

```bash
# 宿主上准备 stub
mkdir -p ~/pty-stub
printf 'raise ImportError("PTY unavailable in ciel proot backend; "\\\n         "forcing acbs subprocess fallback")\n' > ~/pty-stub/pexpect-stub.py
# 验证（guest python 路径以实测为准，日志显示是 python3.14）
export CIEL_PROOT_EXTRA_OPTIONS="-b ~/pty-stub/pexpect-stub.py:/usr/lib/python3.14/site-packages/pexpect/__init__.py"
~/ciel-proot.sh -C ~/buildroots pty-probe arm64
# 第 5 段应变成 "import pexpect ERR ImportError(...)"，第 6 段 VERDICT 变 no-pexpect
```

> **不能用空目录 bind 盖住整个 pexpect 包**：Python 3.3+ 会把没有
> `__init__.py` 的目录当 namespace package，`import pexpect` 照样成功，
> 只是变成空模块，后续 `pexpect.spawn` 会以 AttributeError 崩溃 —— 比现在更糟。
> 必须 bind **单个文件**盖住 `__init__.py`。

修法 B 的代价：没有 build log 与 winsize（`acbs-build` 的编译输出会直接
混进 ciel 的 stdout，不再单独落 `/var/cache/acbs/build/acbs-build_*.log`）。
对 M3 验收（产出 .deb + tree 干净）没有影响。

**Step 4 · 确认 guest python 路径后再动手：**

日志显示 guest 是 `python=3.14.7`，site-packages 路径是
`/usr/lib/python3.14/site-packages/`。修法 B 的 bind 目标路径必须与实测一致，
先跑一次确认再写死。

### 7.5 一个需要留意的风险：修法 A 的 `/dev/pts` bind

`-b /dev/pts:/dev/pts` 会把 Android 宿主的 devpts 实例共享给 guest。
guest 与 Termux 自身的 pty 编号会混在同一个 devpts 里。对 `openpty` 语义
没有影响（内核按 fd 分配），但意味着 guest 里 `os.ttyname()` 返回的
`/dev/pts/N` 与 Termux 的 pty 共享命名空间。`pty-probe` 的第 4 段
（forkpty + exec + 读回）会直接暴露这类问题，所以**修法 A 验证时必须看
第 4 段是不是 OK**，不能只看第 2 段。

---

## 8. 修法 A/B 实测复盘（2026-10-06 晚）：两次实测的选项都没传到 proot

存档：`~/Desktop/pty-probe-method-a.log`、`pty-probe-method-b.log`。

### 8.1 真正的根因：harness 没转发 `CIEL_PROOT_EXTRA_OPTIONS`

两份 log 的 guest 段与基线（第 7 节）**逐字节相同** —— ptmx/pts 仍 ENOENT、
pexpect 仍 import 成功、仍 `out of pty devices`。两个修法"无效"的表象完全一致。

真因：**`ciel-proot.sh` 的 `build_argv` 从来没读过 `CIEL_PROOT_EXTRA_OPTIONS`**，
只有 Rust 侧（`proot.rs:164`）读它。而 pty-probe 的建议文本让用户 export 它 ——
是我在建议里给了一个 harness 不处理的变量。选项从未到达 proot，guest 当然不变。

### 8.2 这次实测真正拿到的宝贵数据：宿主 devpts 完全可用

两份 log 的宿主段（相同）给出了修法 A 需要的全部前置条件：

```
stat(/dev/ptmx)      OK  S_ISCHR=True            ← Android 有 ptmx 字符设备
stat(/dev/pts)       OK  S_ISDIR=True            ← devpts 已挂载
listdir(/dev/pts)    ERR 13 (EACCES)             ← 正常：Android 不让 app 列 /dev/pts
posix_openpt(O_RDWR) OK  3                       ← ★ 能开 pty
os.openpty()         OK  (4, 5)                  ← ★ master+slave 都拿到
forkpty + exec       OK  ('PTY_FORK_OK','exit=0')← ★ 端到端 fork+exec+读回全通
```

**结论：修法 A 可行。** Termux 宿主的 devpts 是完整可用的（这与
tmux/ssh/script 在 Termux 能用的间接证据吻合）。`listdir` 的 EACCES 无害 ——
`openpty` 不需要列目录，只需要 open `/dev/ptmx` 和 `/dev/pts/N`。

### 8.3 顺带暴露的第二个坑：`~` 在双引号里不展开

修法 B 的命令写的是：

```bash
export CIEL_PROOT_EXTRA_OPTIONS="-b ~/pty-stub/pexpect-stub.py:..."
```

bash 在**双引号里不展开 `~`**，所以这个值里是字面 `~/pty-stub/...`；
而 proot 不是 shell，也不展开。即使 harness 转发了，这个 bind 也会失败。
**正确写法用 `$HOME`**（双引号里 `$HOME` 会展开）：

```bash
export CIEL_PROOT_EXTRA_OPTIONS="-b $HOME/pty-stub/pexpect-stub.py:/usr/lib/python3.14/site-packages/pexpect/__init__.py"
```

harness 与 `proot.rs` 现在都会对以 `~/` 开头的 token 自行展开成 `$HOME/` 兜底，
但别依赖这个兜底 —— 显式用 `$HOME` 两边语义完全一致。

### 8.4 已做的修复

| 文件 | 修复 |
|---|---|
| `scripts/ciel-proot.sh` `build_argv` | 补上 `CIEL_PROOT_EXTRA_OPTIONS` 转发（按空白切 token 追加到 argv，位置与 `proot.rs:164` 对应），并对 `~/` 前缀做 `$HOME` 展开 |
| `scripts/ciel-proot.sh` `cmd_pty_probe` | **自诊断**：开头先打印本次实际生效的 extra options，含字面 `~` 时告警 —— 「选项没传到」这类问题从此一眼可见，不会再伪装成「修法无效」 |
| `src/backend/proot.rs:164` | 同样加 `~/` → `$HOME` 展开（proot 不展开 `~`；你下一步跑 `ciel build` 时同样会踩） |

`cargo check --no-default-features --features proot --target x86_64-unknown-linux-gnu`
已通过（14.93s）。

### 8.5 下一步：重跑修法 A（宿主数据说它应该能行）

```bash
# 手机上，注意用 $HOME 而不是 ~
export CIEL_PROOT_EXTRA_OPTIONS="-b /dev/ptmx -b /dev/pts:/dev/pts"
~/ciel-proot.sh -C ~/buildroots pty-probe arm64
```

**判读（现在 pty-probe 开头会先回显 extra options，先确认它出现了）：**

| guest 第 2 段 `posix_openpt` | 含义 | 动作 |
|---|---|---|
| `OK <fd>` | PTY 通了 | **必须再看第 4 段** `forkpty + exec` 是否 OK（`/dev/pts` bind 会共享宿主 devpts 实例，第 2 段过了不代表 forkpty 链路通）。两段都 OK → 直接进阶段 C/D/E，acbs 能拿到 build log，这是最优结果 |
| `ERR 2 ENOENT` | bind 没生效（看开头回显段确认选项在不在） | 回来查转发 |
| `ERR 13 EACCES` / `EPERM` | SELinux 拦截 | 走修法 B |

**修法 B 重跑**（A 失败时）：

```bash
mkdir -p ~/pty-stub
printf 'raise ImportError("PTY unavailable in ciel proot backend; "\\\n         "forcing acbs subprocess fallback")\n' > ~/pty-stub/pexpect-stub.py
export CIEL_PROOT_EXTRA_OPTIONS="-b $HOME/pty-stub/pexpect-stub.py:/usr/lib/python3.14/site-packages/pexpect/__init__.py"
~/ciel-proot.sh -C ~/buildroots pty-probe arm64
# 第 5 段应变 "import pexpect ERR ImportError(...)"，第 6 段 VERDICT 变 no-pexpect
```

**M3 之后**：确认哪个修法可用后，把对应的 `-b` 选项固化进 `proot.rs` 的
设备节点/bind 列表（A），或做成 config.toml 里的开关（B），别靠环境变量长期兜着。

---

## 9. 方法 A 定案（2026-10-06 晚，`pty-probe-method-a-v2.log`）：PTY 链路全通

转发修复后重跑，`-b /dev/ptmx -b /dev/pts:/dev/pts` 真正生效：

```
== 1 ==  stat(/dev/ptmx) OK S_ISCHR=True / stat(/dev/pts) OK S_ISDIR=True
== 2 ==  posix_openpt(O_RDWR)   OK 3        ← 基线是 ENOENT
== 3 ==  os.openpty()           OK (4, 5)   ← 基线是 ENOENT
== 4 ==  forkpty + exec + 读回  OK ('PTY_FORK_OK','exit=0')
== 5 ==  pexpect.spawn(...)     成功         ← 见下
```

### 9.1 第 5 段的 traceback 其实是成功的

traceback 抛在 `child.expect(pexpect.TIMEOUT, timeout=10)`，**不是** `spawn()`。
dump 证明 spawn 完全成功：`pid: 10904`、`child_fd: 8`、`exitstatus: 0`、
`before: b'PTY_PEXPECT_OK\r\n'`（echo 的输出被完整读到）。
是探测脚本写错了：`echo` 立即退出产生 EOF，而脚本 `expect(TIMEOUT)` 等超时。
**PTY 四段全通**：posix_openpt → openpty → forkpty+exec+读回 → pexpect.spawn 实跑。
探测脚本已改为 `expect(pexpect.EOF)` 并校验输出内容。

### 9.2 已固化进代码（环境变量退出历史舞台）

| 文件 | 改动 |
|---|---|
| `src/backend/proot.rs` | `/dev` 单节点 bind 列表后新增：**`/dev/ptmx` + `/dev/pts` 成对 bind**，门控 `Path::new("/dev/ptmx").exists() && Path::new("/dev/pts").is_dir()`。只 bind ptmx 会让 posix_openpt 成功但 slave 打不开（glibc openpty 要 open(ptsname)），所以必须成对。注释完整记录根因与真机证据 |
| `scripts/ciel-proot.sh` `build_argv` | 同样的成对 bind + 同样的门控（harness 头注要求与 proot.rs 同步） |
| `scripts/ciel-proot.sh` pty-probe | 修掉 `expect(TIMEOUT)` bug；tl;dr 更新为「bind 已内置，此失败 = 宿主缺 devpts」 |

验证：`cargo check --no-default-features --features proot --target x86_64-unknown-linux-gnu` 通过（15.49s）；
门控两分支都实测（Git Bash 有 ptmx 无 pts → 正确跳过；模拟双全 → 正确追加）。

**现在手机上应 `unset CIEL_PROOT_EXTRA_OPTIONS`**（bind 已内置，重复传无害但是噪音），然后重建 cieli 二进制再继续。

### 9.3 下一步：M3 阶段 C/D/E

PTY 这条最大障碍已清除，回到 M3-TEST-PLAN.md 第 2 节的原计划：

```bash
# 阶段 C：只下载（验证 wget + bsdtar + SRCS bind）
$CIEL build -g app-utils/pwgen -i arm64
sha256sum SRCS/0ba92080422607d88128703a417826bed0cb5eedd09b7ab4e18a7a7e011e9749  # = dab03dd3... 即验证通过

# 阶段 D：手工完整构建（串行）
./ciel-proot.sh -C ~/buildroots run arm64 \
  /bin/bash -c 'NOPARALLEL=1 /bin/acbs-build -- app-utils/pwgen'
ls -la OUTPUT-*/debs/*.deb

# 阶段 E：走 ciel + 验收
$CIEL build app-utils/pwgen -i arm64
git -C TREE status --porcelain      # 必须为空
```

**阶段 D 现在值得预期的新风险**（PTY 通了之后才轮得到它们的）：
- acbs 的 `apt-get build-dep`（pwgen BUILDDEP 空，应跳过）
- autobuild4 的 `dpkg-deb -b` 与 `/debs` bind 写入
- acbs 对 `/tree` 的 `git describe/status`（脏检测读，应无写入）

---

## 10. 阶段 C 实测通过（2026-10-06 21:26）：download-only 全绿，"SRCS 里没有 pwgen"不是问题

`ciel build -g app-utils/pwgen -i arm64` 真机结果（Rust 侧二进制，含固化的 ptmx/pts bind）：

```
rolled back: 10272 directories, 0 files (0 B), 0 symlinks   ← 早期 dpkg-probe 实验留下的空目录，正常
Welcome to ACBS - 20260930
[1/1] pwgen → Dependencies resolved
wget: download.sf.net → 301 → 301 → 302 → sf-west-interserver-1... 200 OK
Length: 54884 (54K)  →  saved to /var/cache/acbs/tarballs/0ba92080...e9749
ACBS Build Successful — pwgen  Download only
```

**阶段 C 的三个验证目标全部达成：**

| 目标 | 证据 |
|---|---|
| guest 网络 + DNS | wget 解析并跟随 3 次重定向（301×2 + 302），TLS 握手成功 —— resolv.conf bind 与 proot 网络直通都正常 |
| SRCS bind 可写（write-through） | 文件落在**宿主** `SRCS/0ba92080...e9749` —— guest 的 `/var/cache/acbs/tarballs` 写透到宿主 |
| ciel build -g 全流水线 | mount → 选择性同步回滚 → acbs 依赖解析（1/1）→ 抓取 → Download only 成功 |

**"问题"的真相**：`ls SRCS/ | grep pwgen` 为空是**测试计划的验证命令写错了**。
acbs 的缓存文件名不是上游文件名，而是**下载 URL 的 sha256**——
`sha256("https://download.sf.net/pwgen/pwgen-2.08.tar.gz") = 0ba92080...e9749`，
与宿主 SRCS/ 下的实际文件名逐字符一致。内容本身已双确认是真的 pwgen：
`sha256sum` = spec 的 `dab03dd3...`，字节数 54884 = 上游 content-length。

顺带：acbs 构建期还会用 spec 的 CHKSUMS 再校验一次内容，所以即使不手动
sha256sum，阶段 D 的 bsdtar 解包前也有一次官方校验。

**下一步：阶段 D**（真正的编译，PTY/编译器/dpkg-deb 首秀）：

```bash
./ciel-proot.sh -C ~/buildroots run arm64 \
  /bin/bash -c 'NOPARALLEL=1 /bin/acbs-build -- app-utils/pwgen'
ls -la OUTPUT-*/debs/*.deb
```

缓存已在 SRCS/，这次不会再下载。若构建成功，`/debs` bind 会把 .deb 透传到宿主 `OUTPUT-stable/debs/`。

---

## 11. 阶段 D 失败分析与修复（2026-10-06 22:00）：umask 0077 泄漏进 guest

`pwgen-compile-log.log`：**编译本身全部成功**（40 秒跑完 checksum→解包→
autoreconf→configure→make→install→strip→QA），死在最后一步打包：

```
dpkg-deb: error: control directory has bad permissions 700 (must be >=0755 and <=0775)
/usr/lib/autobuild4/pm/dpkg.sh:201: error: command exited with 2   ← pm_build_package
```

### 11.1 根因：harness 没设 umask，Termux shell 的 0077 泄漏进 guest

- ab4 的 `mkdir -p "$PKGDIR"/DEBIAN`（pm/dpkg.sh:182，**无 `-m`**）新目录模式 =
  `0777 & ~umask`。得到 700 → **创建时刻 umask 必为 0077**。
- 排查了全部嫌疑：acbs（全部 .py）、autobuild4（全部 .sh + native/*.cpp）、
  pexpect 4.9.0、ptyprocess 0.7.0 —— **没有任何 umask 调用**。
- 唯一差异：这次走的是 **harness**（`./ciel-proot.sh run`），它从不设 umask，
  直接继承 Termux 交互 shell 的 umask。而 ciel 的 main.rs 第一件事就是
  `libc::umask(022)`（上游注释原话："ensure correct permissions on rootfs"）。
  阶段 C 走的是 ciel 二进制，所以没踩到。
- 机械验证：umask 0077 → mkdir DEBIAN = **700**（报错数字）；0022 → **755**（要求值）。
- 为什么之前 harness 跑的 oma upgrade / dpkg-probe 全都没事：dpkg 按包内
  元数据**显式**设模式，对 umask 免疫；构建树（mkdir/install 创建源码目录、
  abdist、DEBIAN）是第一个 umask 敏感环节。

### 11.2 修复

`ciel-proot.sh` 头部加 `umask 022`（对齐 ciel main.rs）。已装机。

手机上先确认一下你的 Termux umask（应打印 0077）：

```bash
umask
```

然后**重新同步 harness 到 ~/ciel-proot.sh**（脚本顶部多了 umask 022），重跑：

```bash
~/ciel-proot.sh -C ~/buildroots run arm64 \
  /bin/bash -c 'NOPARALLEL=1 /bin/acbs-build -- app-utils/pwgen'
```

tarball 已缓存在 SRCS/，不会再下载；编译部分 ~40s。也可以用 acbs 存的
checkpoint 续跑：`acbs-build --resume /tmp/1791295664.acbs-ckpt`（更慢，无优势）。

### 11.3 对 M3 的意义

- **这不会影响阶段 E（`ciel build`）**——Rust 侧 main.rs 本来就设 022。
  harness 补齐只是让它兑现"argv 构造原样搬到 shell"的承诺（环境也得一致）。
- 值得记录的正面信号：**PTY/pexpect 链路在真实构建里完全正常**（autobuild
  通过 pexpect 交互运行、build log 落盘、setwinsize 正常）；LTO/strip/elf
  filter 全过。pwgen 只差最后一步 chmod 语义。

---

## 12. 阶段 F：chan ✅ / bzip3 ❌（2026-10-06 23:00）——git fetcher 的零拷贝拷贝在 proot 上炸

### 12.1 chan：全绿，且验证了大量新东西

```
dpkg-deb: building package 'chan' / 'chan-dbg'            ← umask 修复实战确认 + 符号包也产出
Selecting/Unpacking/Setting up chan (0.0.4)                ← ab4 把产物安装进了实例（80-pm-pack-install）
'chan_0.0.4-0_arm64.deb' -> '/debs/c/chan_0.0.4-0_arm64.deb'  ← 归档到 /debs（分支子目录 c/）
[INFO]: Asking ciel to refresh repository...
info: Scanning 4 packages...                               ← ★ acbs↔ciel inotify 监控握手成功
ACBS Build Successful — chan 0:01:06
```

acbs 打包 → **安装进实例** → 归档 → 通过 `fresh.lock` + inotify 让 ciel 刷新本地
repo → 包间 rollback（10272 dirs, 20 files, 43.1 MiB）→ bzip3 开始。整条
ciel↔acbs 集成链路全部打通。

### 12.2 bzip3：fetch 阶段失败——不是依赖问题

失败点在 `acbs/fetch.py:207 git_processor → shutil.copytree`（把 git bare 仓库
从 SRCS bind 拷到构建目录的 `.git`），**还没到编译**；`apt-get build-dep` 显示
"0 newly installed"，与依赖无关。这是 bzip3 spec 用 `git::` 源（chan/pwgen 是
`tbl::` tarball，走 bsdtar，不受影响）触发的独立问题：

```
shutil.Error: [Errno 22] Invalid argument: '.../objects/pack/pack-*.pack'
```

### 12.3 根因（读 CPython 3.14 shutil.py + copy_file_range(2) man page 实锤）

CPython `copyfile` 的两个零拷贝快速路径（`_fastcopy_copy_file_range` →
`_fastcopy_sendfile` → `copyfileobj` 降级链）有个结构性的脆弱点：

- `blocksize = max(st_size, 8MiB)` —— 对 400KB 的 pack 文件请求 8MiB，
  **range 超出源文件 EOF**；
- `copy_file_range(2)` 旧内核语义对此返回 **EINVAL**（"Requested range extends
  beyond the end of the source file"；Linux 5.3 rework 后新内核允许并在 EOF
  返回 0 —— 设备内核行为属旧语义/混合）；
- CPython 只在**首次调用且零写入**时降级（`_GiveupOnFastCopy`）；**首调成功、
  后续调用 EINVAL** 时 `raise err`（且 `err.filename = fsrc.name` —— 这就是
  报错带 SRC 路径的出处）；
- 小文件即使首调 EINVAL 也会静默降级存活，所以错误列表全是 pack 文件。
- `_copyxattr` 的 EINVAL 在 3.14 已被吞掉 —— xattr 嫌疑排除。

结论：**proot/Android 内核 × CPython 零拷贝路径的环境组合问题**，与依赖、
abbs spec、ciel 代码均无关。ciel 侧的 mount/rollback/repo 刷新全部正常。

### 12.4 修复：`scripts/guest-patches/sitecustomize.py`（基础层补丁）

新增 `scripts/guest-patches/sitecustomize.py`——guest 内所有 Python 启动时
自动加载（`site` 模块机制，无需改 acbs），禁用 shutil 的两个零拷贝路径：

```python
shutil._USE_CP_COPY_FILE_RANGE = False
shutil._USE_CP_SENDFILE = False
```

`copyfile` 随之走纯 readinto/write 循环，对 EINVAL 免疫（400KB 文件性能无感）。

**安装（放基础层 dist，实例 rollback 会同步到它，不会丢）**：

```bash
mkdir -p ~/buildroots/.ciel/container/dist/usr/lib/python3.14/site-packages
cp guest-patches/sitecustomize.py \
   ~/buildroots/.ciel/container/dist/usr/lib/python3.14/site-packages/
```

注意：放在 `dist`（基础层）而不是实例——`package_build` 每包构建前会
rollback，实例里的补丁会被洗掉；`oma upgrade` 若升级 acbs 也**不会**动
sitecustomize（它不属于任何包），比直接改 fetch.py 持久。

**验证（顺带测 ciel 的 checkpoint 恢复功能）**：

```bash
~/ciel-rs/target/release/ciel-rs build -c ./STATES/app-utils_bzip3-1791299494.ciel-ckpt -i arm64
```

chan 已建好，checkpoint 会从 bzip3 继续。成功则 M3 验收全面关闭
（3 个包 / 至少 4 个 deb：pwgen、chan、chan-dbg、bzip3[+dbg]）。

### 12.5 后续

- 值得给 acbs 上游报 issue：`git_processor` 的 copytree 对 shutil.Error 无
  降级；以及 CPython mid-copy fast-path 失败在非桌面 Linux 上的普遍性。
- 若未来给别的 Python 工具排 proot 环境问题，"fast-copy 首调降级/中段炸"
  这条模式直接套用。

---

## 13. sitecustomize 补丁未生效的排查（2026-10-06 23:30）

checkpoint 恢复跑 bzip3 仍然报**一模一样**的 pack 文件 EINVAL。由于补丁生效后
copyfile 走纯 read/write 循环（普通文件不可能 EINVAL），结论：**补丁没有到达
guest**。最可能的两个坑：

1. **补丁放在实例里** —— `package_build` 每包构建前 rollback 会删除实例中
   dist 没有的文件（dircopy.rs Phase 1），补丁直接被洗掉；
2. **文件没有真正从 Windows 同步到手机**，或 dist 下的 python 版本目录写错。

### 13.1 已交付的两个机制

| 机制 | 作用 |
|---|---|
| `install_guest_patches`（harness `require_instance` 内） | 把 **ciel-proot.sh 同目录**的 `sitecustomize.py` 自动装进 dist + 实例的 `python3.*/site-packages`（glob 匹配版本目录，cmp 幂等）。交付链路的坑全部消除 |
| `copy-probe` 子命令 | 一次跑完取证：①sitecustomize/sys.modules/flags 状态；②对 pack 文件裸 read；③`copy_file_range` 与 `sendfile` 的 **CPython 精确调用序列回放**（打印每次调用的 offset/count/结果）；④`shutil.copy2` 复现 |

### 13.2 操作

```bash
# 手机上：把两个文件放在一起（harness 自动发现旁边的 sitecustomize.py）
scp scripts/ciel-proot.sh scripts/guest-patches/sitecustomize.py <phone>:~/

# 然后一条命令取证 + 自动装补丁
~/ciel-proot.sh -C ~/buildroots copy-probe arm64
# 看 #0：flags off 应为 True；#3/#4 应显示 "call#N offset>0 -> EINVAL"；
# #5 copy2 应 OK。随后重跑 checkpoint 恢复。
```

### 13.3 扩散面评估（用户第二问）：**是，广泛**

抽样 ABBS `stable` 分支 400 个 spec（总量 6056）：**43.2% 的包含 `git::` 源**
（tbl/https 363 个、pypi 21 个）。即 git fetcher 不修好，**约四成的包无法在本
机构建**。但两点让它属于"本机环境适配"而非"abbs 层面问题"：

- 触发条件是**这台设备的内核**对 `copy_file_range` 超出 EOF 请求返回 EINVAL
  （Linux 5.3+ 语义已放宽；AOSC 官方构建机的现代内核不会触发）——换到
  正常 Linux 主机上同一套 ciel/abbs 不会复现；
- 修复载体是环境级 sitecustomize（一处补丁覆盖全部 43%），不需要碰任何
  abbs spec。

所以优先级判断不变：修好 git fetcher（本 probe 确认后），M3 完整收口，然后
M4 promote 是主要矛盾。

---

## 14. 真正的根因（2026-10-07 12:50，SSH 直连诊断定案）：proot link2symlink × CPython copytree

> 第 12 节的"零拷贝 EINVAL"理论**被证伪**（sitecustomize 禁 fast-path 无效——
> fast-path 从来不是凶手）。以下是 SSH 逐层取证后的真根因。

### 14.1 排查链

1. copy-probe 全绿但构建依旧失败 → 1:1 复现脚本（同 harness、同 pack 文件、
   同 copytree 调用）**复现成功**，且逐 syscall 拆解显示该文件的
   stat/read/write/copy2 **全部单独成功** → 凶手在 copytree 的流程里，不在文件 IO。
2. 条目类型清单：pack 目录里出现大量 **`.l2s.tmp_*` 文件**；6 个真实 pack 文件
   `d_type=LNK` 但 **readlink 失败**、lstat 却显示真实文件大小。
3. 对照 CPython 3.14 `_copytree`：`srcentry.is_symlink()`（走 d_type）→ True →
   **第 571 行 `os.readlink(srcname)`** → EINVAL → 被 `except OSError` 收集成
   `"[Errno 22] Invalid argument: '<src>'"`——报错格式逐字节吻合；带日志的
   copy2 包装器确认 **copy2 从未被调用**（出错在 readlink，比拷贝更早）。

### 14.2 机制：proot link2symlink 的伪装不完整

- 宿主文件系统**不支持硬链接**（实测 `ln` → EACCES）→ link2symlink 必须开着。
- git 在 guest 里对 pack/idx/rev 调 `link()` → proot 把真数据挪进
  `.l2s.tmp_X.0001`，原名变成符号链接，并**伪造 lstat**（guest 看到普通文件）。
- 但 **getdents64 的 d_type 泄漏 DT_LNK**、**readlink 被刻意 EINVAL**（维持伪装）
  → 三条 syscall 对同一文件给出矛盾视图。CPython copytree 恰好组合了
  d_type + readlink，撞个正着。
- 手动 copy2 / probe 一直全绿的原因：`open()` 跟随符号链接读真数据，
  从不碰 readlink。

### 14.3 修复（`scripts/guest-patches/sitecustomize.py` 重写）

宿主无硬链接 → link2symlink 不能关 → 在 guest Python 层做 **l2s 感知的
copytree 替身**：`shutil.copytree` 被同名函数覆盖，语义与 CPython 3.14 一致，
唯一差异——`is_symlink()==True` 但 `readlink` 抛 OSError 时，降级为
"按普通文件跟随拷贝"（copy2 穿透链接拷真数据，产物反而是更干净的普通文件）。
其余路径（正常 symlink、目录递归、ignore、dangling 处理）逐条对齐原版。

fast-path 禁用保留（防御性，代价可忽略；原理论虽被证伪，防御无害）。

### 14.4 验证

- repro1（1:1 copytree 复现）：FAIL → **COPYTREE OK**
- 完整构建（checkpoint 恢复，Rust 侧二进制）：**BUILD SUCCESSFUL - 1 packages
  in 00:01:08**，`bzip3_1.5.4-0_arm64.deb` + `bzip3-dbg` 打包/安装/归档全通。
- **M3 验收全面关闭**：宿主 `OUTPUT-stable/debs/` 共 6 个 deb——
  pwgen(+dbg)、chan(+dbg)、bzip3(+dbg)；覆盖 tbl::、git:: 两种源类型、
  -dbg 符号包、多包循环、包间 rollback、acbs↔ciel repo 刷新握手。

### 14.5 模式沉淀

- **proot 环境排 Python 工具问题，先查 link2symlink 的"三视图矛盾"**：
  d_type=LNK + lstat=REG + readlink=EINVAL。任何"用 d_type 判类型再 readlink"
  的代码都会炸（copytree、find -type l 等）。
- 复用第 13 节结论：git:: 源占 ABBS 43.2%，此修复是 git 源包的**总闸**。
- 上游可报：proot（d_type 与 lstat/readlink 视图不一致）、acbs
  （copytree 错误无降级提示）。

---

## 14. 真正的根因（2026-10-07 12:50，SSH 直连诊断定案）：proot link2symlink × CPython copytree

> 第 12 节的"零拷贝 EINVAL"理论**被证伪**（sitecustomize 禁 fast-path 无效——
> fast-path 从来不是凶手）。以下是 SSH 逐层取证后的真根因。

### 14.1 排查链

1. copy-probe 全绿但构建依旧失败 → 1:1 复现脚本（同 harness、同 pack 文件、
   同 copytree 调用）**复现成功**，且逐 syscall 拆解显示该文件的
   stat/read/write/copy2 **全部单独成功** → 凶手在 copytree 的流程里，不在文件 IO。
2. 条目类型清单：pack 目录里出现大量 **`.l2s.tmp_*` 文件**；6 个真实 pack 文件
   `d_type=LNK` 但 **readlink 失败**、lstat 却显示真实文件大小。
3. 对照 CPython 3.14 `_copytree`：`srcentry.is_symlink()`（走 d_type）→ True →
   **第 571 行 `os.readlink(srcname)`** → EINVAL → 被 `except OSError` 收集成
   `"[Errno 22] Invalid argument: '<src>'"`——报错格式逐字节吻合；带日志的
   copy2 包装器确认 **copy2 从未被调用**（出错在 readlink，比拷贝更早）。

### 14.2 机制：proot link2symlink 的伪装不完整

- 宿主文件系统**不支持硬链接**（实测 `ln` → EACCES）→ link2symlink 必须开着。
- git 在 guest 里对 pack/idx/rev 调 `link()` → proot 把真数据挪进
  `.l2s.tmp_X.0001`，原名变成符号链接，并**伪造 lstat**（guest 看到普通文件）。
- 但 **getdents64 的 d_type 泄漏 DT_LNK**、**readlink 被刻意 EINVAL**（维持伪装）
  → 三条 syscall 对同一文件给出矛盾视图。CPython copytree 恰好组合了
  d_type + readlink，撞个正着。
- 手动 copy2 / probe 一直全绿的原因：`open()` 跟随符号链接读真数据，
  从不碰 readlink。

### 14.3 修复（`scripts/guest-patches/sitecustomize.py` 重写）

宿主无硬链接 → link2symlink 不能关 → 在 guest Python 层做 **l2s 感知的
copytree 替身**：`shutil.copytree` 被同名函数覆盖，语义与 CPython 3.14 一致，
唯一差异——`is_symlink()==True` 但 `readlink` 抛 OSError 时，降级为
"按普通文件跟随拷贝"（copy2 穿透链接拷真数据，产物反而是更干净的普通文件）。
其余路径（正常 symlink、目录递归、ignore、dangling 处理）逐条对齐原版。

fast-path 禁用保留（防御性，代价可忽略；原理论虽被证伪，防御无害）。

### 14.4 验证

- repro1（1:1 copytree 复现）：FAIL → **COPYTREE OK**
- 完整构建（checkpoint 恢复，Rust 侧二进制）：**BUILD SUCCESSFUL - 1 packages
  in 00:01:08**，`bzip3_1.5.4-0_arm64.deb` + `bzip3-dbg` 打包/安装/归档全通。
- **M3 验收全面关闭**：宿主 `OUTPUT-stable/debs/` 共 6 个 deb——
  pwgen(+dbg)、chan(+dbg)、bzip3(+dbg)；覆盖 tbl::、git:: 两种源类型、
  -dbg 符号包、多包循环、包间 rollback、acbs↔ciel repo 刷新握手。

### 14.5 模式沉淀

- **proot 环境排 Python 工具问题，先查 link2symlink 的"三视图矛盾"**：
  d_type=LNK + lstat=REG + readlink=EINVAL。任何"用 d_type 判类型再 readlink"
  的代码都会炸（copytree、find -type l 等）。
- 复用第 13 节结论：git:: 源占 ABBS 43.2%，此修复是 git 源包的**总闸**。
- 上游可报：proot（d_type 与 lstat/readlink 视图不一致）、acbs
  （copytree 错误无降级提示）。

---

## 15. 补丁落地 Rust + 多包批量测试（2026-10-07 13:20）

### 15.1 Rust 侧改动（已在手机编译通过，本地仓库已同步）

| 文件 | 内容 |
|---|---|
| `src/guestpatch.rs`（新） | `include_str!("../scripts/guest-patches/sitecustomize.py")` 内嵌补丁；`ensure_installed(dist, root)` 幂等安装（内容一致则跳过）；best-effort，失败只 warn 不中断 |
| `src/dircopy.rs` | `DirCopyManager::rollback()` 尾部挂 `guestpatch::ensure_installed(&self.dist, &self.root)`——每次回滚同步后自动就位 |
| `src/main.rs` | `mod guestpatch;` |
| 坑 1 | `include_str!` 路径相对**文件所在目录**：`src/` 下要用 `../scripts/`（编译器报错后修正） |
| 坑 2 | `info!` 宏展开引用 `style()`，调用点必须 `use console::style;`（宏卫生机制） |

幂等性验证：第二次构建不再出现 "Installed guest Python patch" 日志（内容一致跳过）✓

### 15.2 批量测试中撞出的第二个环境缺口：`/dev/fd` 缺失（已修）

**首个 Python 包（sortedcontainers, ABTYPE=python）触发**：autobuild4
`proc/53-spiral.sh:33-48` 用 bash 进程替换 `done < <(find ...)`，打开
`/dev/fd/63` → `No such file or directory`。

- 基础层 payload 的 `dev/` **只有 `shm/`**——`fd`、`stdin`、`stdout`、`stderr`
  四个标准符号链接本该由运行时创建，无特权 seed 不会带。
- 修复：`src/dircopy.rs` 新增 `prepare_dev(root)`（与 `prepare_dns` 同风格、
  同两个调用点）：缺则补建 `fd→/proc/self/fd`、`stdin→/proc/self/fd/0` 等。
  宿主侧符号链接经绑定的 `/proc` 解析到**打开者自己的**描述符表，语义正确；
  无需 proot 参数。

### 15.3 多包测试结果：7 包 13 deb 全绿

| 包 | ABTYPE | 源 | 结果 |
|---|---|---|---|
| pwgen | default(autotools) | tbl:: | ✅ 2.08-0(+dbg) |
| chan | default | tbl:: | ✅ 0.0.4-0(+dbg) |
| bzip3 | default | **git::** | ✅ 1.5.4-0(+dbg) |
| xxhash | default | **git::** | ✅ 0.8.3-0(+dbg)，**12.7s** |
| libunibreak | default | **git::**(salsa) | ✅ 6.1-0(+dbg) |
| sortedcontainers | **python** | **pypi::** | ✅ 2.4.0-0 noarch |
| hexedit | default | git:: + **ncurses 链接** | ✅ 1.6-0(+dbg)，30.8s |

覆盖面：tbl/git/pypi 三种 fetcher、autotools/python 两种 ABTYPE、
BUILDDEP 安装路径（setuptools-python3）、C 库链接、noarch 包、
多包循环 + 包间 rollback + repo 刷新握手 ×13。

### 15.4 结论

`ciel build` 在 proot/Android 上的已知环境缺口**全部关闭**：
PTY（ptmx/pts bind）、link2symlink×copytree（sitecustomize）、
argv 传输（harness）、umask、/dev/fd。git 源（ABBS 43.2%）与 Python 包
路径均已打通。下一步 = M4（promote 落地 Rust）。

---

## 15. 补丁落地 Rust + 多包批量测试（2026-10-07 13:20）

### 15.1 Rust 侧改动（已在手机编译通过，本地仓库已同步）

| 文件 | 内容 |
|---|---|
| `src/guestpatch.rs`（新） | `include_str!("../scripts/guest-patches/sitecustomize.py")` 内嵌补丁；`ensure_installed(dist, root)` 幂等安装（内容一致则跳过）；best-effort，失败只 warn 不中断 |
| `src/dircopy.rs` | `DirCopyManager::rollback()` 尾部挂 `guestpatch::ensure_installed(&self.dist, &self.root)`——每次回滚同步后自动就位 |
| `src/main.rs` | `mod guestpatch;` |
| 坑 1 | `include_str!` 路径相对**文件所在目录**：`src/` 下要用 `../scripts/`（编译器报错后修正） |
| 坑 2 | `info!` 宏展开引用 `style()`，调用点必须 `use console::style;`（宏卫生机制） |

幂等性验证：第二次构建不再出现 "Installed guest Python patch" 日志（内容一致跳过）✓

### 15.2 批量测试中撞出的第二个环境缺口：`/dev/fd` 缺失（已修）

**首个 Python 包（sortedcontainers, ABTYPE=python）触发**：autobuild4
`proc/53-spiral.sh:33-48` 用 bash 进程替换 `done < <(find ...)`，打开
`/dev/fd/63` → `No such file or directory`。

- 基础层 payload 的 `dev/` **只有 `shm/`**——`fd`、`stdin`、`stdout`、`stderr`
  四个标准符号链接本该由运行时创建，无特权 seed 不会带。
- 修复：`src/dircopy.rs` 新增 `prepare_dev(root)`（与 `prepare_dns` 同风格、
  同两个调用点）：缺则补建 `fd→/proc/self/fd`、`stdin→/proc/self/fd/0` 等。
  宿主侧符号链接经绑定的 `/proc` 解析到**打开者自己的**描述符表，语义正确；
  无需 proot 参数。

### 15.3 多包测试结果：7 包 13 deb 全绿

| 包 | ABTYPE | 源 | 结果 |
|---|---|---|---|
| pwgen | default(autotools) | tbl:: | ✅ 2.08-0(+dbg) |
| chan | default | tbl:: | ✅ 0.0.4-0(+dbg) |
| bzip3 | default | **git::** | ✅ 1.5.4-0(+dbg) |
| xxhash | default | **git::** | ✅ 0.8.3-0(+dbg)，**12.7s** |
| libunibreak | default | **git::**(salsa) | ✅ 6.1-0(+dbg) |
| sortedcontainers | **python** | **pypi::** | ✅ 2.4.0-0 noarch |
| hexedit | default | git:: + **ncurses 链接** | ✅ 1.6-0(+dbg)，30.8s |

覆盖面：tbl/git/pypi 三种 fetcher、autotools/python 两种 ABTYPE、
BUILDDEP 安装路径（setuptools-python3）、C 库链接、noarch 包、
多包循环 + 包间 rollback + repo 刷新握手 ×13。

### 15.4 结论

`ciel build` 在 proot/Android 上的已知环境缺口**全部关闭**：
PTY（ptmx/pts bind）、link2symlink×copytree（sitecustomize）、
argv 传输（harness）、umask、/dev/fd。git 源（ABBS 43.2%）与 Python 包
路径均已打通。下一步 = M4（promote 落地 Rust）。
