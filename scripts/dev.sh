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
#   tsf     | 6     构建 C++ TSF DLL (MinGW 交叉编译; tsf debug → 调试变体)
#   data    | 7     组装 data/（data/ 源 + .cache/ 下载 → build_debug/data/）
#   fmt     | f     cargo fmt
#   fmt-check       cargo fmt --check (CI 用)
#   clean   | c     cargo clean
#   ci              fmt-check + clippy + test (提交前一把过)
#
# 实测 / 远程部署（SSH；配置见 scripts/deploy.local: WIND_REMOTE / WIND_REMOTE_DIR）:
#   repl [data]     本机跑候选 REPL（无需 TSF/UI；读编码→打印候选）
#   push [debug] [data]  交叉编译 exe 并 drop-in 到 Windows 安装目录（复用其 TSF DLL+大词库）；
#                   debug → 构建 debug_variant 并推为 wind_input_debug.exe；先 taskkill 远程进程再覆盖；
#                   data  → exe 推完再推源 data/（manifest/方案/主题），随手同步免遗漏
#   push-data       仅推送源 data/（manifest/方案/主题/默认配置）到 Windows，不重编 exe
#   pull-data       从 Windows 安装目录拉 data/（真实词库）到 .cache/pulled-data/ 供 REPL 使用
#   pull-config     从 Windows 拉 config.toml（%APPDATA%\<App>）到 .remote/ 查看
#   pull-log [all]  从 Windows 拉日志（%LOCALAPPDATA%\<App>\logs）到 .remote/；
#                   默认仅最新一天，all 拉整目录（需 deploy.local 配 WIND_DATA_DIR/WIND_LOCAL_DIR）
#   gen-data        下载外部词库到 .cache/ + 组装 build_debug/data/
#
# 数据目录说明：
#   data/           源文件（入库）：配置、五笔词库、主题等手工维护文件
#   .cache/         外部下载/生成（gitignore）：rime-frost、opencc、unigram 等
#   build_debug/data/ 完整运行时数据（由 assemble_data 从 data/ + .cache/ 合并）
#
# 推荐实测流程：① gen-data 下载+组装词库 → ② repl 在 Linux 验证候选逻辑
#               ③ push 把 exe drop-in 到 Windows → 重启服务做应用内实测

set -o pipefail

# ---------- 路径 ----------
# 目录层级: <产品仓>/scripts/dev.sh
#   SCRIPT_DIR   = <产品仓>/scripts
#   PRODUCT_ROOT = <产品仓>          (含 docs/VERSION、data/、.cache/ 等)
#   PROJECT_ROOT = <产品仓>/wind_input (Cargo workspace 根)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PRODUCT_ROOT="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$PRODUCT_ROOT/wind_input"
# C++ TSF 核心层（MinGW 交叉编译，见 wind_tsf/Makefile）
TSF_DIR="$PRODUCT_ROOT/wind_tsf"
VERSION="$(tr -d '[:space:]' < "$PRODUCT_ROOT/docs/VERSION" 2>/dev/null || echo '?')"
BUILD_DIR="$PROJECT_ROOT/build"
BUILD_DEBUG_DIR="$PROJECT_ROOT/build_debug"
# 外部下载/生成的词库缓存目录（不入库）
CACHE_DIR="$PRODUCT_ROOT/.cache"
# Rust 工具链根目录（wind_input/ workspace）
RUST_WORKSPACE="$PRODUCT_ROOT/wind_input"

# 远程 Windows 测试机配置（SSH）。在 scripts/deploy.local 或环境变量中设置：
#   WIND_REMOTE      = user@host           （SSH 目标）
#   WIND_REMOTE_DIR  = Windows 安装目录     （含 wind_input.exe；用 scp 正斜杠风格，
#                      如 'C:/Users/me/AppData/Local/Programs/WindInput'；调试时指向 WindInputDebug 安装目录）
[ -f "$SCRIPT_DIR/deploy.local" ] && . "$SCRIPT_DIR/deploy.local"
WIND_REMOTE="${WIND_REMOTE:-}"
WIND_REMOTE_DIR="${WIND_REMOTE_DIR:-}"
# 远程数据/本地目录（拉配置、拉日志用；见 deploy.local 注释）：
#   WIND_DATA_DIR   = %APPDATA%\<App>        含 config.toml（用户配置）
#   WIND_LOCAL_DIR  = %LOCALAPPDATA%\<App>   含 logs/（服务日志）、cache/
WIND_DATA_DIR="${WIND_DATA_DIR:-}"
WIND_LOCAL_DIR="${WIND_LOCAL_DIR:-}"
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
say()  { printf '%b%b%b\n' "$C_GREEN" "$1" "$C_RESET"; }
warn() { printf '%b%b%b\n' "$C_YELLOW" "$1" "$C_RESET"; }
err()  { printf '%b%b%b\n' "$C_RED" "$1" "$C_RESET"; }
gray() { printf '%b%b%b\n' "$C_GRAY" "$1" "$C_RESET"; }

# ---------- 构建 ----------
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

    local src_exe="$PROJECT_ROOT/target/$TARGET/$bindir/wind_input.exe"
    local dst_exe="$outdir/wind_input${suffix}.exe"
    if [ -f "$src_exe" ]; then
        cp -f "$src_exe" "$dst_exe"
        gray "已复制: wind_input${suffix}.exe ($(du -h "$dst_exe" | cut -f1))"
    else
        warn "未找到产物: $src_exe"
    fi

    build_tsf "$outdir" "$debug"
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

# ---------- 部署 ----------
# 构建 C++ TSF 核心层（MinGW 交叉编译）并复制到输出目录。
#   $1 outdir（默认 build/）  $2 = "debug" → 调试变体 wind_tsf_debug.dll
build_tsf() {
    local outdir="${1:-$BUILD_DIR}"
    local debug="${2:-}"
    mkdir -p "$outdir"

    if ! command -v x86_64-w64-mingw32-g++ >/dev/null 2>&1; then
        warn "未找到 x86_64-w64-mingw32-g++（mingw-w64 工具链）；跳过 TSF 构建。"
        gray "  安装后可构建 TSF DLL；'push' 经 SSH 部署时也可复用 Windows 上已有的 DLL。"
        return 0
    fi

    local mk_args dll
    if [ "$debug" = "debug" ]; then
        mk_args="DEBUG_VARIANT=1"; dll="wind_tsf_debug.dll"
    else
        mk_args=""; dll="wind_tsf.dll"
    fi

    say "\n正在交叉编译 TSF DLL ($dll)..."
    # 让 make 直接输出到目标目录，避免二次复制
    if make -C "$TSF_DIR" $mk_args VERSION="$VERSION" OUTDIR="$outdir" >/dev/null; then
        gray "已构建: $dll ($(du -h "$outdir/$dll" 2>/dev/null | cut -f1))"
    else
        err "TSF DLL 构建失败！见 'make -C $TSF_DIR $mk_args' 输出。"
        return 1
    fi
}

# ---------- 词库下载 ----------

# helper: 下载单个文件（已存在则跳过）
download_file() {
    local url="$1" dst="$2" desc="${3:-}"
    if [ -f "$dst" ]; then
        gray "[skip] $(basename "$dst") 已存在"
        return 0
    fi
    gray "[get ] $(basename "$dst") $desc"
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "$dst" "$url"; then
        err "下载失败: $url"
        return 1
    fi
}

# 下载外部词库到 .cache/
download_dicts() {
    say "\n下载外部词库 → $CACHE_DIR"
    local rime_frost="$CACHE_DIR/rime-frost"
    local rime_frost_cn="$rime_frost/cn_dicts"
    local rime_frost_en="$rime_frost/en_dicts"
    local opencc="$CACHE_DIR/opencc/dictionaries"
    mkdir -p "$rime_frost_cn" "$rime_frost_en" "$opencc"

    local FROST_BASE="https://raw.githubusercontent.com/gaboolic/rime-frost/master"
    gray "rime-frost (拼音):"
    download_file "$FROST_BASE/rime_frost.dict.yaml"              "$rime_frost/rime_frost.dict.yaml"        "词库入口"
    download_file "$FROST_BASE/cn_dicts/8105.dict.yaml"           "$rime_frost_cn/8105.dict.yaml"           "单字词库"
    download_file "$FROST_BASE/cn_dicts/41448.dict.yaml"          "$rime_frost_cn/41448.dict.yaml"          "扩展字表"
    download_file "$FROST_BASE/cn_dicts/base.dict.yaml"           "$rime_frost_cn/base.dict.yaml"           "基础词库 ~10MB"
    download_file "$FROST_BASE/cn_dicts/ext.dict.yaml"            "$rime_frost_cn/ext.dict.yaml"            "扩展词库 ~8MB"
    download_file "$FROST_BASE/cn_dicts/others.dict.yaml"         "$rime_frost_cn/others.dict.yaml"         "容错词"
    download_file "$FROST_BASE/cn_dicts/corrections.dict.yaml"    "$rime_frost_cn/corrections.dict.yaml"    "错音词"
    download_file "$FROST_BASE/cn_dicts/tencent.dict.yaml"        "$rime_frost_cn/tencent.dict.yaml"        "腾讯词频 ~17MB"

    gray "rime-frost (英文):"
    download_file "$FROST_BASE/en_dicts/en.dict.yaml"     "$rime_frost_en/en.dict.yaml"     "主词库"
    download_file "$FROST_BASE/en_dicts/en_ext.dict.yaml" "$rime_frost_en/en_ext.dict.yaml" "扩展"

    local OPENCC_BASE="https://raw.githubusercontent.com/BYVoid/OpenCC/master/data/dictionary"
    gray "OpenCC 简繁词典:"
    download_file "$OPENCC_BASE/STCharacters.txt" "$opencc/STCharacters.txt" "简->繁 字级"
    download_file "$OPENCC_BASE/STPhrases.txt"    "$opencc/STPhrases.txt"    "简->繁 词级"
    download_file "$OPENCC_BASE/TWVariants.txt"   "$opencc/TWVariants.txt"   "台湾字形"
    download_file "$OPENCC_BASE/TWPhrases.txt"    "$opencc/TWPhrases.txt"    "台湾词汇"
    download_file "$OPENCC_BASE/HKVariants.txt"   "$opencc/HKVariants.txt"   "香港字形"
}

# 从 data/（源）+ .cache/（下载/生成）组装完整运行时数据到 $outdir/data/
assemble_data() {
    local outdir="${1:-$BUILD_DEBUG_DIR}"
    local data="$outdir/data"
    local schemas="$data/schemas"
    local pinyin="$schemas/pinyin"
    local pinyin_cn="$pinyin/cn_dicts"
    local english="$schemas/english"
    local rime_frost="$CACHE_DIR/rime-frost"

    say "\n组装 data/ → $data"
    rm -rf "$data"

    # 1. 复制 data/ 源文件（configs、五笔词库、主题等）
    cp -rf "$PRODUCT_ROOT/data" "$data"

    # 2. rime-frost 拼音词库
    mkdir -p "$pinyin_cn"
    if [ -f "$rime_frost/rime_frost.dict.yaml" ]; then
        cp -f "$rime_frost/rime_frost.dict.yaml" "$pinyin/"
        for f in 8105.dict.yaml 41448.dict.yaml base.dict.yaml ext.dict.yaml \
                 others.dict.yaml corrections.dict.yaml; do
            [ -f "$rime_frost/cn_dicts/$f" ] && cp -f "$rime_frost/cn_dicts/$f" "$pinyin_cn/"
        done
    else
        warn "缺 .cache/rime-frost/，拼音词库不可用（运行 gen-data 下载）"
    fi

    # 3. 英文词库
    mkdir -p "$english"
    for f in en.dict.yaml en_ext.dict.yaml; do
        [ -f "$rime_frost/en_dicts/$f" ] && cp -f "$rime_frost/en_dicts/$f" "$english/"
    done

    # 4. Unigram 语言模型
    local unigram_cache="$CACHE_DIR/pinyin-frost/unigram.txt"
    if [ -f "$unigram_cache" ]; then
        cp -f "$unigram_cache" "$pinyin/unigram.txt"
    else
        warn "缺 unigram.txt（运行 gen-data 生成）"
    fi

    # 5. OpenCC 编译 .octrie（Rust 工具 gen_opencc）
    mkdir -p "$data/opencc"
    if [ -d "$CACHE_DIR/opencc/dictionaries" ] && \
       [ "$(ls "$CACHE_DIR/opencc/dictionaries/"*.txt 2>/dev/null | wc -l)" -gt 0 ]; then
        gray "编译 OpenCC → .octrie ..."
        ( cd "$RUST_WORKSPACE" && cargo run -q --bin gen_opencc -- \
            --src "$CACHE_DIR/opencc/dictionaries" --out "$data/opencc" ) \
            || warn "OpenCC 编译失败（简繁转换不可用）"
    else
        warn "缺 .cache/opencc/，OpenCC 不可用（运行 gen-data 下载）"
    fi

    gray "data/ 组装完成 ($(find "$data" -type f | wc -l) 文件)"
}

# 组装 data/ 到指定输出目录。
# 优先从 data/ + .cache/ 本地组装；若 .cache/ 尚未下载，回退到 Go 构建产物。
copy_data() {
    local outdir="${1:-$BUILD_DIR}"
    local cache_probe="$CACHE_DIR/rime-frost/cn_dicts/base.dict.yaml"

    if [ -f "$cache_probe" ]; then
        assemble_data "$outdir"
    else
        warn "找不到词典数据；请运行 'gen-data' 下载词库，或 'pull-data' 从 Windows 拉取"
    fi
}

deploy_all() {
    local outdir="${1:-$BUILD_DIR}"
    mkdir -p "$outdir"
    build_tsf "$outdir"
    copy_data "$outdir"
    say "部署完成! -> $outdir"
}

# ---------- 实测 / 远程部署（SSH）----------

# 本机跑候选 REPL。data 目录优先 build_debug/data/，其次 .cache/pulled-data/。
do_repl() {
    local data="${1:-}"
    if [ -z "$data" ]; then
        if [ -f "$BUILD_DEBUG_DIR/data/schemas/pinyin/unigram.txt" ]; then
            data="$BUILD_DEBUG_DIR/data"
        elif [ -d "$CACHE_DIR/pulled-data" ]; then
            data="$CACHE_DIR/pulled-data"
            gray "使用 pull-data 拉取的词库: $data"
        else
            warn "未找到词库数据；请先运行 gen-data 或 pull-data"
            data="$BUILD_DEBUG_DIR/data"
        fi
    fi
    say "\n启动候选 REPL (data=$data)..."
    cd "$PROJECT_ROOT" && WIND_DATA="$data" cargo run --release -p wind-repl -- "$data"
}

require_remote() {
    if [ -z "$WIND_REMOTE" ] || [ -z "$WIND_REMOTE_DIR" ]; then
        err "未配置远程：请在 $SCRIPT_DIR/deploy.local 设置 WIND_REMOTE 与 WIND_REMOTE_DIR"
        echo "  示例: WIND_REMOTE=me@192.168.1.10"
        echo "        WIND_REMOTE_DIR='C:/Users/me/AppData/Local/Programs/WindInput'"
        return 1
    fi
}

# drop-in：把交叉编译的 exe 推到 Windows 安装目录。
# 参数（顺序无关，可组合）：
#   debug  → 构建 debug_variant 并推为 wind_input_debug.exe（缺省推 release）
#   data   → exe 推送后，额外推送源 data/（manifest/方案/主题等；见 do_push_data）
do_push() {
    require_remote || return 1
    cd "$PROJECT_ROOT" || return 1
    local variant="" push_data=""
    local a
    for a in "$@"; do
        case "$a" in
            debug) variant="debug" ;;
            data)  push_data="1" ;;
        esac
    done
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

    say "停止远程 $exe_name..."
    ssh "$WIND_REMOTE" "taskkill /F /IM $exe_name" >/dev/null 2>&1 || true
    sleep 1

    say "推送 $exe_name → $WIND_REMOTE:$WIND_REMOTE_DIR/"
    if scp "$exe" "$WIND_REMOTE:$WIND_REMOTE_DIR/$exe_name"; then
        say "已推送 exe。"
    else
        err "scp 失败：检查 WIND_REMOTE_DIR 路径(正斜杠 C:/...)、SSH 连通、文件是否仍被占用"
        return 1
    fi

    # 可选：连源 data/ 一并推送（manifest/方案/主题改动随 exe 一起上去）
    if [ -n "$push_data" ]; then
        do_push_data || return 1
    fi
    say "完成。请在 Windows 桌面重启 $exe_name。"
}

# 推送源 data/（$PRODUCT_ROOT/data：manifest/方案 toml/主题/默认配置/短语）到 Windows 安装目录的 data/。
# 仅覆盖同名文件、新增缺失文件（scp 合并语义，不删除）——远端组装的大词库（拼音/opencc/unigram，
# 由 .cache 组装、不在源 data/）不受影响。适合改完 manifest/schema/theme 后单独同步，无需重编 exe。
do_push_data() {
    require_remote || return 1
    local src="$PRODUCT_ROOT/data"
    [ -d "$src" ] || { err "源 data/ 不存在: $src"; return 1; }
    say "\n推送源 data/ → $WIND_REMOTE:$WIND_REMOTE_DIR/data/ ($(du -sh "$src" 2>/dev/null | cut -f1))"
    # 推送 data/ 内容（用 /* 推内容而非目录本身，避免 scp 生成 data/data 嵌套）。
    # 远端 data/ 已随安装存在；若不存在 scp 会明确报错（按提示先做一次完整 deploy）。
    if scp -r "$src"/* "$WIND_REMOTE:$WIND_REMOTE_DIR/data/"; then
        say "已推送源 data/（远端大词库未触碰）。manifest 改动重启或「重载配置」后生效。"
    else
        err "scp data 失败：检查 WIND_REMOTE_DIR/data 路径、SSH、文件占用"
        return 1
    fi
}

# 从 Windows 安装目录拉取已处理的 data/（含真实词库）到 .cache/pulled-data/ 供 REPL 使用。
do_pull_data() {
    require_remote || return 1
    local dst="$CACHE_DIR/pulled-data"
    say "\n拉取 data/ ← $WIND_REMOTE:$WIND_REMOTE_DIR/data  →  $dst"
    rm -rf "$dst"
    mkdir -p "$CACHE_DIR"
    if scp -r "$WIND_REMOTE:$WIND_REMOTE_DIR/data" "$dst"; then
        say "已拉取 → $dst"
        say "提示: REPL 会自动使用此词库，或用 './dev.sh repl $dst' 显式指定"
    else
        err "scp 失败（检查路径/SSH）"
    fi
}

require_remote_dirs() {
    require_remote || return 1
    if [ -z "$WIND_DATA_DIR" ] || [ -z "$WIND_LOCAL_DIR" ]; then
        err "未配置远程目录：请在 $SCRIPT_DIR/deploy.local 设置 WIND_DATA_DIR 与 WIND_LOCAL_DIR"
        echo "  示例: WIND_DATA_DIR='C:/Users/me/AppData/Roaming/WindInputDebug'"
        echo "        WIND_LOCAL_DIR='C:/Users/me/AppData/Local/WindInputDebug'"
        return 1
    fi
}

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
    say "\n查询远程最新日志 ← $WIND_REMOTE:$WIND_LOCAL_DIR/logs/"
    local latest
    latest="$(ssh "$WIND_REMOTE" "powershell -NoProfile -Command \"Get-ChildItem -Path '$WIND_LOCAL_DIR/logs' -Filter 'wind_input.log*' | Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty Name\"" 2>/dev/null | tr -d '\r')"
    if [ -z "$latest" ]; then
        err "未找到日志文件（或用 'pull-log all' 整目录拉取）"
        return 1
    fi
    local dst="$REMOTE_PULL_DIR/logs/$latest"
    say "拉取最新日志 $latest"
    if scp "$WIND_REMOTE:$WIND_LOCAL_DIR/logs/$latest" "$dst"; then
        say "已拉取 → $dst"
    else
        err "scp 失败"
    fi
}

# 下载外部词库到 .cache/ + 生成 unigram + 组装 build_debug/data/
do_gen_data() {
    local outdir="${1:-$BUILD_DEBUG_DIR}"
    if ! command -v curl >/dev/null 2>&1; then
        err "需要 curl（下载词库用）"; return 1
    fi

    download_dicts || return 1

    # 生成 Unigram 语言模型（Rust 工具 gen_unigram）
    local unigram_cache="$CACHE_DIR/pinyin-frost/unigram.txt"
    mkdir -p "$(dirname "$unigram_cache")"
    if [ ! -f "$unigram_cache" ]; then
        say "生成 Unigram 语言模型..."
        ( cd "$RUST_WORKSPACE" && cargo run -q --bin gen_unigram -- \
            --rime "$CACHE_DIR/rime-frost/cn_dicts" \
            --out "$unigram_cache" ) \
            || warn "Unigram 生成失败（智能组句不可用）"
    else
        gray "Unigram 已缓存"
    fi

    assemble_data "$outdir"
    say "gen-data 完成 → $outdir/data"
}

# ---------- 发布:产出完整可打包目录(exe + x64/x86 TSF + data)----------
# 全部产物落到 BUILD_DIR(wind_input/build/),供 scripts/pack-installer.sh 打包。
do_dist() {
    local outdir="$BUILD_DIR"
    say "\n=== 构建发布目录 → $outdir ==="

    do_build || return 1                    # release exe + x64 TSF + copy_data

    # x86 TSF（32 位宿主程序兼容,需要 i686 工具链）
    if command -v i686-w64-mingw32-g++ >/dev/null 2>&1; then
        say "\n交叉编译 x86 TSF DLL (wind_tsf_x86.dll)..."
        if make -C "$TSF_DIR" x86 VERSION="$VERSION" OUTDIR="$outdir" >/dev/null; then
            gray "已构建: wind_tsf_x86.dll ($(du -h "$outdir/wind_tsf_x86.dll" 2>/dev/null | cut -f1))"
        else
            err "x86 TSF 构建失败！见 'make -C $TSF_DIR x86' 输出。"; return 1
        fi
    else
        warn "未找到 i686-w64-mingw32-g++；跳过 x86 TSF（32 位宿主将无输入法）。"
    fi

    # 正式数据(下载词库 + gen_unigram + assemble→编译 opencc octrie)
    do_gen_data "$outdir" || return 1

    say "\n发布目录就绪 → $outdir"
    say "  打包: scripts/pack-installer.sh --version $VERSION"
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
    echo  "    6  - 构建 C++ TSF DLL (MinGW 交叉编译)"
    echo  "    7  - 组装 data/ (data/ 源 + .cache/ → build_debug/data/)"
    printf '\n%b  实测 / 远程 (SSH → %s):%b\n' "$C_YELLOW" "${WIND_REMOTE:-未配置}" "$C_RESET"
    echo  "    r  - 候选 REPL (本机验证, 无需 Windows)"
    echo  "    p  - push: 交叉编译 exe → Windows (release)"
    echo  "    pd - push debug: exe + 源 data/ → Windows (调试)"
    echo  "    pda- push-data: 仅推源 data/ (manifest/方案/主题, 不重编 exe)"
    echo  "    dl - pull-data: 从 Windows 拉真实词库 → .cache/pulled-data/"
    echo  "    pc - pull-config: 从 Windows 拉 config.toml 回本机查看"
    echo  "    pl - pull-log: 从 Windows 拉最新日志回本机 (pla = 整目录)"
    echo  "    gd - gen-data: 下载外部词库 + 组装 build_debug/data/"
    printf '\n%b  工具:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    f  - cargo fmt (代码格式化)"
    echo  "    i  - ci (fmt-check + clippy + test)"
    echo  "    c  - cargo clean (清理构建)"
    echo  "    q  - 退出"
    printf '%b============================================%b\n' "$C_CYAN" "$C_RESET"
}

pause() { printf '\n'; read -e -r -p "按回车继续..." _; }

menu_loop() {
    # 启用历史，使下方 read -e 的上/下方向键可调出历史输入。
    set -o history 2>/dev/null || true
    while true; do
        show_menu
        printf '\n'
        # -e 启用 readline 行编辑：方向键不再回显 ^[[A，左右移动光标、上下调历史。
        read -e -r -p "请输入选项: " choice
        [ -n "$choice" ] && history -s "$choice"
        case "$(printf '%s' "$choice" | tr '[:upper:]' '[:lower:]')" in
            1)   do_build;          pause ;;
            1d)  do_build debug;    pause ;;
            2)   do_check;          pause ;;
            3)   do_clippy;         pause ;;
            4)   do_test;           pause ;;
            5)   deploy_all;        pause ;;
            6)   build_tsf;         pause ;;
            7)   copy_data "$BUILD_DEBUG_DIR"; pause ;;
            r)   do_repl;           pause ;;
            p)   do_push;           pause ;;
            pd)  do_push debug data; pause ;;
            pda) do_push_data;      pause ;;
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
# 长名与菜单缩写均可直调（如 './dev.sh push-data' 等价 './dev.sh pda'）；命令转小写以容错。
cmd="$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')"
case "$cmd" in
    ""|menu)              menu_loop ;;
    release|1)            do_build ;;
    debug|1d)             do_build debug ;;
    check|2)              do_check ;;
    clippy|3)             do_clippy ;;
    test|4)               do_test ;;
    deploy|5)             deploy_all ;;
    tsf|dll|6)            build_tsf "$BUILD_DIR" "${2:-}" ;;
    data|7)               copy_data "$BUILD_DEBUG_DIR" ;;
    fmt|f)                do_fmt ;;
    fmt-check)            do_fmt_check ;;
    clean|c)              do_clean ;;
    ci|i)                 do_ci ;;
    repl|r)               do_repl "${2:-}" ;;
    push|p)               do_push "${2:-}" "${3:-}" ;;
    pd)                   do_push debug data ;;
    push-data|pushdata|pda) do_push_data ;;
    pull-data|dl)         do_pull_data ;;
    pull-config|pc)       do_pull_config ;;
    pull-log|pl)          do_pull_log "${2:-}" ;;
    pla)                  do_pull_log all ;;
    gen-data|gd)          do_gen_data ;;
    dist)                 do_dist ;;
    -h|--help|help)
        grep '^#' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        ;;
    *)
        err "未知命令: $1"
        echo "运行 './scripts/dev.sh --help' 查看可用命令"
        exit 1
        ;;
esac
