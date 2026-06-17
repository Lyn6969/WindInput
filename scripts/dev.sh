#!/usr/bin/env bash
# WindInput 开发菜单 (Linux → Windows 交叉编译)
#
# 用法:
#   ./scripts/dev.sh            # 交互式菜单 (对齐 dev.ps1)
#   ./scripts/dev.sh <命令>     # 非交互直调, 如 ./scripts/dev.sh release
#
# 本机 (Linux) 交叉编译为 Windows 可执行文件:
#   - 目标 triple: x86_64-pc-windows-gnu (纯 Rust, 无 C 链接)
#   - 产物: target/<target>/<profile>/wind_input.exe
#   - build/check/clippy 走 Windows 目标; test 在本机跑 (Windows 代码在 cfg(windows) 后)
#
# 命令列表 (与菜单一一对应):
#   release | 1     Release 构建 + 部署
#   debug   | 1d    Debug 构建 + 部署 (dev profile + debug_variant 特性)
#   check   | 2     cargo check (Windows 目标, 全工作区)
#   clippy  | 3     cargo clippy (Windows 目标, 全工作区)
#   test    | 4     cargo test (本机, 全工作区)
#   deploy  | 5     完整部署 (复制 DLL + data)
#   dll     | 6     从 Go 仓库复制 TSF DLL
#   data    | 7     从 Go 仓库复制 data/
#   fmt     | f     cargo fmt
#   fmt-check       cargo fmt --check (CI 用)
#   clean   | c     cargo clean
#   ci              fmt-check + clippy + test (提交前一把过)
#
# 实测 / 远程部署（SSH；配置见 scripts/deploy.local: WIND_REMOTE / WIND_REMOTE_DIR）:
#   repl [data]     本机跑候选 REPL（无需 TSF/UI；读编码→打印候选）
#   push [debug]    交叉编译 exe 并 drop-in 到 Windows 安装目录（复用其 TSF DLL+data）；
#                   debug → 构建 debug_variant 并推为 wind_input_debug.exe；先 taskkill 远程进程再覆盖
#   pull-data       从 Windows 安装目录拉 data/（真实词库）回本机供 REPL
#   pull-config     从 Windows 拉 config.toml（%APPDATA%\<App>）到 .remote/ 查看
#   pull-log [all]  从 Windows 拉日志（%LOCALAPPDATA%\<App>\logs）到 .remote/；
#                   默认仅最新一天，all 拉整目录（需 deploy.local 配 WIND_DATA_DIR/WIND_LOCAL_DIR）
#   gen-data        独立下载+转换词库（暂用 Go 工具；Rust 化为后续）
#
# 推荐实测流程：① pull-data 拉真实词库 → ② repl 在 Linux 验证候选逻辑
#               ③ push 把 exe drop-in 到 Windows → 重启服务做应用内实测（并验协议兼容）

set -o pipefail

# ---------- 路径 ----------
# 目录层级: <产品仓>/scripts/dev.sh （产品级编排脚本，统管 wind_input/ 及未来的 tsf/macos/）
#   SCRIPT_DIR   = <产品仓>/scripts
#   PRODUCT_ROOT = <产品仓>          (产品仓根, 含 docs/VERSION、data/ 等共享资产)
#   PROJECT_ROOT = <产品仓>/wind_input (Cargo workspace 根)
# 路径全部相对脚本自身(BASH_SOURCE)解析，与 CWD 无关。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PRODUCT_ROOT="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$PRODUCT_ROOT/wind_input"
VERSION="$(tr -d '[:space:]' < "$PRODUCT_ROOT/docs/VERSION" 2>/dev/null || echo '?')"
BUILD_DIR="$PROJECT_ROOT/build"
BUILD_DEBUG_DIR="$PROJECT_ROOT/build_debug"
# Go 仓库与产品仓同级: windinput/WindInput
GO_REPO="$(dirname "$PRODUCT_ROOT")/WindInput"

# 远程 Windows 测试机配置（SSH）。在 scripts/deploy.local 或环境变量中设置：
#   WIND_REMOTE      = user@host           （SSH 目标）
#   WIND_REMOTE_DIR  = Windows 安装目录     （含 wind_input.exe；用 scp 正斜杠风格，
#                      如 'C:/Users/me/AppData/Local/Programs/WindInput'；调试时指向 WindInputDebug 安装目录）
# 传输用 scp（stock Windows OpenSSH 自带，无需装 rsync）。deploy.local 不入库（.gitignore）。
[ -f "$SCRIPT_DIR/deploy.local" ] && . "$SCRIPT_DIR/deploy.local"
WIND_REMOTE="${WIND_REMOTE:-}"
WIND_REMOTE_DIR="${WIND_REMOTE_DIR:-}"
# 远程数据/本地目录（拉配置、拉日志用；见 deploy.local 注释）：
#   WIND_DATA_DIR   = %APPDATA%\<App>        含 config.toml（用户配置）
#   WIND_LOCAL_DIR  = %LOCALAPPDATA%\<App>   含 logs/（服务日志）、cache/
WIND_DATA_DIR="${WIND_DATA_DIR:-}"
WIND_LOCAL_DIR="${WIND_LOCAL_DIR:-}"
# 本机给 REPL 用的 data 目录（pull-data 拉取到此）
LOCAL_DATA="$PRODUCT_ROOT/data"
# 从远程拉取的配置/日志落地处（本地查看用，不入库）
REMOTE_PULL_DIR="$PRODUCT_ROOT/.remote"

TARGET="x86_64-pc-windows-gnu"

# ---------- 颜色 ----------
if [ -t 1 ]; then
    C_CYAN='\033[36m'; C_YELLOW='\033[33m'; C_GREEN='\033[32m'
    C_RED='\033[31m'; C_GRAY='\033[90m'; C_RESET='\033[0m'
else
    C_CYAN=''; C_YELLOW=''; C_GREEN=''; C_RED=''; C_GRAY=''; C_RESET=''
fi
# 消息用 %b 解释 \n 等转义 (消息内容受控)
say()  { printf '%b%b%b\n' "$C_GREEN" "$1" "$C_RESET"; }
warn() { printf '%b%b%b\n' "$C_YELLOW" "$1" "$C_RESET"; }
err()  { printf '%b%b%b\n' "$C_RED" "$1" "$C_RESET"; }
gray() { printf '%b%b%b\n' "$C_GRAY" "$1" "$C_RESET"; }

# ---------- 构建 ----------
# 参数: $1 = "debug" 表示调试构建, 否则 release
do_build() {
    local debug="${1:-}"
    local profile outdir suffix bindir
    if [ "$debug" = "debug" ]; then
        profile="debug"; outdir="$BUILD_DEBUG_DIR"; suffix="_debug"; bindir="debug"
    else
        profile="release"; outdir="$BUILD_DIR"; suffix=""; bindir="release"
    fi

    say "\n正在交叉编译 ($profile, $TARGET)..."
    cd "$PROJECT_ROOT" || return 1

    if [ "$debug" = "debug" ]; then
        cargo build --target "$TARGET" -p wind_service --features debug_variant || { err "构建失败!"; return 1; }
    else
        cargo build --release --target "$TARGET" -p wind_service || { err "构建失败!"; return 1; }
    fi

    mkdir -p "$outdir"

    # 复制二进制
    local src_exe="$PROJECT_ROOT/target/$TARGET/$bindir/wind_input.exe"
    local dst_exe="$outdir/wind_input${suffix}.exe"
    if [ -f "$src_exe" ]; then
        cp -f "$src_exe" "$dst_exe"
        gray "已复制: wind_input${suffix}.exe ($(du -h "$dst_exe" | cut -f1))"
    else
        warn "未找到产物: $src_exe"
    fi

    copy_tsf_dll "$outdir"
    copy_data "$outdir"
    say "构建完成! -> $outdir"
}

do_check() {
    say "\n正在运行 cargo check ($TARGET, 全工作区)..."
    cd "$PROJECT_ROOT" && cargo check --target "$TARGET" --workspace
}

do_clippy() {
    say "\n正在运行 cargo clippy ($TARGET, 全工作区)..."
    cd "$PROJECT_ROOT" && cargo clippy --target "$TARGET" --workspace
}

do_test() {
    say "\n正在运行 cargo test (本机, 全工作区)..."
    cd "$PROJECT_ROOT" && cargo test --workspace
}

do_fmt() {
    say "\n正在运行 cargo fmt..."
    cd "$PROJECT_ROOT" && cargo fmt
}

do_fmt_check() {
    say "\n正在运行 cargo fmt --check..."
    cd "$PROJECT_ROOT" && cargo fmt --all -- --check
}

do_clean() {
    say "\n正在运行 cargo clean..."
    cd "$PROJECT_ROOT" && cargo clean
}

do_ci() {
    cd "$PROJECT_ROOT" || return 1
    do_fmt_check || { err "fmt 检查失败!"; return 1; }
    do_clippy    || { err "clippy 失败!"; return 1; }
    do_test      || { err "test 失败!"; return 1; }
    say "\nCI 全部通过 ✓"
}

# ---------- 部署 (从 Go 仓库复制 DLL / data) ----------
# 注意: Go 仓库 (../WindInput) 需先自行构建, 否则 DLL/词典缺失, 此处仅告警。
copy_tsf_dll() {
    local outdir="${1:-$BUILD_DIR}"
    mkdir -p "$outdir"
    say "\n复制 TSF DLL (暂复用 Go 仓库产物, 尚无 Rust 版)..."
    local dll base src found=0
    for dll in wind_tsf.dll wind_tsf_x86.dll; do
        for base in "$GO_REPO/build" "$GO_REPO/build_debug"; do
            src="$base/$dll"
            if [ -f "$src" ]; then
                cp -f "$src" "$outdir/$dll"
                gray "已复制: $dll (来自 $base)"
                found=1
                break
            fi
        done
    done
    if [ "$found" = 0 ]; then
        gray "未找到 Go TSF DLL (Go 仓库未构建)。多数情况下无碍：'push' 经 SSH 部署时"
        gray "复用 Windows 安装目录里已有的 DLL；仅在制作本地 build/ 完整镜像时才需要它。"
    fi
}

# 词典探针：判断某 data 目录是否含已处理词典（而非仅 schema）。
DICT_PROBE="schemas/wubi86/wubi86_jidian.dict.yaml"

copy_data() {
    local outdir="${1:-$BUILD_DIR}"
    # data 来源优先级：① 本机已拉取的真实词库 ($LOCAL_DATA, 来自 pull-data)
    #                  ② Go 构建产物 (build_debug/data, 含 rime 词典)
    #                  ③ Go 源目录 (仅 schema, 无 .dict.yaml 词典 → 引擎无候选)
    # 优先真实词库，避免历史上的"缺词典回退"告警。
    local src
    if [ -f "$LOCAL_DATA/$DICT_PROBE" ]; then
        src="$LOCAL_DATA"
        gray "data 源: 本机真实词库 $LOCAL_DATA (来自 pull-data)"
    elif [ -f "$GO_REPO/build_debug/data/$DICT_PROBE" ]; then
        src="$GO_REPO/build_debug/data"
        gray "data 源: Go 构建产物 $src"
    elif [ -d "$GO_REPO/data" ]; then
        src="$GO_REPO/data"
        warn "data 源: Go 源目录 (仅 schema, 无词典) —— 建议先 './dev.sh dl' 拉真实词库"
    else
        warn "找不到任何 data 源 (本机无真实词库, Go 仓库也未构建); 跳过 data 复制"
        return 0
    fi

    # 避免自拷：源即目标时无需复制。
    if [ "$src" = "$outdir/data" ]; then
        gray "data 已在目标位置, 跳过"
        return 0
    fi
    say "\n复制 data/ ($src → $outdir/data)..."
    mkdir -p "$outdir"
    rm -rf "$outdir/data"
    cp -rf "$src" "$outdir/data"
    gray "已复制: data/"
}

deploy_all() {
    local outdir="${1:-$BUILD_DIR}"
    mkdir -p "$outdir"
    copy_tsf_dll "$outdir"
    copy_data "$outdir"
    say "部署完成! -> $outdir"
}

# ---------- 实测 / 远程部署（SSH）----------

# 本机跑候选 REPL（无需 TSF/UI）：读编码→打印候选。data 目录优先 $LOCAL_DATA，可传参覆盖。
do_repl() {
    local data="${1:-$LOCAL_DATA}"
    say "\n启动候选 REPL (data=$data)..."
    cd "$PROJECT_ROOT" && WIND_DATA="$data" cargo run --release -p wind-repl -- "$data"
}

# 校验远程配置
require_remote() {
    if [ -z "$WIND_REMOTE" ] || [ -z "$WIND_REMOTE_DIR" ]; then
        err "未配置远程：请在 $SCRIPT_DIR/deploy.local 设置 WIND_REMOTE 与 WIND_REMOTE_DIR"
        echo "  示例: WIND_REMOTE=me@192.168.1.10"
        echo "        WIND_REMOTE_DIR='C:/Users/me/AppData/Local/Programs/WindInput'"
        return 1
    fi
}

# drop-in：把交叉编译的 exe 推到 Windows 安装目录（复用其 TSF DLL + data/），顺带验证 IPC 协议兼容。
# 用法: push          → release，推为 wind_input.exe
#       push debug    → debug 变体，推为 wind_input_debug.exe（WindInputDebug 隔离环境）
# 处理文件占用：先 taskkill 远程进程再 scp（覆盖后请在 Windows 桌面重启，TSF 自动重连）。
do_push() {
    require_remote || return 1
    cd "$PROJECT_ROOT" || return 1
    local variant="${1:-}"
    local exe exe_name
    if [ "$variant" = "debug" ]; then
        say "\n构建 debug 变体 (debug_variant)..."
        cargo build --target "$TARGET" -p wind_service --features debug_variant || { err "构建失败"; return 1; }
        exe="$PROJECT_ROOT/target/$TARGET/debug/wind_input.exe"
        exe_name="wind_input_debug.exe"
    else
        say "\n构建 release..."
        cargo build --release --target "$TARGET" -p wind_service || { err "构建失败"; return 1; }
        exe="$PROJECT_ROOT/target/$TARGET/release/wind_input.exe"
        exe_name="wind_input.exe"
    fi
    [ -f "$exe" ] || { err "未找到产物: $exe"; return 1; }

    say "停止远程 $exe_name（若在运行，避免文件占用）..."
    ssh "$WIND_REMOTE" "taskkill /F /IM $exe_name" >/dev/null 2>&1 || true
    sleep 1

    say "推送 $exe_name → $WIND_REMOTE:$WIND_REMOTE_DIR/"
    if scp "$exe" "$WIND_REMOTE:$WIND_REMOTE_DIR/$exe_name"; then
        say "已推送。请在 Windows 桌面重启 $exe_name（双击或经输入法菜单/重启服务），TSF 会自动重连命名管道。"
    else
        err "scp 失败：检查 WIND_REMOTE_DIR 路径(正斜杠 C:/...)、SSH 连通、文件是否仍被占用"
    fi
}

# 从 Windows 安装目录拉取已处理的 data/（含真实词库）到本机，供 REPL 使用。
# 用 scp -r（stock OpenSSH 兼容）；若你的 Windows 装了 rsync，可自行用 rsync 做增量。
do_pull_data() {
    require_remote || return 1
    say "\n拉取 data/ ← $WIND_REMOTE:$WIND_REMOTE_DIR/data  →  $LOCAL_DATA"
    rm -rf "$LOCAL_DATA"
    # scp -r 把远程 data 目录整体拷到产品仓根，即得 $PRODUCT_ROOT/data (=$LOCAL_DATA)
    if scp -r "$WIND_REMOTE:$WIND_REMOTE_DIR/data" "$PRODUCT_ROOT/"; then
        say "已拉取 → $LOCAL_DATA。现在可 './scripts/dev.sh repl' 用真实词库测试。"
    else
        err "scp 失败（检查路径/SSH）"
    fi
}

# 校验远程数据/本地目录配置（拉配置、拉日志用）
require_remote_dirs() {
    require_remote || return 1
    if [ -z "$WIND_DATA_DIR" ] || [ -z "$WIND_LOCAL_DIR" ]; then
        err "未配置远程目录：请在 $SCRIPT_DIR/deploy.local 设置 WIND_DATA_DIR 与 WIND_LOCAL_DIR"
        echo "  示例: WIND_DATA_DIR='C:/Users/me/AppData/Roaming/WindInputDebug'   # %APPDATA%"
        echo "        WIND_LOCAL_DIR='C:/Users/me/AppData/Local/WindInputDebug'    # %LOCALAPPDATA%"
        return 1
    fi
}

# 从 Windows 拉取用户配置 config.toml（%APPDATA%\<App>\config.toml）到本机查看。
do_pull_config() {
    require_remote_dirs || return 1
    mkdir -p "$REMOTE_PULL_DIR"
    local dst="$REMOTE_PULL_DIR/config.toml"
    say "\n拉取 config.toml ← $WIND_REMOTE:$WIND_DATA_DIR/config.toml"
    if scp "$WIND_REMOTE:$WIND_DATA_DIR/config.toml" "$dst"; then
        say "已拉取 → $dst"
    else
        err "scp 失败（检查 WIND_DATA_DIR 路径/SSH；config.toml 可能尚未生成）"
    fi
}

# 从 Windows 拉取服务日志（%LOCALAPPDATA%\<App>\logs\）到本机查看。
# 默认只拉最新一天的日志；传 all 拉整个 logs 目录。cache/ 不在此目录，不会被带下来。
do_pull_log() {
    require_remote_dirs || return 1
    mkdir -p "$REMOTE_PULL_DIR/logs"
    local mode="${1:-}"
    if [ "$mode" = "all" ]; then
        say "\n拉取全部日志 ← $WIND_REMOTE:$WIND_LOCAL_DIR/logs/"
        if scp -r "$WIND_REMOTE:$WIND_LOCAL_DIR/logs" "$REMOTE_PULL_DIR/"; then
            say "已拉取 → $REMOTE_PULL_DIR/logs/"
        else
            err "scp 失败（检查 WIND_LOCAL_DIR 路径/SSH）"
        fi
        return
    fi
    # 取远程最新的日志文件（按天滚动：wind_input.log.YYYY-MM-DD）
    say "\n查询远程最新日志 ← $WIND_REMOTE:$WIND_LOCAL_DIR/logs/"
    local latest
    latest="$(ssh "$WIND_REMOTE" "powershell -NoProfile -Command \"Get-ChildItem -Path '$WIND_LOCAL_DIR/logs' -Filter 'wind_input.log*' | Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty Name\"" 2>/dev/null | tr -d '\r')"
    if [ -z "$latest" ]; then
        err "未找到日志文件（检查 WIND_LOCAL_DIR/logs 是否存在；或用 'pull-log all' 整目录拉取）"
        return 1
    fi
    local dst="$REMOTE_PULL_DIR/logs/$latest"
    say "拉取最新日志 $latest"
    if scp "$WIND_REMOTE:$WIND_LOCAL_DIR/logs/$latest" "$dst"; then
        say "已拉取 → $dst"
    else
        err "scp 失败（检查 SSH/路径）"
    fi
}

# 独立词库流水线（不依赖 Windows 安装）：下载 rime 原始词库 + 转换。
# 转换暂复用 Go 仓库的工具（go run），Rust 化转换器为后续任务（见 docs）。
# 当前仅编排：调用 Go 仓库 build 脚本产出 data，再复制到 $LOCAL_DATA。
do_gen_data() {
    if ! command -v go >/dev/null 2>&1; then
        err "需要 Go 工具链（转换器仍是 Go 实现，Rust 化为后续任务）"; return 1
    fi
    if [ ! -d "$GO_REPO" ]; then
        err "未找到 Go 仓库: $GO_REPO（词库下载/转换工具在此）"; return 1
    fi
    warn "调用 Go 仓库下载+转换词库（首次约下载数十 MB）..."
    # build.sh 为 macOS 风格，Linux 可能需适配；失败请改用 pull-data 复用 Windows 已处理 data/。
    ( cd "$GO_REPO" && bash scripts_mac/build/build.sh data ) || { err "Go data 构建失败（可改用 pull-data）"; return 1; }
    local src="$GO_REPO/build/data"
    [ -d "$src" ] || src="$GO_REPO/build_debug/data"
    if [ -d "$src" ]; then
        mkdir -p "$LOCAL_DATA"; rsync -a --delete "$src/" "$LOCAL_DATA/"
        say "已生成 data → $LOCAL_DATA"
    else
        err "未找到生成的 data（$src）"
    fi
}

# ---------- 菜单 ----------
show_menu() {
    clear 2>/dev/null || true
    printf '%b============================================%b\n' "$C_CYAN" "$C_RESET"
    printf '%b  WindInput 开发菜单  v%s  (Linux→Win)%b\n' "$C_CYAN" "$VERSION" "$C_RESET"
    printf '%b============================================%b\n\n' "$C_CYAN" "$C_RESET"
    printf '%b  构建:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    1  - Release 构建 + 部署"
    echo  "    1d - Debug 构建 + 部署"
    echo  "    2  - cargo check (快速编译检查)"
    echo  "    3  - cargo clippy (代码检查)"
    echo  "    4  - cargo test (运行测试, 本机)"
    printf '\n%b  部署 (本机 build/ 镜像):%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    5  - 完整部署 (复制 DLL + data 到 build/)"
    echo  "    6  - 从 Go 仓库复制 TSF DLL"
    echo  "    7  - 复制 data/ (优先真实词库, 见 dl)"
    printf '\n%b  实测 / 远程 (SSH → %s):%b\n' "$C_YELLOW" "${WIND_REMOTE:-未配置}" "$C_RESET"
    echo  "    r  - 候选 REPL (本机验证, 无需 Windows)"
    echo  "    p  - push: 交叉编译 exe → Windows (release)"
    echo  "    pd - push debug: → wind_input_debug.exe (调试)"
    echo  "    dl - pull-data: 从 Windows 拉真实词库回本机"
    echo  "    pc - pull-config: 从 Windows 拉 config.toml 回本机查看"
    echo  "    pl - pull-log: 从 Windows 拉最新日志回本机 (pla = 整目录)"
    echo  "    gd - gen-data: 独立下载+转换词库"
    printf '\n%b  工具:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    f  - cargo fmt (代码格式化)"
    echo  "    i  - ci (fmt-check + clippy + test)"
    echo  "    c  - cargo clean (清理构建)"
    echo  "    q  - 退出"
    printf '%b============================================%b\n' "$C_CYAN" "$C_RESET"
}

pause() { printf '\n'; read -r -p "按回车继续..." _; }

menu_loop() {
    while true; do
        show_menu
        printf '\n'
        read -r -p "请输入选项: " choice
        case "$(printf '%s' "$choice" | tr '[:upper:]' '[:lower:]')" in
            1)   do_build;          pause ;;
            1d)  do_build debug;    pause ;;
            2)   do_check;          pause ;;
            3)   do_clippy;         pause ;;
            4)   do_test;           pause ;;
            5)   deploy_all;        pause ;;
            6)   copy_tsf_dll;      pause ;;
            7)   copy_data;         pause ;;
            r)   do_repl;           pause ;;
            p)   do_push;           pause ;;
            pd)  do_push debug;     pause ;;
            dl)  do_pull_data;      pause ;;
            pc)  do_pull_config;    pause ;;
            pl)  do_pull_log;       pause ;;
            pla) do_pull_log all;   pause ;;
            gd)  do_gen_data;       pause ;;
            f)   do_fmt;            pause ;;
            i)   do_ci;             pause ;;
            c)   do_clean;          pause ;;
            q)   exit 0 ;;
            "")  ;;
            *)   err "无效选项"; sleep 1 ;;
        esac
    done
}

# ---------- 命令行直调 ----------
case "${1:-}" in
    ""|menu)            menu_loop ;;
    release|1)          do_build ;;
    debug|1d)           do_build debug ;;
    check|2)            do_check ;;
    clippy|3)           do_clippy ;;
    test|4)             do_test ;;
    deploy|5)           deploy_all ;;
    dll|6)              copy_tsf_dll ;;
    data|7)             copy_data ;;
    fmt|f)              do_fmt ;;
    fmt-check)          do_fmt_check ;;
    clean|c)            do_clean ;;
    ci|i)               do_ci ;;
    repl)               do_repl "${2:-}" ;;
    push)               do_push "${2:-}" ;;
    pull-data)          do_pull_data ;;
    pull-config)        do_pull_config ;;
    pull-log)           do_pull_log "${2:-}" ;;
    gen-data)           do_gen_data ;;
    -h|--help|help)
        grep '^#' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        ;;
    *)
        err "未知命令: $1"
        echo "运行 './scripts/dev.sh --help' 查看可用命令"
        exit 1
        ;;
esac
