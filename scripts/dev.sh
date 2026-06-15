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

set -o pipefail

# ---------- 路径 ----------
# 脚本位于 scripts/ 子目录, 工程根为其父目录
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
VERSION="$(tr -d '[:space:]' < "$PROJECT_ROOT/docs/VERSION" 2>/dev/null || echo '?')"
BUILD_DIR="$PROJECT_ROOT/build"
BUILD_DEBUG_DIR="$PROJECT_ROOT/build_debug"
# Go 仓库与本工程同级: windinput/WindInput
GO_REPO="$(dirname "$PROJECT_ROOT")/WindInput"

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
    local go_build="$GO_REPO/build"
    say "\n从 Go 仓库复制 TSF DLL ($go_build)..."
    mkdir -p "$outdir"
    local dll
    for dll in wind_tsf.dll wind_tsf_x86.dll; do
        if [ -f "$go_build/$dll" ]; then
            cp -f "$go_build/$dll" "$outdir/$dll"
            gray "已复制: $dll"
        else
            warn "未找到: $go_build/$dll"
        fi
    done
}

copy_data() {
    local outdir="${1:-$BUILD_DIR}"
    # 必须用 Go 仓库的 build_debug/data (构建产物, 含已下载的 rime 词典 + .schema.toml),
    # 而非 WindInput/data (源目录, 不含 .dict.yaml 词典文件)。否则部署后词典缺失,
    # 引擎无法构建, 只能显示编码无候选。
    local go_data="$GO_REPO/build_debug/data"
    if [ ! -f "$go_data/schemas/wubi86/wubi86_jidian.dict.yaml" ]; then
        warn "警告: $go_data 缺少词典, 回退到源目录 (仅 schema, 无词典)"
        go_data="$GO_REPO/data"
    fi

    say "\n从 Go 仓库复制 data/ ($go_data)..."
    if [ -d "$go_data" ]; then
        mkdir -p "$outdir"
        rm -rf "$outdir/data"
        cp -rf "$go_data" "$outdir/data"
        gray "已复制: data/"
    else
        warn "未找到: $go_data"
    fi
}

deploy_all() {
    local outdir="${1:-$BUILD_DIR}"
    mkdir -p "$outdir"
    copy_tsf_dll "$outdir"
    copy_data "$outdir"
    say "部署完成! -> $outdir"
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
    printf '\n%b  部署:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    5  - 完整部署 (复制 DLL + data)"
    echo  "    6  - 从 Go 仓库复制 TSF DLL"
    echo  "    7  - 从 Go 仓库复制 data/"
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
    -h|--help|help)
        grep '^#' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        ;;
    *)
        err "未知命令: $1"
        echo "运行 './scripts/dev.sh --help' 查看可用命令"
        exit 1
        ;;
esac
