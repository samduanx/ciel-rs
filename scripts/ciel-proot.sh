#!/usr/bin/env bash
# ciel-proot.sh — 在 Android/Termux 上直接驱动 ciel 的 proot 后端
#
# 存在的唯一理由：把 src/backend/proot.rs:81-163 里那段 argv 构造原样搬到
# shell 里，让你能手动进去做实验，而不用每次都重新编译 ciel。
# 它复刻的参数表必须与 proot.rs 保持一致；改了那边记得同步改这里。
#
# 它**不需要** ciel 二进制本身，只需要：
#   1) Termux 里的 proot         (pkg install proot)
#   2) 一个已经初始化好的 workspace（含 .ciel/ 与 .ciel/container/dist）
#
# 用法：
#   ./ciel-proot.sh -C ~/buildroots doctor          # 环境体检 + 打印实际 argv
#   ./ciel-proot.sh -C ~/buildroots shell arm64     # 交互式 guest shell
#   ./ciel-proot.sh -C ~/buildroots run arm64 /bin/bash -ec 'dpkg --audit'
#   ./ciel-proot.sh -C ~/buildroots syscall-probe arm64  # ★ errno: host vs proot
#   ./ciel-proot.sh -C ~/buildroots pty-probe arm64# ★ PTY 可用性（acbs/pexpect）
#   ./ciel-proot.sh -C ~/buildroots copy-probe arm64# ★ shutil 零拷贝 × 内核 EINVAL 取证
#   ./ciel-proot.sh -C ~/buildroots dpkg-probe arm64     # ★ aosc-aaa 对照矩阵
#   ./ciel-proot.sh -C ~/buildroots update arm64         # 只在实例里更新
#   ./ciel-proot.sh -C ~/buildroots update-base arm64# 更新+验证+提升基础层
#   ./ciel-proot.sh -C ~/buildroots promote arm64        # 只做提升
#
# 全局选项（必须出现在子命令之前）：
#   -C, --workspace <dir>   workspace 根目录（含 .ciel/ 的那层）
#   -i, --instance <name>   默认 arm64
#       --no-pseudo         不 bind /proc /sys 和 /dev 节点（对照实验用）
#       --no-link2symlink   不加 --link2symlink
#       --no-change-id      不加 --change-id=0:0
#   -v, --verbose           打印实际执行的 argv
#   -h, --help

set -uo pipefail

# ★ umask 022 —— 与 ciel 的 main.rs 保持一致（原话："set umask to 022 to
# ensure correct permissions on rootfs"）。
#
# 2026-10-06 阶段 D 真机教训：harness 此前不设 umask，直接继承 Termux
# 交互 shell 的 umask（0077）。autobuild4 打包时的
#   mkdir -p "$PKGDIR"/DEBIAN     （pm/dpkg.sh:182，无 -m）
# 新目录模式 = 0777 & ~umask = 700，dpkg-deb 拒绝打包：
#   "control directory has bad permissions 700 (must be >=0755 and <=0775)"
# 之前经 harness 跑的 oma upgrade / dpkg-probe 都没踩到，是因为 dpkg 按
# 包内元数据**显式**设模式、对 umask 免疫；构建树（mkdir/install 创建
# 源码目录、abdist、DEBIAN）是第一个 umask 敏感的环节。
# 注意：`ciel build`（Rust 侧）不受影响——main.rs 本来就设 022；
# 这里补齐是为了让 harness 与 ciel 行为一致（毕竟它的存在意义就是
# "argv 构造原样搬到 shell 里"）。
umask 022

# ---------------------------------------------------------------------------
# 与 proot.rs 保持一致的常量
# ---------------------------------------------------------------------------
GUEST_PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

WORKSPACE=""
INSTANCE="arm64"
USE_PSEUDO=1
USE_LINK2SYMLINK=1
USE_CHANGE_ID=1
VERBOSE=0

# ---------------------------------------------------------------------------
# 输出helper
# ---------------------------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_R=$'\033[31m'; C_G=$'\033[32m'; C_Y=$'\033[33m'
    C_B=$'\033[34m'; C_0=$'\033[0m'
else
    C_R=""; C_G=""; C_Y=""; C_B=""; C_0=""
fi
say()  { printf '%s==>%s %s\n' "$C_B" "$C_0" "$*" >&2; }
ok()   { printf '%s  ok%s %s\n' "$C_G" "$C_0" "$*" >&2; }
warn() { printf '%swarn%s %s\n' "$C_Y" "$C_0" "$*" >&2; }
die()  { printf '%sfail%s %s\n' "$C_R" "$C_0" "$*" >&2; exit 1; }
head1() { printf '\n%s=== %s ===%s\n' "$C_B" "$*" "$C_0" >&2; }

usage() { sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }

# ---------------------------------------------------------------------------
# 参数解析
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        -C|--workspace) WORKSPACE="${2:?--workspace 需要参数}"; shift 2 ;;
        -i|--instance) INSTANCE="${2:?--instance 需要参数}"; shift 2 ;;
        --no-pseudo)       USE_PSEUDO=0; shift ;;
        --no-link2symlink) USE_LINK2SYMLINK=0; shift ;;
        --no-change-id)    USE_CHANGE_ID=0; shift ;;
        -v|--verbose)      VERBOSE=1; shift ;;
        -h|--help)         usage ;;
        --)                shift; break ;;
        -*)                die "未知选项 $1（-h 看帮助）" ;;
        *)                 break ;;
    esac
done
CMD="${1:-help}"; shift || true

# 本脚本所在目录（guest Python 兼容补丁的来源）
SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || SCRIPT_DIR=""

# ---------------------------------------------------------------------------
# 定位 workspace
# ---------------------------------------------------------------------------
find_workspace_upwards() {
    local d="$1"
    while [ -n "$d" ] && [ "$d" != "/" ]; do
        if [ -d "$d/.ciel" ]; then printf '%s' "$d"; return 0; fi
        d="$(dirname "$d")"
    done
    return 1
}

if [ -n "$WORKSPACE" ]; then
    [ -d "$WORKSPACE" ] || die "workspace 不存在：$WORKSPACE"
    WS="$(cd "$WORKSPACE" && pwd)" || die "无法进入 $WORKSPACE"
    [ -d "$WS/.ciel" ] || die "$WS 下没有 .ciel/ —— 这不是一个 ciel workspace"
elif [ -d "./.ciel" ]; then
    WS="$(pwd)"
elif WS="$(find_workspace_upwards "$PWD")"; then
    : # 从当前目录向上找到的
else
    cat >&2 <<EOF
${C_R}fail${C_0} 找不到 ciel workspace（任何层级都没有 .ciel/）

  用 -C 显式指定：
      $0 -C /path/to/workspace ${CMD}
  例如：
      $0 -C ~/buildroots doctor
EOF
    exit 1
fi
cd "$WS" || die "无法进入 workspace $WS"

CIEL_DIR="$WS/.ciel"
DIST="$CIEL_DIR/container/dist"
INST_DIR="$CIEL_DIR/container/instances/$INSTANCE"
ROOT="$INST_DIR/root"
CONFIG="$CIEL_DIR/data/config.toml"

# ---------------------------------------------------------------------------
# config.toml
# ---------------------------------------------------------------------------
# 键名必须与 src/config.rs 里的 serde 字段名逐字一致：
#   local_sources（无 rename）、sep_mount（rename 成 branch-exclusive-output）
# 写成 local-sources 会静默取不到值而退回默认 —— 那是最危险的情况。
cfg_bool() {
    local key="$1" def="$2" val
    val="$(grep -E "^[[:space:]]*${key}[[:space:]]*=" "$CONFIG" 2>/dev/null \
           | head -1 | cut -d= -f2- | tr -d ' \r"')"
    if [ -n "$val" ]; then printf '%s' "$val"; else printf '%s' "$def"; fi
}

if [ -f "$CONFIG" ]; then
    LOCAL_SOURCES="$(cfg_bool local_sources true)"
    SEP_MOUNT="$(cfg_bool branch-exclusive-output true)"
    CONFIG_STATE="已读取"
else
    LOCAL_SOURCES="true"; SEP_MOUNT="true"
    CONFIG_STATE="不存在，使用 Rust 侧默认值"
fi

# branch-exclusive-output = true 时 debs 落在 OUTPUT-<branch>/
if [ "$SEP_MOUNT" = "true" ]; then
    BRANCH="$(git -C TREE rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
    OUT_DIR="OUTPUT-$BRANCH"
else
    OUT_DIR="OUTPUT"
fi

# ---------------------------------------------------------------------------
# 构造 argv —— 这里是 proot.rs 的镜像，改动请同步
# ---------------------------------------------------------------------------
build_argv() {
    local root="$1"; shift
    local -a argv=()

    argv+=("--kill-on-exit")
    [ "$USE_LINK2SYMLINK" -eq 1 ] && argv+=("--link2symlink")
    [ "$USE_CHANGE_ID" -eq 1 ] && argv+=("--change-id=0:0")
    argv+=("--rootfs=$root")
    argv+=("-w" "/root")

    # proot.rs（2026-10-06 修复后）—— /proc /sys 整体 bind；/dev 只 bind
    # 单个设备节点。绝不整体 bind /dev：aosc-aaa 的 payload 带 ./dev/shm，
    # dpkg 必须在 guest 的 /dev 下 mkdir 它；若整体 bind，guest /dev 就是
    # Android 真实的 /dev（root 属主的 tmpfs，app 进程不可写），
    # mkdir 直接 EACCES —— 这就是 "error creating directory './dev/shm'"
    # 的来源，也是 oma upgrade 反复失败的根因（真机 dpkg-probe 已证实）。
    if [ "$USE_PSEUDO" -eq 1 ]; then
        for p in /proc /sys; do
            argv+=("-b" "$p")
        done
        for node in /dev/null /dev/zero /dev/full /dev/random /dev/urandom /dev/tty; do
            [ -e "$node" ] || continue
            argv+=("-b" "$node")
        done
        # PTY（2026-10-06 真机定案，docs/M3-TEST-PLAN.md 第 7-9 节）：
        # acbs-build 用 pexpect.spawn('autobuild') 起构建，经
        # pty.fork() -> openpty() 需要 /dev/ptmx + 可解析的 /dev/pts/N。
        # AOSC rootfs 两者都没有，缺 bind 时每个构建都死于
        # `OSError: out of pty devices`。两者成对 bind 自宿主
        # （Termux devpts 完整可用，forkpty+exec+读回已端到端验证）；
        # 只 bind ptmx 会让 posix_openpt 成功但 slave 打不开，
        # 所以两个都在才 bind。与 proot.rs 的设备节点列表保持同步。
        if [ -e /dev/ptmx ] && [ -d /dev/pts ]; then
            argv+=("-b" "/dev/ptmx")
            argv+=("-b" "/dev/pts")
        fi
    fi

    # proot.rs:104-117 —— DEFAULT_MOUNTS，对应 actions/mod.rs:15-20
    local abs
    abs="$(realpath -m "$OUT_DIR/debs" 2>/dev/null || echo "$WS/$OUT_DIR/debs")"
    argv+=("-b" "$abs:/debs/")
    abs="$(realpath -m "TREE" 2>/dev/null || echo "$WS/TREE")"
    argv+=("-b" "$abs:/tree")
    if [ "$LOCAL_SOURCES" = "true" ]; then
        abs="$(realpath -m "SRCS" 2>/dev/null || echo "$WS/SRCS")"
        argv+=("-b" "$abs:/var/cache/acbs/tarballs")
    fi
    abs="$(realpath -m "CACHE" 2>/dev/null || echo "$WS/CACHE")"
    argv+=("-b" "$abs:/var/cache/apt/archives")

    # proot.rs:121-127 —— nspawn 会自动复制 resolv.conf，proot 不会
    local resolv=""
    if [ -f /etc/resolv.conf ]; then
        resolv="/etc/resolv.conf"
    elif [ -n "${PREFIX:-}" ] && [ -f "$PREFIX/etc/resolv.conf" ]; then
        resolv="$PREFIX/etc/resolv.conf"
    fi
    [ -n "$resolv" ] && argv+=("-b" "$resolv:/etc/resolv.conf")

    # proot.rs:164 —— CIEL_PROOT_EXTRA_OPTIONS，与 Rust 侧语义一致。
    #
    # ★ 2026-10-06 真机教训：这个 harness 此前**根本没读这个环境变量**
    # （只有 Rust 侧读），但 pty-probe 的建议文本让用户 export 它 ——
    # 于是修法A（bind /dev/ptmx）和修法B（stub pexpect）的两次实测里
    # 环境变量都被静默丢弃，guest 输出与基线完全相同，看起来像
    # 「修法无效」。实际是选项从未传到 proot。
    #
    # 另外：proot 不是 shell，不展开 ~；而 bash 在双引号里也不展开 ~
    # （`export X="-b ~/foo"` 里 X 的值是字面 `~/foo`）。所以这里对
    # 每个以 `~/` 开头的 token 自行展开为 $HOME/ —— Rust 侧 proot.rs
    # 已做同样的展开，两边保持一致。
    if [ -n "${CIEL_PROOT_EXTRA_OPTIONS:-}" ]; then
        local tok
        for tok in $CIEL_PROOT_EXTRA_OPTIONS; do
            case "$tok" in
                '~'/*) tok="$HOME/${tok#'~'/}" ;;
            esac
            argv+=("$tok")
        done
    fi

    # 注意：这里绝对不能 push "--"。proot 用 src/cli/cli.c 的手写扫描器，
    # 不是 getopt；"--" 以 '-' 开头会被送进选项表查不到，直接
    # `unknown option '--'.` 退出。命令首 token 一定是绝对路径，
    # 扫描器在那里自然终止。
    [ $# -gt 0 ] && argv+=("$@")
    # ★ 必须用 NUL 分隔，不能用换行。
    #
    # 换行分隔（老版本用 printf '%s\n' + mapfile -t）会在参数本身含换行时
    # 把一个 argv 元素拆成多个。例如把一段多行 Python 通过
    # `/bin/bash -c '...'` 传给 guest 时，guest 的 bash -c 只收到第一行，
    # 报 `unexpected EOF while looking for matching '"'`。
    #
    # 这不只是测试时的不便：任何含换行的 guest 命令都会被**静默**打散，
    # 且症状（语法错误）指向错误的行，看起来像用户引号写错了。
    # Rust 侧 proot.rs 直接把 OsString 交给 Command::args()，无此中间层，
    # 所以这个 bug 只存在于本 harness。
    printf '%s\0' "${argv[@]}"
    return 0
}

# proot 在 bind 源不存在时会失败，而 Rust 侧 ensure_host_sanity()
# 会在跑之前把这些目录建出来。这里保持一致。
ensure_bind_dirs() {
    mkdir -p "$OUT_DIR/debs" TREE CACHE 2>/dev/null || true
    if [ "$LOCAL_SOURCES" = "true" ]; then mkdir -p SRCS 2>/dev/null || true; fi
}

# 真正执行。proot 本体必须用绝对路径——我们把 PATH 换成了 guest 的。
PROOT_BIN=""
run_guest() {
    local root="$1"; shift
    if [ -z "$PROOT_BIN" ]; then
        PROOT_BIN="$(command -v proot || true)"
        [ -n "$PROOT_BIN" ] || die "找不到 proot。Termux 里执行：pkg install proot"
    fi
    local -a argv=()
    # -d '' = NUL 分隔，与 build_argv 的 printf '%s\0' 配对。
    # 换行分隔会在参数含换行时把一个 argv 元素拆成多个（详见 build_argv 注释）。
    mapfile -d '' -t argv < <(build_argv "$root" "$@")

    if [ "$VERBOSE" -eq 1 ]; then
        printf '%sargv:%s' "$C_Y" "$C_0" >&2
        printf ' %q' "$PROOT_BIN" "${argv[@]}" >&2
        printf '\n' >&2
    fi

    local -a guest_env=(
        "HOME=/root"
        "PATH=$GUEST_PATH"
        "TERM=${TERM:-xterm-256color}"
    )
    [ -n "${CIEL_STAGE2:-}" ] && guest_env+=("ABSTAGE2=1")
    [ -n "${CIEL_PROOT_NO_SECCOMP:-}" ] && guest_env+=("PROOT_NO_SECCOMP=1")

    # env -i 对应 proot.rs:47-63 的 env_clear()：guest 是 glibc rootfs，
    # Termux 的 loader/library 环境漏进去会让 guest 二进制崩掉。
    # argv[0]（proot 自己）不在这份 env 里，所以要用绝对路径。
    env -i "${guest_env[@]}" "$PROOT_BIN" "${argv[@]}"
}

# ---------------------------------------------------------------------------
# guest Python 兼容补丁（scripts/guest-patches/sitecustomize.py）
# ---------------------------------------------------------------------------
# 2026-10-06 bzip3 教训：sitecustomize 补丁放在实例里会被 package_build 每包
# 构建前的 rollback 洗掉；手工放 dist 又容易漏（python 版本目录要猜）。
# 这里自动安装：把与 ciel-proot.sh 同目录的 sitecustomize.py 装进 dist（基础层）
# 与实例的 site-packages（glob python3.* 目录），cmp 幂等、跑多少遍无副作用。
# 补丁内容与依据见该文件头注释 + docs/M3-TEST-PLAN.md 第 12 节。
install_guest_patches() {
    local src="$SCRIPT_DIR/sitecustomize.py"
    [ -f "$src" ] || return 0
    local sp_dir
    for sp_dir in "$DIST"/usr/lib/python3.*/site-packages \
                  "$ROOT"/usr/lib/python3.*/site-packages; do
        [ -d "$sp_dir" ] || continue
        if ! cmp -s "$src" "$sp_dir/sitecustomize.py" 2>/dev/null; then
            if cp "$src" "$sp_dir/sitecustomize.py" 2>/dev/null; then
                ok "已安装 guest 补丁: ${sp_dir#$WS/}/sitecustomize.py"
            else
                warn "无法写入 $sp_dir/sitecustomize.py"
            fi
        fi
    done
}

# 取第一个位置参数当实例名（可选）
set_instance() {
    if [ $# -ge 1 ] && [ -n "${1:-}" ]; then
        INSTANCE="$1"
        INST_DIR="$CIEL_DIR/container/instances/$INSTANCE"
        ROOT="$INST_DIR/root"
    fi
}

require_instance() {
    [ -d "$ROOT/usr" ] || die \
"实例 $INSTANCE 未初始化（$ROOT 下没有 usr/）。

  先用 ciel 建好这个实例：
      cd $WS
      <ciel> add -i $INSTANCE
  或换一个已经存在的实例名：
      $0 -C $WS doctor          # 看 instances/ 下有哪些"
    ensure_bind_dirs
    # 在这里检查 proot（而不是让 run_guest 去 die）：所有会跑 guest 命令的
    # 子命令都经过 require_instance，一次检查覆盖全部。
    # 而 run_guest 里的 die 会连同调用点的输出重定向一起被丢弃 ——
    # 在 Windows 上试跑实测为「EXIT=1 且 stdout/stderr 全空」，
    # 报错完全不可见。提前 die 则保证消息落在终端上。
    command -v proot >/dev/null 2>&1 || die \
"找不到 proot。Termux 里执行：
      pkg install proot"
    install_guest_patches
}

# ---------------------------------------------------------------------------
# 子命令
# ---------------------------------------------------------------------------

cmd_doctor() {
    head1 "workspace"
    printf '  WS            %s\n' "$WS"
    printf '  DIST          %s%s%s\n' "$C_B" "$DIST" "$C_0"
    printf '  instance      %s\n' "$INSTANCE"
    printf '  ROOT          %s\n' "$ROOT"
    printf '  config        %s (%s)\n' "$CONFIG" "$CONFIG_STATE"
    printf '    local_sources           = %s\n' "$LOCAL_SOURCES"
    printf '    branch-exclusive-output = %s\n' "$SEP_MOUNT"
    printf '  output dir    %s/debs\n' "$OUT_DIR"
    if [ -d TREE ]; then ok "TREE/ 存在"; else warn "TREE/ 不存在（ciel build 会去 clone）"; fi

    head1 "proot"
    printf '  binary        %s\n' "$(command -v proot || echo '<未安装：pkg install proot>')"
    printf '  flags         pseudo=%s link2symlink=%s change-id=%s\n' \
        "$USE_PSEUDO" "$USE_LINK2SYMLINK" "$USE_CHANGE_ID"
    printf '  seccomp       PROOT_NO_SECCOMP=%s\n' "${CIEL_PROOT_NO_SECCOMP:-<unset>}"
    # 过渡期提示（2026-10-06）：/dev/ptmx + /dev/pts 已固化进默认 argv，
    # 环境变量再传一遍就是重复 bind。无害但属于噪音，提醒 unset。
    case "${CIEL_PROOT_EXTRA_OPTIONS:-}" in
        *ptmx*|*'/dev/pts'*|*pexpect-stub*)
            warn "CIEL_PROOT_EXTRA_OPTIONS 里的 bind 已固化进默认 argv（见下方 argv 的重复项）："
            warn "  建议执行  unset CIEL_PROOT_EXTRA_OPTIONS  消除重复；"
            warn "  保留该变量也无害（同一 bind 声明两次，proot 翻译结果相同）。"
            ;;
    esac

    head1 "instances"
    local found=0 p n
    for p in "$CIEL_DIR/container/instances"/*/; do
        [ -d "$p" ] || continue
        n="$(basename "$p")"
        found=1
        if [ -f "$p/manifest/seeded" ] && [ -d "$p/root/usr" ]; then
            printf '  %-16s 已就绪\n' "$n"
        elif [ -d "$p/root" ]; then
            printf '  %-16s 半播种（缺 manifest/seeded 或 root/usr）\n' "$n"
        else
            printf '  %-16s 空\n' "$n"
        fi
    done
    [ "$found" -eq 0 ] && warn "instances/ 下没有任何实例"

    head1 "base layer"
    if [ -d "$DIST/usr" ]; then ok "dist/usr 存在"; else warn "dist/usr 不存在——先跑 ciel load-os"; fi
    if [ -f "$INST_DIR/manifest/seeded" ]; then
        ok "实例 $INSTANCE 已播种"
    else
        warn "实例 $INSTANCE 未播种——先跑 ciel add $INSTANCE"
    fi
    local old
    for old in "$DIST".pristine-*; do
        [ -d "$old" ] && warn "存在旧基础层备份，确认无用后可删：$old"
    done

    head1 "root/ 顶层检查（SKIP_PATHS 的真实语义）"
    # 注意：dircopy.rs 的 is_skip() 只用于 rollback，seed 不过滤 ——
    # dist/dev dist/proc dist/sys dist/run（目录本体，设备节点在解包时
    # 已跳过）会被完整播种进 root/。所以这四个「存在」是【正常】的。
    #   * tree/debs 是 workspace bind 目标，不应存在于磁盘 —— 存在即异常。
    #   * /proc /sys 在运行时被整体 bind 遮蔽（磁盘副本无害）。
    #   * /dev 是 rootfs 自己的目录（dpkg 可写、可 mkdir ./dev/shm），
    #     设备节点靠上面的 -b 逐个注入。
    if [ -d "$ROOT" ]; then
        local d
        for d in tree debs; do
            if [ -e "$ROOT/$d" ]; then
                printf '  root/%-6s 存在 —— 异常！workspace bind 目标不该在磁盘上\n' "$d"
            else
                printf '  root/%-6s 不存在（正常，运行时靠 -b 提供）\n' "$d"
            fi
        done
        for d in proc sys dev run; do
            if [ -e "$ROOT/$d" ]; then
                printf '  root/%-6s 存在（正常，seed 自 dist；proc/sys 运行时被遮蔽）\n' "$d"
            else
                printf '  root/%-6s 不存在 —— 可疑！dist 里应有此目录，检查 load-os\n' "$d"
            fi
        done
    fi

    head1 "将要执行的 argv"
    # build_argv 现在输出 NUL 分隔（为了安全传递含换行的参数），
    # 直接打印会是一坨带^@ 的东西。转成换行显示，可读性等价于旧的
    # 换行分隔版本 —— 显示用的分隔符和传输用的分隔符刻意不同。
    build_argv "$ROOT" /bin/bash -c 'echo hello from guest' \
        | tr '\0' '\n'
}

cmd_shell_i() {
    set_instance "${1:-}"
    require_instance
    head1 "进入实例 $INSTANCE（等同 ciel shell -i $INSTANCE）"
    # 不带命令 -> proot 回退到 /bin/sh（argv[argc_offset] 为 NULL）
    run_guest "$ROOT"
}

cmd_run() {
    [ $# -ge 1 ] || die "run 需要实例名：$0 run <inst> <cmd> [args...]"
    set_instance "$1"; shift
    [ $# -gt 0 ] || die "run 需要一条命令"
    require_instance
    case "$1" in -*) die "命令不能以 '-' 开头：proot 没有 -- 分隔符，会把它当自己的选项" ;; esac
    run_guest "$ROOT" "$@"
}

# ---------------------------------------------------------------------------
# ★ syscall-probe：决定性实验
# ---------------------------------------------------------------------------
# 目的：判断 dpkg 报的 EISDIR(21)/EEXIST(17) 是不是 proot 翻译语义的产物。
# 方法：同一段 python 在宿主 (Termux) 和 proot 里各跑一遍，diff 输出。
# 如果 errno 不一致，那 dpkg 的 EDSP::Resolve 失败就是 proot 造成的，
# 与 dpkg 自身、与 aosc-aaa 的 payload 都无关。
read -r -d '' PROBE_SRC <<'PYEOF'
import os, sys, errno, stat

def show(label, fn):
    try:
        r = fn()
        print("%-40s OK   %r" % (label, r))
    except OSError as e:
        print("%-40s ERR  %d (%s) %s" % (
            label, e.errno, errno.errorcode.get(e.errno, "?"), e.strerror))
    except Exception as e:
        print("%-40s EXC  %r" % (label, e))

OPATH = getattr(os, "O_PATH", 0)
REMOVDIR = getattr(os, "AT_REMOVEDIR", 0)
R = "/"

print("== identity ==")
print("euid=%d egid=%d" % (os.geteuid(), os.getegid()))

print("== the stat family ==")
show("stat(R)",          lambda: os.stat(R))
show("lstat(R)",         lambda: os.lstat(R))
show("stat(R) is dir",   lambda: stat.S_ISDIR(os.stat(R).st_mode))
show("scandir(R) count", lambda: len(list(os.scandir(R))))

print("== open with O_PATH|O_NOFOLLOW (dpkg EDSP style) ==")
show("open(R, O_PATH|O_NOFOLLOW)",           lambda: os.open(R, OPATH | os.O_NOFOLLOW))
show("open(R, O_PATH|O_NOFOLLOW|O_DIRECTORY)",
     lambda: os.open(R, OPATH | os.O_NOFOLLOW | os.O_DIRECTORY))
show("open(R, O_RDONLY|O_DIRECTORY)",        lambda: os.open(R, os.O_RDONLY | os.O_DIRECTORY))
show("open(R, O_RDONLY|O_CREAT|O_EXCL)",     lambda: os.open(R, os.O_RDONLY | os.O_CREAT | os.O_EXCL, 0o644))

print("== mkdirat family: the EEXIST that breaks dpkg ==")
# fd may be unavailable (Android host: open(/, O_DIRECTORY) = EACCES by
# SELinux) — skip the dirfd-dependent probes rather than crash, so the
# host run always completes and the diff stays meaningful.
try:
    fd = os.open(R, os.O_RDONLY | os.O_DIRECTORY)
except OSError as e:
    fd = None
    print("(dirfd unavailable: %d (%s) — skipping mkdirat/unlinkat probes)" % (
        e.errno, errno.errorcode.get(e.errno, "?")))
if fd is not None:
    show("mkdirat(fd, R)",     lambda: os.mkdir(R, 0o755, dir_fd=fd))
    show("mkdirat(fd, dot)",   lambda: os.mkdir(".", 0o755, dir_fd=fd))
    show("mkdirat(fd, empty)", lambda: os.mkdir("", 0o755, dir_fd=fd))

    print("== unlink family: the EISDIR that breaks dpkg ==")
    show("unlinkat(fd, R, 0)",           lambda: os.unlink(R, dir_fd=fd))
    show("unlinkat(fd, R, AT_REMOVEDIR)", lambda: os.unlink(R, dir_fd=fd, flags=REMOVDIR))

print("== chmod/chown: proot --change-id fakes uid 0 ==")
show("chmod(R, 0o755)",  lambda: os.chmod(R, 0o755))
show("lchown(R, 0, 0)",  lambda: os.lchown(R, 0, 0))

print("== top-level dirs dpkg has to materialise ==")
for p in ("/dev", "/proc", "/sys", "/etc", "/var", "/usr", "/run", "/tmp"):
    show("stat(%s)" % p, (lambda q=p: os.stat(q)))

print("== can dpkg create files on / at all? ==")
for p in ("/.ciel-probe-1", "/.ciel-probe-2"):
    try:
        os.close(os.open(p, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644))
        print("%-40s CREATED" % ("open(%s, O_CREAT|O_EXCL)" % p))
        os.unlink(p)
    except OSError as e:
        print("%-40s ERR  %d (%s)" % ("open(%s, O_CREAT|O_EXCL)" % p,
                e.errno, errno.errorcode.get(e.errno, "?")))
PYEOF

have_guest_python() {
    # stderr 必须吞掉：这里的失败会被翻译成「guest 里没 python3」这句
    # 面向用户的话。proot 缺失的情况已由 require_instance 提前拦下。
    run_guest "$ROOT" /bin/bash -c 'command -v python3' >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# ★ pty-probe：acbs-build 需要 PTY（pexpect），而 proot 只bind 了 6 个字符设备
# ---------------------------------------------------------------------------
# 为什么用独立脚本而不是 `python3 -c "..."`：
#   宿主 bash → proot → guest /bin/bash -c → python3 -c 是四层引号嵌套。
#   python -c 的代码里必然有换行和双引号（traceback 格式串、dict、字符串），
#   任何一层引号失衡都会产生**指向错误位置**的报错docs/M3-TEST-PLAN.md 阶段 B
#   记录过一次真实翻车：`unexpected EOF while looking for matching '"'`，
#   看起来像用户引号写错，实际是 argv 传输层的问题。
#   写成文件后代码完全不需要转义，也不受 argv 分隔符影响。
#
# 与 syscall-probe 同样的手法：文件落到 $ROOT/tmp/，用 guest 视角的绝对路径跑。
read -r -d '' PTY_PROBE_SRC <<'PYEOF'
import errno
import os
import stat
import sys


def show(label, fn):
    try:
        r = fn()
        print("%-42s OK   %r" % (label, r))
        return True
    except OSError as e:
        print("%-42s ERR  %d (%s) %s" % (
            label, e.errno, errno.errorcode.get(e.errno, "?"), e.strerror))
        return False
    except Exception as e:
        print("%-42s EXC  %r" % (label, e))
        return False


print("== identity ==")
# geteuid/getegid 只在 Unix 有；防御一下，让 probe 在非 Unix 上也能跑到底
# （否则后面所有段都不会执行，本地复现/单元测试就没法做了）。
try:
    print("euid=%d egid=%d" % (os.geteuid(), os.getegid()))
except AttributeError:
    print("euid/egid: n/a (non-Unix platform)")
print("python=%s" % sys.version.split()[0])

print()
print("== 1. device nodes: does /dev/ptmx exist at all ==")
show("stat(/dev/ptmx)",   lambda: os.stat("/dev/ptmx"))
show("S_ISCHR(/dev/ptmx)", lambda: stat.S_ISCHR(os.stat("/dev/ptmx").st_mode))
show("stat(/dev/pts)",    lambda: os.stat("/dev/pts"))
show("S_ISDIR(/dev/pts)", lambda: stat.S_ISDIR(os.stat("/dev/pts").st_mode))
show("listdir(/dev/pts)", lambda: os.listdir("/dev/pts"))

print()
print("== 2. posix_openpt: the libc call pexpect actually makes ==")
# pexpect -> ptyprocess -> os.openpty() -> posix_openpt(O_RDWR)
# which opens /dev/ptmx. If /dev/ptmx is missing this is ENOENT; if it
# exists but the kernel refuses (SELinux / devpts not mounted) it's EPERM/EACCES.
show("posix_openpt(O_RDWR)", lambda: os.posix_openpt(os.O_RDWR))

print()
print("== 3. os.openpty: what pexpect.spawn() ends up calling ==")
show("os.openpty()", lambda: os.openpty())

print()
print("== 4. pty.fork(): full fork+exec under a pty, like a real build ==")


def fork_and_exec():
    # ★ 必须用 os.forkpty()，不能用 os.fork()。
    # os.fork() 返回裸 int，解包成 (pid, fd) 必然 TypeError；
    # 更糟的是 fork 已经成功，父/子两个进程都会继续跑完脚本剩余部分，
    # 导致后面每段输出出现两遍、两份 traceback 交织（2026-10-06 实测翻车）。
    # os.forkpty() 返回 (pid, master_fd)，子进程天生就是会话首进程 +
    # 带控制终端，正是 pexpect/ptyprocess 要的语义。
    # 副产物：那次翻车顺带证明了 proot 下 os.fork() 本身是好的 ——
    # 只有 pty 设备分配失败，fork/clone 没问题。
    pid, fd = os.forkpty()
    if pid == 0:
        # child: already a session leader with a controlling terminal.
        # Use `sh` (not /bin/echo) so this works on both the guest and a
        # Termux host, where /bin/echo does not exist.
        import shutil
        sh = shutil.which("sh") or "/bin/sh"
        try:
            os.execvp(sh, [sh, "-c", "echo PTY_FORK_OK"])
        except BaseException:
            os._exit(127)
        os._exit(127)  # unreachable, belt and braces
    # parent: drain the master side
    out = b""
    try:
        import select
        while True:
            r, _, _ = select.select([fd], [], [], 5.0)
            if not r:
                out += b"<TIMEOUT>"
                break
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break  # EIO: child exited and slave side closed
            if not chunk:
                break
            out += chunk
    finally:
        os.close(fd)
        _, status = os.waitpid(pid, 0)
    return (out.decode(errors="replace").strip(), "exit=%d" % (status >> 8))


show("forkpty + exec sh -c echo", fork_and_exec)

print()
print("== 5. the actual acbs codepath ==")
try:
    import pexpect
    print("%-42s OK   pexpect %s" % ("import pexpect", pexpect.__version__))
    try:
        import ptyprocess
        print("%-42s OK   ptyprocess %s"
              % ("import ptyprocess", getattr(ptyprocess, "__version__", "?")))
    except ImportError as e:
        print("%-42s ERR  %s" % ("import ptyprocess", e))
    # This is literally what acbs/utils.py:start_build_capture does.
    print("now trying pexpect.spawn('/bin/echo') -- acbs's exact call ...")
    child = pexpect.spawn("/bin/echo", ["PTY_PEXPECT_OK"], timeout=10)
    # expect(EOF), NOT expect(TIMEOUT): echo exits immediately, so EOF is
    # the expected termination. Expecting TIMEOUT here made a *successful*
    # spawn look like a failure -- the 2026-10-06 method-A-v2 run showed a
    # pexpect.exceptions.EOF traceback even though the dump proved
    # before=b'PTY_PEXPECT_OK\\r\\n' and exitstatus=0 (spawn worked; only
    # the expect call was wrong).
    child.expect(pexpect.EOF, timeout=10)
    got = child.before.decode(errors="replace").strip()
    if "PTY_PEXPECT_OK" in got:
        print("%-42s OK   %r (exitstatus=%s)"
              % ("pexpect.spawn + readback", got, child.exitstatus))
    else:
        print("%-42s ERR  expected output missing, got %r"
              % ("pexpect.spawn + readback", got))
except ImportError as e:
    print("%-42s ERR  %s" % ("import pexpect", e))
    print("  -> acbs would set build_logging=False and fall back to")
    print("     subprocess.check_call(['autobuild']) (utils.py:339),")
    print("     which needs NO pty. That path is a valid workaround.")

print()
print("== 6. tl;dr ==")
try:
    import pexpect  # noqa: F401
    HAS_PEXPECT = True
except ImportError:
    HAS_PEXPECT = False

try:
    # posix_openpt 只在 Unix 有；Windows 上会 AttributeError
    os.posix_openpt(os.O_RDWR)
    OPENPT = True
except AttributeError:
    OPENPT = False
except OSError:
    OPENPT = False

if not HAS_PEXPECT:
    print("VERDICT: no pexpect -> acbs build path needs NO PTY.")
    print("  acbs sets build_logging=False and calls subprocess.check_call.")
    print("  Build should work as-is (you lose the build log / winsize).")
elif OPENPT:
    print("VERDICT: pexpect present AND posix_openpt works -> PTY is fine here.")
    print("  acbs-build should take the pexpect path successfully.")
    print("  If the build still fails, the problem is NOT the PTY.")
else:
    print("VERDICT: pexpect present BUT posix_openpt fails -> PTY IS the blocker.")
    print("  acbs/utils.py:214 pexpect.spawn('autobuild') will raise here.")
    print("  (Python reports this as `OSError: out of pty devices` after")
    print("   os.openpty() ENOENT and the BSD /dev/ptyXX fallback exhausts.)")
    print("  NOTE 2026-10-06: -b /dev/ptmx + -b /dev/pts are now BAKED INTO")
    print("    both ciel-proot.sh and proot.rs (gated on the host having")
    print("    them), so this failure means the HOST lacks a usable devpts.")
    print("    Check the host run below: if host posix_openpt is OK but the")
    print("    guest's is ENOENT, the host files are missing (bind source")
    print("    gate skipped them); if both fail, Android/SELinux blocks it.")
    print("  Fix B (fallback): stub out pexpect so the import fails, forcing")
    print("    acbs down the subprocess.check_call path (no PTY needed).")
    print("    NOTE: shadowing the package DIR with an empty bind does NOT")
    print("    work -- Python 3.3+ treats a dir without __init__.py as a")
    print("    namespace package, so `import pexpect` still succeeds. You")
    print("    must bind a stub over .../pexpect/__init__.py instead.")
PYEOF

# ---------------------------------------------------------------------------
# ★ copy-probe：shutil 零拷贝拷贝 × proot/Android 内核的 EINVAL 取证
# ---------------------------------------------------------------------------
# 背景（docs/M3-TEST-PLAN.md 第 12 节）：acbs 的 git fetcher（fetch.py:207
# git_processor → shutil.copytree）把 .git pack 文件从 SRCS bind 拷进构建
# 目录时报 `[Errno 22] Invalid argument`。读 CPython 3.14 shutil.py 实锤：
#   * 快速路径用 blocksize = max(st_size, 8MiB)，请求 range 超出源文件 EOF；
#   * copy_file_range(2) 旧内核语义对此返回 EINVAL（"Requested range extends
#     beyond the end of the source file"）；
#   * CPython 只在「首调且零写入」时降级；首调成功后续 EINVAL 直接 raise，
#     且 err.filename = fsrc.name —— 报错带 SRC 路径的出处。
# 本 probe 对 pack 文件逐一回放两个 syscall 的精确调用序列，一次跑完回答：
#   ① sitecustomize 补丁是否已生效（flags 是否为 False）；
#   ② 裸 read 是否正常（排除读层问题）；
#   ③ copy_file_range / sendfile 分别在哪一次调用、什么 offset 上 EINVAL；
#   ④ shutil.copy2 能否复现。
read -r -d '' COPY_PROBE_SRC <<'PYEOF'
import glob
import os
import shutil
import sys


def show(label, fn):
    try:
        r = fn()
        print("%-46s OK   %s" % (label, r))
        return True
    except OSError as e:
        print("%-46s ERR  %s" % (label, e))
        return False
    except Exception as e:
        print("%-46s EXC  %r" % (label, e))
        return False


print("== 0. patch state ==")
print("sitecustomize in sys.modules:", "sitecustomize" in sys.modules)
print("shutil._USE_CP_SENDFILE      =", getattr(shutil, "_USE_CP_SENDFILE", "MISSING"))
print("shutil._USE_CP_COPY_FILE_RANGE =", getattr(shutil, "_USE_CP_COPY_FILE_RANGE", "MISSING"))
for cand in sorted(glob.glob("/usr/lib/python3.*/site-packages/sitecustomize.py")):
    print("on disk:", cand)

packs = sorted(glob.glob("/var/cache/acbs/tarballs/*/objects/pack/*.pack"))
print()
print("== 1. pack files found: %d ==" % len(packs))
if not packs:
    print("no pack files to probe (git 源包还没抓过?)")
    sys.exit(0)
p = packs[0]
size = os.path.getsize(p)
print("probe:", p)
print("size:", size, "bytes")

print()
print("== 2. plain open/read (慢速路径等价) ==")
show("open+read all %d bytes" % size, lambda: "%d bytes, magic=%r" % (len(open(p, "rb").read()), open(p, "rb").read(4)))


def seq(syscall_name, fn):
    """回放 CPython 的精确调用序列：blocksize=max(size,8MiB)，循环到 EOF。"""
    def run():
        src = os.open(p, os.O_RDONLY)
        dst = os.open("/tmp/.ciel-probe-dst.bin",
                      os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        try:
            blocksize = max(os.fstat(src).st_size, 2 ** 23)
            offset = 0
            calls = 0
            while True:
                calls += 1
                try:
                    n = fn(src, dst, offset, blocksize)
                except OSError as e:
                    return ("call#%d offset=%d count=%d -> %s"
                            % (calls, offset, blocksize, e))
                if n == 0:
                    return "clean EOF after %d call(s)" % calls
                offset += n
        finally:
            os.close(src)
            os.close(dst)
    return show(syscall_name, run)


print()
print("== 3. copy_file_range，CPython 精确序列 ==")
seq("copy_file_range sequence",
    lambda s, d, off, cnt: os.copy_file_range(s, d, cnt, offset_dst=off))

print()
print("== 4. sendfile，CPython 精确序列 ==")
seq("sendfile sequence",
    lambda s, d, off, cnt: os.sendfile(d, s, off, cnt))

print()
print("== 5. shutil.copy2（acbs copytree 实际用的）==")
show("copy2(pack)", lambda: str(shutil.copy2(p, "/tmp/.ciel-probe-copy2.bin")))

print()
print("== 6. tl;dr ==")
flags_off = (getattr(shutil, "_USE_CP_SENDFILE", True) is False
             and getattr(shutil, "_USE_CP_COPY_FILE_RANGE", True) is False)
print("patch loaded:", "sitecustomize" in sys.modules, "| flags off:", flags_off)
if flags_off:
    print("补丁已生效。若上面 #5 仍失败，说明 EINVAL 不在快速拷贝路径——")
    print("把本输出发回，重新定位（裸 read 的结果尤其关键）。")
else:
    print("补丁未生效（flags 不是 False）。sitecustomize.py 必须在")
    print("ciel-proot.sh 同目录（harness 会自动装进 dist+实例），或手工放")
    print("dist/usr/lib/python3.14/site-packages/ —— 放实例里会被 rollback 洗掉。")
PYEOF

cmd_copy_probe() {
    set_instance "${1:-}"
    require_instance

    if ! have_guest_python; then
        die "guest 里没有 python3，probe 跑不起来。"
    fi

    # require_instance 已顺手安装 sitecustomize（若脚本旁有该文件），
    # 所以这里 flags 的状态就是「下次真实构建会看到的状态」。
    mkdir -p "$ROOT/tmp"
    printf '%s\n' "$COPY_PROBE_SRC" >"$ROOT/tmp/.ciel-copy-probe.py"

    head1 "guest 拷贝机制取证（shutil 零拷贝 × 内核 EINVAL）"
    run_guest "$ROOT" /usr/bin/python3 /tmp/.ciel-copy-probe.py
    local rc=$?
    rm -f "$ROOT/tmp/.ciel-copy-probe.py"

    printf '\n  读法：\n' >&2
    cat >&2 <<'EOF'
  * #0 flags off: True  -> 补丁生效；此时 #5 应 OK，直接重跑 bzip3。
  * #0 flags off: False -> 补丁没装上。把 sitecustomize.py 放到
    ciel-proot.sh 同目录再跑一次本 probe（harness 会自动装）。
  * #3/#4 显示 call#N offset>0 EINVAL -> 内核 EOF 语义问题实锤，
    sitecustomize 禁 fast-path 即为正解。
  * #2 裸 read 失败 -> 问题比拷贝更底层（SRCS bind 读），另查。
EOF
    return $rc
}

cmd_pty_probe() {
    set_instance "${1:-}"
    require_instance

    # ★ 先把实际生效的 extra options 打出来。2026-10-06 的两次修法实测里，
    # 环境变量被 harness 静默丢弃（当时没转发 CIEL_PROOT_EXTRA_OPTIONS），
    # guest 输出与基线完全相同，用户无从判断「修法无效」还是「选项没传到」。
    # 把这一行放最前面，这类问题一眼可见。
    head1 "extra options（本次实际会传给 proot 的）"
    if [ -n "${CIEL_PROOT_EXTRA_OPTIONS:-}" ]; then
        printf '  CIEL_PROOT_EXTRA_OPTIONS = %s\n' "$CIEL_PROOT_EXTRA_OPTIONS" >&2
        case "$CIEL_PROOT_EXTRA_OPTIONS" in
            *'~'/*)
                warn "值里有字面 ~ —— bash 在双引号里不展开它，proot 也不展开。"
                warn "已自动改为 \$HOME；但 Rust 侧（ciel build）读到的是展开后的值，"
                warn "所以更稳妥的写法是用 \$HOME：export CIEL_PROOT_EXTRA_OPTIONS=\"-b \$HOME/...\""
                ;;
        esac
    else
        printf '  CIEL_PROOT_EXTRA_OPTIONS = <未设置>\n' >&2
        printf '  （默认绑定：/proc /sys 整体 + /dev 六个节点 + workspace binds）\n' >&2
    fi

    if ! have_guest_python; then
        die "guest 里没有 python3，probe 跑不起来。
  先在实例里装一个：
      $0 -C $WS run $INSTANCE /usr/bin/apt-get install -y python3
  如果那条也失败，说明 oma upgrade 还跑不通——那就先解决更新问题。"
    fi

    # guest 有自己的 rootfs，看不到宿主的 $TMPDIR。tmp 不在 SKIP_PATHS 里，
    # root/tmp 一定存在（与 syscall-probe 同一个理由）。
    mkdir -p "$ROOT/tmp"
    printf '%s\n' "$PTY_PROBE_SRC" >"$ROOT/tmp/.ciel-pty-probe.py"

    head1 "guest PTY 探测（等价于 acbs-build 起autobuild 子进程的方式）"
    run_guest "$ROOT" /usr/bin/python3 /tmp/.ciel-pty-probe.py
    local rc=$?
    rm -f "$ROOT/tmp/.ciel-pty-probe.py"

    # 宿主侧跑**完整** probe，不只是第 4 段。
    # 原因：决定走修法A（bind /dev/ptmx）还是修法B（stub pexpect）的唯一
    # 关键数据是「Android/Termux 宿主有没有能用的 devpts」。这只看宿主的
    # posix_openpt / openpty / forkpty，截取单段反而丢信息。
    # （上一版只 sed 出第 4 段，结果那段恰是当时唯一有 bug 的地方 ——
    #   宿主侧什么都没告诉我们。）
    head1 "对照：宿主 (Termux) 完整跑一遍 —— 决定修法A是否可行"
    if command -v python3 >/dev/null 2>&1; then
        printf '%s\n' "$PTY_PROBE_SRC" >"$ROOT/tmp/.ciel-pty-probe-host.py"
        python3 "$ROOT/tmp/.ciel-pty-probe-host.py" 2>&1 || true
        rm -f "$ROOT/tmp/.ciel-pty-probe-host.py"
        printf '\n  ↑ 看宿主的 posix_openpt / os.openpty 那两行：\n' >&2
        printf '    OK   -> 修法A可行，试试下面的命令\n' >&2
        printf '    ERR  -> Android/SELinux 不给 devpts，只能走修法B\n' >&2
    else
        warn "宿主没有 python3（Termux: pkg install python），跳过对照"
        warn "没有宿主数据就无法判断修法A是否可行"
    fi

    printf '\n  修法A验证（宿主 openpty OK 时）：\n' >&2
    cat >&2 <<'EOF'
    export CIEL_PROOT_EXTRA_OPTIONS="-b /dev/ptmx -b /dev/pts:/dev/pts"
    $0 -C $WS pty-probe $INSTANCE
    # guest 的 posix_openpt 从 ENOENT 变 OK -> 去跑 ciel build
    # 变 EPERM/EACCES -> SELinux 拦了，走修法B
    # 注意：单节点 bind 与 2026-10-06 的 /dev 教训不冲突，
    #       整体 bind /dev 才会让 guest 的 /dev 变 Android 只读 tmpfs。
  修法B（修法A不可行时）：stub 掉 pexpect 的 __init__.py。
    不能用空目录 bind 盖住整个包 —— Python 3.3+ 会把没有 __init__.py
    的目录当 namespace package，import 照样成功。
    要 bind 单个文件盖住 __init__.py，让 import 抛 ImportError。
EOF
    return $rc
}

cmd_syscall_probe() {
    set_instance "${1:-}"
    require_instance

    if ! command -v python3 >/dev/null 2>&1; then
        warn "宿主没有 python3（Termux: pkg install python），无法做 host 侧对照"
        warn "仍会在 guest 里跑一次，但 diff 对比会失效"
    fi
    if ! have_guest_python; then
        die "guest 里没有 python3，probe 跑不起来。
  先在实例里装一个：
      $0 -C $WS run $INSTANCE /usr/bin/apt-get install -y python3
  如果那条也失败，说明 oma upgrade 还跑不通——那就先解决更新问题。"
    fi

    local work probe
    work="$(mktemp -d "${TMPDIR:-/tmp}/ciel-probe.XXXXXX")"
    probe="$work/probe.py"
    printf '%s\n' "$PROBE_SRC" >"$probe"

    # probe.py 必须放进 guest 能看到的路径里 —— guest 有自己的 rootfs，
    # 看不到宿主的 $TMPDIR。tmp 不在 SKIP_PATHS 里，root/tmp 一定存在。
    # （v1 的 bug：直接把宿主路径传给 guest 的 python3 → ENOENT）
    mkdir -p "$ROOT/tmp"
    cp "$probe" "$ROOT/tmp/.ciel-probe.py"

    head1 "host (Termux, no proot)"
    if command -v python3 >/dev/null 2>&1; then
        python3 "$probe" 2>&1 | tee "$work/host.txt"
    else
        printf '  (skipped)\n' | tee "$work/host.txt"
    fi

    head1 "guest (inside proot, same script)"
    run_guest "$ROOT" /usr/bin/python3 /tmp/.ciel-probe.py 2>&1 | tee "$work/proot.txt"
    rm -f "$ROOT/tmp/.ciel-probe.py"

    head1 "DIFF (host vs proot)  <-- 这一段是关键"
    # 注意：Android 宿主受 SELinux 限制（open(/, O_DIRECTORY)=EACCES、
    # scandir(/)=EACCES），host/guest 有几处天然差异是平台差异不是 proot
    # 的锅。重点看 mkdir/unlink/open(O_CREAT|O_EXCL) 的 errno 是否一致。
    if diff -u "$work/host.txt" "$work/proot.txt"; then
        ok "errno 语义完全一致 -> proot 不是元凶，问题在 dpkg 自身或 payload"
    else
        warn "上面每一个差异都是 proot 造成的语义偏差 —— dpkg 的 EDSP::Resolve 很可能就死在这里"
    fi
    printf '\n  完整输出：%s/host.txt  %s/proot.txt\n' "$work" "$work" >&2
    say "结果目录 $work （看完可删）"
}

# ---------------------------------------------------------------------------
# ★ dpkg-probe：aosc-aaa 失败的对照矩阵
# ---------------------------------------------------------------------------
cmd_dpkg_probe() {
    set_instance "${1:-}"
    require_instance

    local deb=""
    if [ -d "$WS/CACHE" ]; then
        deb="$(ls -1 "$WS/CACHE"/aosc-aaa_*.deb 2>/dev/null | sort -V | tail -1)"
    fi
    if [ -z "$deb" ] && [ -d "$ROOT/var/cache/apt/archives" ]; then
        deb="$(ls -1 "$ROOT/var/cache/apt/archives"/aosc-aaa_*.deb 2>/dev/null | sort -V | tail -1)"
    fi
    [ -n "$deb" ] || die \
"找不到 aosc-aaa_*.deb。

  这个包是 oma upgrade 下载的，落在 CACHE 那个 bind 上：
      宿主：$WS/CACHE/aosc-aaa_<版本>.deb
      guest：/var/cache/apt/archives/aosc-aaa_<版本>.deb
  先跑一次下载：$0 -C $WS update $INSTANCE"
    say "目标 deb: $deb"

    # 换成 guest 视角的绝对路径（CACHE 被 bind 到 /var/cache/apt/archives）
    local gde
    case "$deb" in
        "$WS/CACHE"/*) gde="/var/cache/apt/archives/${deb##*/}" ;;
        *)             gde="$deb" ;;
    esac
    say "guest 路径: $gde"

    warn "dpkg --unpack 会真的改动实例；实验后建议重建：ciel del + ciel add"

    local saved_p="$USE_PSEUDO" saved_l="$USE_LINK2SYMLINK" saved_c="$USE_CHANGE_ID"
    local v vp vl vc out rc a
    for v in "baseline 1 1 1" \
             "no-pseudo 0 1 1" \
             "no-link2symlink 1 0 1" \
             "no-change-id 1 1 0" \
             "none-of-the-three 0 0 0"; do
        set -- $v
        vp="$2"; vl="$3"; vc="$4"
        USE_PSEUDO="$vp"; USE_LINK2SYMLINK="$vl"; USE_CHANGE_ID="$vc"

        head1 "variant: $1  (pseudo=$vp link2symlink=$vl change-id=$vc)"
        # payload 顶层长什么样 —— 验证 aosc-aaa 是不是唯一只有骨架的包
        printf '  payload 顶层条目：\n'
        run_guest "$ROOT" /bin/bash -c \
            "dpkg-deb -c '$gde' 2>/dev/null | awk '{print \$6}' | sed 's#^\./##' | cut -d/ -f1 | sort -u | head -30 | sed 's/^/    /'"
        # 逐级放宽 force，看哪一级的 force 能救回来
        for a in "" \
                 "--force-overwrite-dir" \
                 "--force-overwrite-dir --force-bad-path" \
                 "--force-all"; do
            if [ -z "$a" ]; then
                printf '  dpkg --unpack (raw) ... '
            else
                printf '  dpkg --unpack %s ... ' "$a"
            fi
            out="$(run_guest "$ROOT" /bin/bash -c "dpkg --unpack $a '$gde' 2>&1")"
            rc=$?
            if printf '%s' "$out" | grep -q "EDSP::Resolve"; then
                printf '%sSTILL EDSP::Resolve FAILS%s\n' "$C_R" "$C_0"
                printf '    %s\n' "$out" | head -c 1200; printf '\n'
            elif [ "$rc" -eq 0 ]; then
                printf '%sSUCCESS%s\n' "$C_G" "$C_0"
            else
                printf 'rc=%s  %s\n' "$rc" "$(printf '%s' "$out" | head -c 1200)"
            fi
        done
    done
    USE_PSEUDO="$saved_p"; USE_LINK2SYMLINK="$saved_l"; USE_CHANGE_ID="$saved_c"

    cat <<'EOF'

读法：
  * baseline 就不过、none-of-the-three 也不过  -> 与 proot flag 无关，是 dpkg/payload 自身
  * 去掉某个 flag 就过-> 该 flag 是元凶，代码里去掉它
  * 某一级 --force 能过-> 记下是哪个 force，再决定是打 patch
     还是让 OMA_UPDATE_SCRIPT 带上它
  * 只有 host/proot errno 有 diff            -> proot 语义偏差，回去看 syscall-probe
EOF
}

# ---------------------------------------------------------------------------
# update / update-base / promote
# ---------------------------------------------------------------------------
# 与 actions/mod.rs:22 逐字一致
OMA_UPDATE_SCRIPT='export OMA_NO_BELL=1 OMA_NO_PROGRESS=1 OMA_NO_CLEAN=1;oma upgrade -y --force-confnew --force-unsafe-io --no-check-dbus && oma autoremove -y --remove-config --no-check-dbus'
# 与 actions/mod.rs:21 逐字一致（注意它没有 --force-unsafe-io）
APT_UPDATE_SCRIPT='export DEBIAN_FRONTEND=noninteractive;apt-get update -y --allow-releaseinfo-change && apt-get -y -o Dpkg::Options::="--force-confnew" full-upgrade --autoremove --purge'

cmd_update() {
    set_instance "${1:-}"
    require_instance
    local script="${2:-$OMA_UPDATE_SCRIPT}"
    head1 "在实例 $INSTANCE 里跑更新（结果只留在实例，不碰基础层）"
    say "$script"
    run_guest "$ROOT" /bin/bash -ec "$script"
    local rc=$?
    if [ "$rc" -eq 0 ]; then
        ok "更新成功。提升为基础层：$0 -C $WS update-base $INSTANCE"
    else
        warn "更新失败 rc=$rc"
        warn "注意 packaging.rs:198 那里还会白等 363 秒（3+9+27+81+243）才换 apt 重试"
    fi
    return $rc
}

# dircopy 模型下的 commit：实例 root 就是 dist 的完整实体副本，
# 更新完之后它本身就是「dist + 全部 delta」。所以 commit 不需要做
# rsync 式的差异合并，直接 rename 换手即可 —— O(1)，且删除操作
# 自动被带上（这正是包被卸载时的正确语义）。
promote() {
    local root="$1"
    [ -d "$root/usr" ] || die "$root 没有 usr/，不是有效 rootfs"
    # proot 的 link2symlink 假硬链接簿记（.l2s.*）带着创建者实例的绝对路径，
    # 冻进 dist 会让下一个实例里 dpkg 的 link(status,status-old) 备份翻车
    # （2026-10-15 aosc 14.0.0 update-os 实测：EPERM + status 库损坏）。
    # 提升前必须清掉 —— 与 dircopy.rs 的 purge_l2s_bookkeeping 对应。
    find "$root" -name '.l2s.*' -depth -delete 2>/dev/null
    local stamp; stamp="$(date +%Y%m%d-%H%M%S)"
    local old="$DIST.pristine-$stamp"

    say "旧基础层 -> $old"
    say "实例 root -> 基础层"
    mv "$DIST" "$old" || die "移动 $DIST 失败"
    if ! mv "$root" "$DIST"; then
        warn "提升失败，回滚：$old -> $DIST"
        mv "$old" "$DIST" || die "回滚也失败了！基础层现在在 $old，手动处理"
        die "已回滚"
    fi
    # 骨架重新补上：promote 只关心 rootfs，manifest 属于实例生命周期
    mkdir -p "$INST_DIR/manifest"
    printf 'dircopy\n' > "$INST_DIR/manifest/backend"
    printf '1\n'       > "$INST_DIR/manifest/seeded"
    ok "基础层已更新（原 BuildKit 保留在 $old）"
    printf '  回退：rm -rf %s && mv %s %s\n' "$DIST" "$old" "$DIST" >&2
    printf '  删旧实例：rm -rf %s\n' "$INST_DIR" >&2
    printf '  释放空间：rm -rf %s\n' "$old" >&2
}

cmd_promote() {
    set_instance "${1:-}"
    [ -d "$ROOT/usr" ] || die "实例 $INSTANCE 的 root 未初始化"
    head1 "把实例 $INSTANCE 提升为新的基础层"
    promote "$ROOT"
}

cmd_update_base() {
    set_instance "${1:-}"
    require_instance
    head1 "① 更新"
    run_guest "$ROOT" /bin/bash -ec "$OMA_UPDATE_SCRIPT"
    local rc=$?
    [ "$rc" -eq 0 ] || die "更新失败 rc=$rc，不提升（半更新的基础层比旧基础层更糟）"

    head1 "② 验证更新真的生效了"
    run_guest "$ROOT" /bin/bash -ec \
        "dpkg --audit; echo '--- oma 版本 ---'; oma -v 2>/dev/null | head -3; echo '--- 抽查几个包 ---'; dpkg-query -W -f='\${Package} \${Version}\n' bash libc6 dpkg 2>/dev/null"

    head1 "③ 提升为基础层"
    promote "$ROOT"
    ok "以后 ciel add 出来的实例直接就是最新的 OS"
    ok "package_build 里的 oma upgrade 从「重放全部 delta」变成廉价 no-op"
    printf '\n  下一步：删掉旧实例，再 add 一个干净的来打包\n' >&2
    printf '  rm -rf %s\n' "$INST_DIR" >&2
}

# ---------------------------------------------------------------------------
case "$CMD" in
    doctor)        cmd_doctor ;;
    shell|shell-i) cmd_shell_i "$@" ;;
    run)           cmd_run "$@" ;;
    syscall-probe) cmd_syscall_probe "$@" ;;
    dpkg-probe)    cmd_dpkg_probe "$@" ;;
    update)        cmd_update "$@" ;;
    update-base)   cmd_update_base "$@" ;;
    promote)       cmd_promote "$@" ;;
    argv)          ensure_bind_dirs; build_argv "$ROOT" /bin/bash -c true | tr '\0' '\n' ;;
    # pty-probe 见 cmd_pty_probe：acbs-build 走 pexpect，需要 PTY。
    # 这是独立脚本而不是内联 python，因为宿主 bash → proot → guest bash
    # → python 四层引号嵌套极易出错，且出错信息具有误导性。
    pty-probe)     cmd_pty_probe "$@" ;;
    copy-probe)    cmd_copy_probe "$@" ;;
    help|-h|--help) usage ;;
    *)             die "未知子命令 $CMD（-h 看帮助）" ;;
esac