#!/usr/bin/env bash
# WindInput 开发菜单 (Linux → Windows 交叉编译)
#
# 用法:
#   ./scripts/dev.sh            # 交互式菜单 (对齐 dev.ps1)
#   ./scripts/dev.sh <命令>     # 非交互直调, 如 ./scripts/dev.sh release
#
# 本机 (Linux) 交叉编译为 Windows (MSVC) 可执行文件:
#   - Rust(wind_input): cargo-xwin → x86_64-pc-windows-msvc (+crt-static 自包含)
#   - Rust(../wind-setting): 独立仓库, 不存在则跳过设置程序
#   - Rust(../wind-portable): 独立仓库, 不存在则跳过便携启动器
#   - C++ TSF: clang + lld-link + llvm-rc + cargo-xwin 的 MSVC SDK (x64 + x86)
#   - 依赖: cargo-xwin + clang-19/lld-19/llvm-19 (MSVC STL 要 clang≥19)
#   - 全构建产物落【项目根】build/(release) 或 build_dev/(dev)，内容 == 安装内容
#
# 命令（菜单与命令行直调同一套；前缀 d=dev, p=push, m=单模块）:
#   1            Release 全构建: wind_input + tsf(x64/x86) + setting + portable + 词库数据 → build/
#   d1           Debug 全构建 → build_dev/
#   m1 / dm1     仅 tsf (x64+x86)            release / dev
#   m2 / dm2     仅 wind_input (核心 exe)     release / dev
#   m3 / dm3     仅 wind_setting (../wind-setting)              release / dev (不存在则跳过)
#   m4 / dm4     仅 wind_portable (../wind-portable)            release / dev (不存在则跳过)
#   8            生成安装包 (= 1 + 打包 → Setup.exe + sha256)
#   8s           跳过编译，直接打包现有 build/
#   p1 / pd1     push 全部 build[_dev]/ → Windows 安装目录 (release / dev)
#   pm1/pm2      push 单模块 (tsf/核心, release)
#   pdm1/pdm2    push 单模块 (dev)
#   k=check  l=clippy  t=test  f=fmt  fmt-check  ci(=fmt+clippy+test)  clean
#   gd=gen-data  r=repl  dl=pull-data  pc=pull-config  pl=pull-log(pla=全部)
#
# 部署配置 scripts/deploy.local（SSH 推送到 Windows 实测机）:
#   WIND_REMOTE              = user@host             # SSH 目标
#   WIND_REMOTE_DIR_RELEASE  = C:/.../WindInput      # p1 全量/ pm* 推送目录
#   WIND_REMOTE_DIR_DEV    = C:/.../WindInputDev  # pd1 / pdm* 推送目录
#   WIND_DATA_DIR / WIND_LOCAL_DIR  = %APPDATA% / %LOCALAPPDATA%\<App>  # pull-config/log 用
#
# 数据目录说明：
#   data/           源文件（入库）：配置、五笔词库、主题等手工维护文件
#   .cache/         外部下载/生成（gitignore）：rime-frost、opencc、unigram、tsf-obj 等
#   build/ build_dev/  全构建产物（gitignore）；内容即安装到 Program Files 的内容
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
# C++ TSF 核心层（clang/MSVC 交叉编译，见 wind_tsf/Makefile）
TSF_DIR="$PRODUCT_ROOT/wind_tsf"
SETTING_DIR="$(cd "$PRODUCT_ROOT/.." && pwd)/wind-setting"
PORTABLE_DIR="$(cd "$PRODUCT_ROOT/.." && pwd)/wind-portable"
VERSION="$(tr -d '[:space:]' < "$PRODUCT_ROOT/docs/VERSION" 2>/dev/null || echo '?')"
# 发布产物目录在【项目根】（内容 == 安装到 Program Files 的内容，无中间产物）
BUILD_DIR="$PRODUCT_ROOT/build"
BUILD_DEV_DIR="$PRODUCT_ROOT/build_dev"
# 外部下载/生成的词库缓存目录（不入库）
CACHE_DIR="$PRODUCT_ROOT/.cache"
# Rust 工具链根目录（wind_input/ workspace）
RUST_WORKSPACE="$PRODUCT_ROOT/wind_input"

# 远程 Windows 测试机配置（SSH）。在 scripts/deploy.local 或环境变量中设置：
#   WIND_REMOTE              = user@host          （SSH 目标）
#   WIND_REMOTE_DIR_RELEASE  = release 安装目录    （p1/pm* 推送目标；scp 正斜杠风格，
#                              如 'C:/Users/me/AppData/Local/Programs/WindInput'）
#   WIND_REMOTE_DIR_DEV    = dev 安装目录      （pd1/pdm* 推送目标，如 .../WindInputDev）
[ -f "$SCRIPT_DIR/deploy.local" ] && . "$SCRIPT_DIR/deploy.local"
WIND_REMOTE="${WIND_REMOTE:-}"
WIND_REMOTE_DIR_RELEASE="${WIND_REMOTE_DIR_RELEASE:-}"
WIND_REMOTE_DIR_DEV="${WIND_REMOTE_DIR_DEV:-}"
WIND_REMOTE_DIR="${WIND_REMOTE_DIR:-}"   # 兼容旧配置：未设 _RELEASE 时作 release 回退
# 远程数据/本地目录（拉配置、拉日志用；见 deploy.local 注释）：
#   WIND_DATA_DIR   = %APPDATA%\<App>        含 config.toml（用户配置）
#   WIND_LOCAL_DIR  = %LOCALAPPDATA%\<App>   含 logs/（服务日志）、cache/
WIND_DATA_DIR="${WIND_DATA_DIR:-}"
WIND_LOCAL_DIR="${WIND_LOCAL_DIR:-}"
# 从远程拉取的配置/日志落地处（本地查看用，不入库）
REMOTE_PULL_DIR="$PRODUCT_ROOT/.remote"
REMOTE_DIR=""   # 由 resolve_remote_dir 按 profile 填充

# Rust 交叉编译目标:MSVC(经 cargo-xwin 在 Linux 上交叉编,tier-1 目标)。
# C++ TSF DLL 也走 MSVC(clang + xwin SDK,见 wind_tsf/Makefile),整链统一。
TARGET="x86_64-pc-windows-msvc"

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

# ---------- cargo-xwin / clang (统一 MSVC 交叉编译工具链) ----------
# Rust/Tauri 经 cargo-xwin、C++ TSF 经 clang+llvm-rc,均交叉编 *-pc-windows-msvc,
# 共用 cargo-xwin 下载的 MSVC CRT/Windows SDK(缓存于 ~/.cache/cargo-xwin)。
# 统一用带版本号的 clang(MSVC STL 要求 clang≥19);可用 WIND_LLVM_VER 切到 20。
# 依赖:cargo-xwin、clang-<ver>、lld-<ver>、llvm-<ver>(含 llvm-rc/llvm-lib)。
XWIN_BIN="$HOME/.local/xwin-bin"
WIND_LLVM_VER="${WIND_LLVM_VER:-19}"
setup_xwin_env() {
    local v="$WIND_LLVM_VER"
    if ! command -v "clang-$v" >/dev/null 2>&1; then
        err "未找到 clang-$v;请安装 clang-$v lld-$v llvm-$v(MSVC STL 要求 clang≥19)。"
        return 1
    fi
    if ! command -v cargo-xwin >/dev/null 2>&1; then
        err "未找到 cargo-xwin;请运行 'cargo install cargo-xwin'。"; return 1
    fi
    # 软链桥:cargo-xwin 按无版本名搜索 clang-cl/lld-link/llvm-rc/llvm-lib/llvm-dlltool,
    # 全部指向 -<v>,使整条链(Rust/Tauri + C 依赖 + 链接)统一走 clang-<v>。幂等。
    mkdir -p "$XWIN_BIN"
    ln -sf "$(command -v clang-$v)"          "$XWIN_BIN/clang-cl"
    ln -sf "$(command -v clang-$v)"          "$XWIN_BIN/clang"
    ln -sf "$(command -v clang++-$v)"        "$XWIN_BIN/clang++"
    ln -sf "$(command -v lld-link-$v)"       "$XWIN_BIN/lld-link"     2>/dev/null || true
    ln -sf "$(command -v llvm-rc-$v)"        "$XWIN_BIN/llvm-rc"      2>/dev/null || true
    ln -sf "$(command -v llvm-lib-$v)"       "$XWIN_BIN/llvm-lib"     2>/dev/null || true
    ln -sf "$(command -v llvm-dlltool-$v)"   "$XWIN_BIN/llvm-dlltool" 2>/dev/null || true
    case ":$PATH:" in *":$XWIN_BIN:"*) ;; *) export PATH="$XWIN_BIN:$PATH";; esac
    export XWIN_ACCEPT_LICENSE="${XWIN_ACCEPT_LICENSE:-1}"  # 接受微软 SDK/CRT 许可
    export XWIN_ARCH="${XWIN_ARCH:-x86,x86_64}"             # 同时 splat 32/64 位(x86 TSF 需要)
}

# 统一 MSVC 构建入口:确保工具链就绪,并注入 +crt-static(静态链 MSVC 运行时,
# 产物自包含,无需目标机装 VC++ 运行库)。RUSTFLAGS 仅作用于此次 cargo-xwin 调用,
# 不污染本机 host 工具(gen_unigram 等 cargo run)的构建。
cargo_xwin() {
    setup_xwin_env || return 1
    RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" cargo xwin "$@"
}

# ---------- 构建（单模块 + 全构建）----------
# 输出目录：release → BUILD_DIR；dev → BUILD_DEV_DIR。
out_for() { [ "${1:-release}" = dev ] && echo "$BUILD_DEV_DIR" || echo "$BUILD_DIR"; }

# 模块一：wind_input 核心 exe。
# dev 变体 = dev-variant profile（继承 dev 优化 + 关 debug_assertions）；源码与 release 完全一致：
#   ① debug_assertions 关闭 → windows_subsystem="windows" 生效，无控制台窗口；
#   ② opt-level=1 快编译且手感够用；③ 独立 _dev 身份(管道/目录隔离, 运行期按 exe 名探测)。
build_core() {
    local profile="${1:-release}" outdir="${2:-$(out_for "$1")}"
    mkdir -p "$outdir"; cd "$PROJECT_ROOT" || return 1
    local suffix="" prof="release"
    [ "$profile" = dev ] && { prof="dev-variant"; suffix="_dev"; }
    say "\n[core] 交叉编译 wind_input ($prof, $TARGET)..."
    cargo_xwin build --profile "$prof" --target "$TARGET" -p wind_service \
        || { err "wind_input 构建失败!"; return 1; }
    local src="$PROJECT_ROOT/target/$TARGET/$prof/wind_input.exe"
    [ -f "$src" ] || { err "未找到产物: $src"; return 1; }
    cp -f "$src" "$outdir/wind_input${suffix}.exe"
    gray "已构建: wind_input${suffix}.exe ($(du -h "$outdir/wind_input${suffix}.exe" | cut -f1))"
    # CLI 包装器 (wind_input config ...; 运行时自辨 dev/release exe, 两变体共用一份)
    [ -f "$PROJECT_ROOT/scripts/wind_cli.bat" ] && cp -f "$PROJECT_ROOT/scripts/wind_cli.bat" "$outdir/wind_cli.bat" && gray "已复制: wind_cli.bat"
}

# 模块二：wind-setting 设置程序。
# 独立兄弟仓库；本地开发缺仓库时跳过。GitHub Release workflow 会先强制 checkout，
# 无权限或缺产物会在 workflow/pack 阶段失败。
build_setting() {
    local profile="${1:-release}" outdir="${2:-$(out_for "$1")}"
    if [ ! -d "$SETTING_DIR" ]; then
        warn "../wind-setting 仓库不存在, 跳过设置程序。"
        return 0
    fi

    local suffix="" cargo_args=(build --target "$TARGET") target_dir="debug"
    if [ "$profile" != dev ]; then
        cargo_args+=(--release)
        target_dir="release"
    else
        suffix="_dev"
    fi

    mkdir -p "$outdir"; cd "$SETTING_DIR" || return 1
    say "\n[setting] 交叉编译 wind_setting ($profile, $TARGET)..."
    export WIND_APP_VERSION="$VERSION"   # 版本注入: docs/VERSION → wind-setting (与主仓统一)
    cargo_xwin "${cargo_args[@]}" || { err "wind_setting 构建失败!"; return 1; }

    local src="$SETTING_DIR/target/$TARGET/$target_dir/wind_setting.exe"
    [ -f "$src" ] || { err "未找到产物: $src"; return 1; }
    cp -f "$src" "$outdir/wind_setting${suffix}.exe"
    gray "已构建: wind_setting${suffix}.exe ($(du -h "$outdir/wind_setting${suffix}.exe" | cut -f1))"
}

# 模块三：wind-portable 便携启动器。
# 独立兄弟仓库；dev/release 均产出同一份 release 二进制。
build_portable() {
    local profile="${1:-release}" outdir="${2:-$(out_for "$1")}"
    if [ ! -d "$PORTABLE_DIR" ]; then
        warn "../wind-portable 仓库不存在, 跳过便携启动器。"
        return 0
    fi

    mkdir -p "$outdir"; cd "$PORTABLE_DIR" || return 1
    say "\n[portable] 交叉编译 wind_portable ($profile → 单一二进制, $TARGET)..."
    export WIND_APP_VERSION="$VERSION"   # 版本注入: docs/VERSION → wind-portable (与主仓统一)
    cargo_xwin build --release --target "$TARGET" || { err "wind_portable 构建失败!"; return 1; }

    local src="$PORTABLE_DIR/target/$TARGET/release/wind_portable.exe"
    [ -f "$src" ] || { err "未找到产物: $src"; return 1; }
    cp -f "$src" "$outdir/wind_portable.exe"
    gray "已构建: wind_portable.exe ($(du -h "$outdir/wind_portable.exe" | cut -f1))"
}

do_check() {
    say "\n正在运行 cargo check ($TARGET, 全工作区)..."
    cd "$PROJECT_ROOT" && cargo_xwin check --target "$TARGET" --workspace
}

do_clippy() {
    say "\n正在运行 cargo clippy ($TARGET, 全工作区)..."
    # 注:暂不加 -D warnings(现存 ~36 个 warning 待并发会话稳定后单独清理)
    cd "$PROJECT_ROOT" && cargo_xwin clippy --target "$TARGET" --workspace
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

# 模块四：C++ TSF DLL（x64 + x86；clang/MSVC 交叉编译）。
# obj 中间产物落 .cache，保持 outdir 干净（== 安装内容）。dev → _dev 后缀。
build_tsf_all() {
    local profile="${1:-release}" outdir="${2:-$(out_for "$1")}"
    mkdir -p "$outdir"
    if ! command -v "clang++-$WIND_LLVM_VER" >/dev/null 2>&1; then
        warn "未找到 clang++-$WIND_LLVM_VER（C++ TSF 需 clang≥19）；跳过 TSF。"
        gray "  安装 clang-$WIND_LLVM_VER lld-$WIND_LLVM_VER llvm-$WIND_LLVM_VER 后可构建。"
        return 0
    fi
    if [ ! -d "$HOME/.cache/cargo-xwin/xwin/sdk" ]; then
        warn "未找到 MSVC SDK 缓存；请先跑一次完整构建（cargo-xwin 会下载 SDK）。跳过 TSF。"
        return 0
    fi
    local dv=0; [ "$profile" = dev ] && dv=1
    local objbase="$CACHE_DIR/tsf-obj"
    say "\n[tsf] 交叉编译 x64 + x86 ($profile, clang-$WIND_LLVM_VER/MSVC)..."
    local a objsfx; [ "$dv" = 1 ] && objsfx="d" || objsfx=""
    for a in x64 x86; do
        make -C "$TSF_DIR" ARCH="$a" DEV_VARIANT="$dv" VERSION="$VERSION" OUTDIR="$outdir" \
             OBJDIR="$objbase/$a$objsfx" \
             CLANG="clang++-$WIND_LLVM_VER" LLVM_RC="llvm-rc-$WIND_LLVM_VER" >/dev/null \
          || { err "TSF $a 构建失败！见 'make -C $TSF_DIR ARCH=$a' 输出。"; return 1; }
    done
    gray "已构建: $(cd "$outdir" && ls wind_tsf*.dll 2>/dev/null | tr '\n' ' ')"
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

    local pinyin_data="$CACHE_DIR/pinyin-data"
    mkdir -p "$pinyin_data"
    local PINYIN_BASE="https://raw.githubusercontent.com/mozillazg/pinyin-data/master"
    gray "pinyin-data (汉字拼音反查):"
    download_file "$PINYIN_BASE/kXHC1983.txt"       "$pinyin_data/kXHC1983.txt"       "新华字典多音字"
    download_file "$PINYIN_BASE/kTGHZ2013.txt"      "$pinyin_data/kTGHZ2013.txt"      "通用规范汉字"
    download_file "$PINYIN_BASE/kMandarin_8105.txt" "$pinyin_data/kMandarin_8105.txt" "8105 标准首音"
    download_file "$PINYIN_BASE/overwrite.txt"      "$pinyin_data/overwrite.txt"      "手工纠正"

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
    local outdir="${1:-$BUILD_DEV_DIR}"
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

    # 1b. 合并 wind_input/data/settings/（manifest.toml 等 RPC 元数据）。
    # wind-rpc 运行时优先读 data_dir()/settings/manifest.toml；
    # 该文件不在 PRODUCT_ROOT/data/ 故需单独合并，否则 Windows 部署端缺失/过期。
    if [ -d "$PROJECT_ROOT/data/settings" ]; then
        mkdir -p "$data/settings"
        cp -rf "$PROJECT_ROOT/data/settings/." "$data/settings/"
    fi

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

    # 4b. 汉字拼音反查表（候选拼音提示/拼音方案自动出码）
    local pinyin_map_cache="$CACHE_DIR/pinyin-data/pinyin_map.txt"
    if [ -f "$pinyin_map_cache" ]; then
        cp -f "$pinyin_map_cache" "$data/pinyin_map.txt"
    else
        warn "缺 pinyin_map.txt（运行 gen-data 生成）"
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

# ---------- 实测 / 远程部署（SSH）----------

# 本机跑候选 REPL。data 目录优先 build_dev/data/，其次 .cache/pulled-data/。
do_repl() {
    local data="${1:-}"
    if [ -z "$data" ]; then
        if [ -f "$BUILD_DEV_DIR/data/schemas/pinyin/unigram.txt" ]; then
            data="$BUILD_DEV_DIR/data"
        elif [ -d "$CACHE_DIR/pulled-data" ]; then
            data="$CACHE_DIR/pulled-data"
            gray "使用 pull-data 拉取的词库: $data"
        else
            warn "未找到词库数据；请先运行 gen-data 或 pull-data"
            data="$BUILD_DEV_DIR/data"
        fi
    fi
    say "\n启动候选 REPL (data=$data)..."
    cd "$PROJECT_ROOT" && WIND_DATA="$data" cargo run --release -p wind-repl -- "$data"
}

require_remote() {
    if [ -z "$WIND_REMOTE" ]; then
        err "未配置 WIND_REMOTE：请在 $SCRIPT_DIR/deploy.local 设置 SSH 目标"
        echo "  示例: WIND_REMOTE=me@192.168.5.30"
        return 1
    fi
}

# 解析远端安装目录（按 profile）。结果写入全局 REMOTE_DIR（去尾斜杠避免 // ）。
#   release → WIND_REMOTE_DIR_RELEASE（兼容旧 WIND_REMOTE_DIR）；dev → WIND_REMOTE_DIR_DEV
resolve_remote_dir() {
    local profile="${1:-release}"
    if [ "$profile" = dev ]; then
        REMOTE_DIR="${WIND_REMOTE_DIR_DEV:-}"
        [ -n "$REMOTE_DIR" ] || { err "未配置 WIND_REMOTE_DIR_DEV（deploy.local）"; return 1; }
    else
        REMOTE_DIR="${WIND_REMOTE_DIR_RELEASE:-${WIND_REMOTE_DIR:-}}"
        [ -n "$REMOTE_DIR" ] || { err "未配置 WIND_REMOTE_DIR_RELEASE（deploy.local）"; return 1; }
    fi
    REMOTE_DIR="${REMOTE_DIR%/}"
}

# 在远端跑 PowerShell：脚本经 UTF-16LE+base64 编码传入，彻底避开 bash/ssh/cmd 多层引号。
remote_ps() {
    command -v iconv >/dev/null 2>&1 || { err "需要 iconv（编码远端 PowerShell 脚本）"; return 1; }
    local b64; b64="$(printf '%s' "$1" | iconv -t UTF-16LE | base64 | tr -d '\n')"
    ssh "$WIND_REMOTE" "powershell -NoProfile -EncodedCommand $b64"
}

# 本 profile 的二进制基名（exe/dll）。data/ 不在此（不会被锁，直接 scp 覆盖）。
bins_for() {
    local sfx=""; [ "$1" = dev ] && sfx="_dev"
    printf '%s\n' "wind_input${sfx}.exe" "wind_tsf${sfx}.dll" "wind_tsf${sfx}_x86.dll"
}

# 把 bash 列表转成 PowerShell 字符串数组字面量： a b → 'a','b'
ps_list() { local out="" x; for x in "$@"; do out="$out${out:+,}'$x'"; done; printf '%s' "$out"; }

# 终止远端进程（按 profile 决定 _dev 后缀；mod 限定只杀该模块的进程）。
remote_taskkill() {
    local profile="$1" mod="${2:-}" sfx=""
    [ "$profile" = dev ] && sfx="_dev"
    local procs=()
    case "$mod" in
        core)    procs=("wind_input${sfx}.exe") ;;
        tsf|"")  procs=("wind_input${sfx}.exe") ;;  # 改 DLL 也需停宿主
    esac
    local p
    for p in "${procs[@]}"; do
        ssh "$WIND_REMOTE" "taskkill /F /IM $p" >/dev/null 2>&1 || true
    done
    sleep 1
}

# 把加载中的旧二进制改名让路（已加载的 DLL/EXE 可改名、不可覆盖）。$@ = 基名列表。
remote_rename_aside() {
    local arr; arr="$(ps_list "$@")"
    remote_ps "\$ErrorActionPreference='SilentlyContinue'; \$d='$REMOTE_DIR'; \
foreach(\$n in @($arr)){ \$p=Join-Path \$d \$n; if(Test-Path \$p){ Rename-Item \$p (\$n+'.old_'+(Get-Random)) -Force } }" >/dev/null 2>&1 || true
}

# 启动远端主进程（避免等 TSF 被动加载）。
# 注意：经 SSH 直接 Start-Process 的子进程会随 SSH 断开被 Job Object 连带杀掉
# （症状：部署后看不到进程）。改用计划任务(schtasks)在用户交互会话拉起，脱离 SSH 生命周期。
remote_start_main() {
    local profile="$1" sfx=""; [ "$profile" = dev ] && sfx="_dev"
    local exe="$REMOTE_DIR/wind_input${sfx}.exe"
    say "启动远端主进程 wind_input${sfx}.exe (计划任务,脱离 SSH 会话)..."
    # 用 ScheduledTasks cmdlet 在用户交互会话(session 1)拉起：进程脱离 SSH 的
    # Job Object，SSH 断开后仍存活；路径作普通字符串传入，无 cmd 引号困扰。
    remote_ps "\$ErrorActionPreference='SilentlyContinue'; \
\$exe='$exe'.Replace('/','\\'); \$wd='$REMOTE_DIR'.Replace('/','\\'); \
\$a=New-ScheduledTaskAction -Execute \$exe -WorkingDirectory \$wd; \
Register-ScheduledTask -TaskName 'WindInputDeployBoot' -Action \$a -Force | Out-Null; \
Start-ScheduledTask -TaskName 'WindInputDeployBoot'; Start-Sleep -Seconds 2; \
Unregister-ScheduledTask -TaskName 'WindInputDeployBoot' -Confirm:\$false" >/dev/null 2>&1 || true
    sleep 2
    if ssh "$WIND_REMOTE" "tasklist /FI \"IMAGENAME eq wind_input${sfx}.exe\" /NH" 2>/dev/null | grep -qi "wind_input${sfx}.exe"; then
        say "主进程已启动并存活。"
    else
        warn "未检测到主进程存活（可能被单例/任务策略挡住）；可在 Windows 手动启动，或开始输入由 TSF 拉起。"
    fi
}

# 清理历史改名残留 .old_*（仍被占用的会自动跳过，下次部署再清）。
remote_cleanup_old() {
    remote_ps "Get-ChildItem -Path '$REMOTE_DIR' -Filter '*.old_*' -EA SilentlyContinue | Remove-Item -Force -EA SilentlyContinue" >/dev/null 2>&1 || true
}

# 全量 push：整个 build[_dev]/ → 远端安装目录（先改名锁定二进制让路，再 scp 覆盖，最后起主进程）。
#   p1 / pd1
do_push_full() {
    local profile="${1:-release}"
    require_remote || return 1
    resolve_remote_dir "$profile" || return 1
    local outdir; outdir="$(out_for "$profile")"
    [ -d "$outdir" ] || { err "无 $outdir；请先 '$([ "$profile" = dev ] && echo d1 || echo 1)' 全构建。"; return 1; }
    say "\n停止远端进程（$profile）..."
    remote_taskkill "$profile"
    ssh "$WIND_REMOTE" "if not exist \"${REMOTE_DIR//\//\\}\" mkdir \"${REMOTE_DIR//\//\\}\"" >/dev/null 2>&1 || true
    local bins; mapfile -t bins < <(bins_for "$profile")
    say "改名让路（加载中的 DLL/EXE）..."
    remote_rename_aside "${bins[@]}"
    say "全量推送 $outdir/ → $WIND_REMOTE:$REMOTE_DIR/"
    if scp -r "$outdir"/* "$WIND_REMOTE:$REMOTE_DIR/"; then
        remote_start_main "$profile"
        remote_cleanup_old
        say "已全量部署并启动（$profile）。"
    else
        err "scp 失败：检查 $([ "$profile" = dev ] && echo WIND_REMOTE_DIR_DEV || echo WIND_REMOTE_DIR_RELEASE) 路径(正斜杠)、SSH、磁盘。"
        return 1
    fi
}

# 单模块 push：只推对应文件（不重编，用现有 build[_dev]/ 产物）。
#   pm1=tsf  pm2=core （pd 前缀 = dev）
do_push_module() {
    local profile="${1:-release}" mod="$2"
    require_remote || return 1
    resolve_remote_dir "$profile" || return 1
    local outdir; outdir="$(out_for "$profile")"
    local sfx=""; [ "$profile" = dev ] && sfx="_dev"
    local files=()
    case "$mod" in
        tsf)     files=("wind_tsf${sfx}.dll" "wind_tsf${sfx}_x86.dll") ;;
        core)    files=("wind_input${sfx}.exe")
                 [ -f "$outdir/wind_cli.bat" ] && files+=("wind_cli.bat") ;;  # CLI 包装器随核心
        *)       err "未知模块: $mod（tsf|core）"; return 1 ;;
    esac
    local f
    for f in "${files[@]}"; do
        [ -f "$outdir/$f" ] || { err "本地无 $outdir/$f（先构建对应模块）"; return 1; }
    done
    say "\n停止远端进程（$profile/$mod）..."
    remote_taskkill "$profile" "$mod"
    say "改名让路 + 推送..."
    remote_rename_aside "${files[@]}"
    local ok=1
    for f in "${files[@]}"; do
        say "推送 $f → $REMOTE_DIR/"
        scp "$outdir/$f" "$WIND_REMOTE:$REMOTE_DIR/$f" || { err "scp $f 失败"; ok=0; }
    done
    if [ "$ok" = 1 ]; then
        # 推了核心/TSF 则重启主进程让其立即生效
        case "$mod" in core|tsf) remote_start_main "$profile" ;; esac
        remote_cleanup_old
        say "模块部署完成（$profile/$mod）。"
    fi
}

# 从 Windows 安装目录拉取已处理的 data/（含真实词库）到 .cache/pulled-data/ 供 REPL 使用。
do_pull_data() {
    require_remote || return 1
    resolve_remote_dir "${1:-release}" || return 1
    local dst="$CACHE_DIR/pulled-data"
    say "\n拉取 data/ ← $WIND_REMOTE:$REMOTE_DIR/data  →  $dst"
    rm -rf "$dst"
    mkdir -p "$CACHE_DIR"
    if scp -r "$WIND_REMOTE:$REMOTE_DIR/data" "$dst"; then
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
        echo "  示例: WIND_DATA_DIR='C:/Users/me/AppData/Roaming/WindInputDev'"
        echo "        WIND_LOCAL_DIR='C:/Users/me/AppData/Local/WindInputDev'"
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

# 下载外部词库到 .cache/ + 生成 unigram + 组装 build_dev/data/
do_gen_data() {
    local outdir="${1:-$BUILD_DEV_DIR}"
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

    # 生成汉字拼音反查表（Rust 工具 gen_pinyin）
    local pinyin_map_cache="$CACHE_DIR/pinyin-data/pinyin_map.txt"
    if [ -f "$CACHE_DIR/pinyin-data/kMandarin_8105.txt" ]; then
        say "生成汉字拼音反查表..."
        ( cd "$RUST_WORKSPACE" && cargo run -q --bin gen_pinyin -- \
            --src "$CACHE_DIR/pinyin-data" \
            --out "$pinyin_map_cache" ) \
            || warn "拼音反查表生成失败（候选拼音提示不可用）"
    else
        warn "缺 .cache/pinyin-data/，拼音反查表不可用"
    fi

    assemble_data "$outdir"
    say "gen-data 完成 → $outdir/data"
}

# 发布前硬门禁:校验关键运行时数据完整。assemble_data 对缺失项仅 warn(交互式
# 部分构建可容忍),但 dist(发版)必须完整——任一关键文件缺失/过小即失败,杜绝
# 发出词库残缺(无智能组句/无简繁/词库不全)的安装器。
verify_dist_data() {
    local data="${1:-$BUILD_DIR}/data"
    local ok=1
    # "相对 data/ 的路径|最小字节数"(下限粗略,仅为捕获缺失/0 字节/截断)
    local checks=(
        "schemas/pinyin/unigram.txt|1000000"
        "schemas/pinyin/cn_dicts/base.dict.yaml|1000000"
        "schemas/pinyin/cn_dicts/8105.dict.yaml|10000"
        "schemas/english/en.dict.yaml|1000"
        "pinyin_map.txt|10000"
    )
    say "\n校验发布数据完整性 → $data"
    local entry path min sz
    for entry in "${checks[@]}"; do
        path="${entry%%|*}"; min="${entry##*|}"
        if [ ! -f "$data/$path" ]; then
            err "  ✗ 缺失: $path"; ok=0; continue
        fi
        sz=$(stat -c%s "$data/$path" 2>/dev/null || echo 0)
        if [ "$sz" -lt "$min" ]; then
            err "  ✗ 过小(${sz}B < 期望 ${min}B,疑似下载/生成失败): $path"; ok=0
        else
            gray "  ✓ $path ($(numfmt --to=iec "$sz" 2>/dev/null || echo "${sz}B"))"
        fi
    done
    # OpenCC:至少一个非空 .octrie(简繁转换)
    local octrie_cnt
    octrie_cnt=$(find "$data/opencc" -name '*.octrie' -size +0c 2>/dev/null | wc -l)
    if [ "$octrie_cnt" -lt 1 ]; then
        err "  ✗ 缺失: opencc/*.octrie(简繁转换编译失败)"; ok=0
    else
        gray "  ✓ opencc/*.octrie ($octrie_cnt 个)"
    fi

    if [ "$ok" -ne 1 ]; then
        err "\n发布数据校验失败!上述文件缺失或异常会导致安装器功能残缺。"
        err "请排查 gen-data 的下载/生成(词库源、网络、gen_unigram/gen_opencc)。"
        return 1
    fi
    say "发布数据校验通过 ✓"
}

# ---------- 全构建（1 / d1）----------
# 全部模块 + 数据落到【项目根】build/(release) 或 build_dev/(dev)。
# 先清空输出目录，确保内容 == 安装到 Program Files 的内容，无任何中间产物。
#   do_full [release|dev]
# 版本变化侦测: 版本号变更时强制重建关键产物 (确定性保险)。
# 产品版本唯一真源是 docs/VERSION; cargo 的 rerun-if-env-changed 已能自动重建, 此处
# 再加保险: 记录上次构建版本, 一旦变化即清理最终产物 (Rust 最终包 + TSF obj),
# 强制重新写入版本资源。仅版本真变时付代价。
sync_version_stamp() {
    local stamp="$CACHE_DIR/.last_build_version" last=""
    [ -f "$stamp" ] && last="$(tr -d '[:space:]' < "$stamp")"
    [ "$last" = "$VERSION" ] && return 0   # 版本未变 → 走增量, 不清理

    if [ -n "$last" ]; then say "\n[version] 版本变化 $last -> $VERSION, 清理关键产物强制刷新版本号..."
    else say "\n[version] 首次记录版本 $VERSION, 清理关键产物确保版本号写入..."; fi

    # Rust: 仅清最终二进制包 (依赖库保留); build.rs 随之重跑注入新版本资源。
    ( cd "$PRODUCT_ROOT/wind_input" && cargo clean -p wind_service >/dev/null 2>&1 ) || true
    if [ -d "$SETTING_DIR" ]; then
        ( cd "$SETTING_DIR" && cargo clean -p wind_setting >/dev/null 2>&1 ) || true
    fi
    if [ -d "$PORTABLE_DIR" ]; then
        ( cd "$PORTABLE_DIR" && cargo clean -p wind_portable >/dev/null 2>&1 ) || true
    fi
    # TSF: 删交叉编译对象目录, 强制重新生成含新版本的资源。
    rm -rf "$CACHE_DIR/tsf-obj"

    mkdir -p "$CACHE_DIR"
    printf '%s' "$VERSION" > "$stamp"
}

do_full() {
    local profile="${1:-release}" outdir; outdir="$(out_for "$profile")"
    say "\n========== 全构建 ($profile) → $outdir =========="
    sync_version_stamp   # 版本号变化则强制重建关键产物 (确定性保险)
    rm -rf "$outdir"; mkdir -p "$outdir"
    build_core    "$profile" "$outdir" || return 1   # wind_input[_dev].exe
    build_tsf_all "$profile" "$outdir" || return 1   # wind_tsf[_x86][_dev].dll
    build_setting "$profile" "$outdir" || return 1   # wind_setting[_dev].exe (可选)
    build_portable "$profile" "$outdir" || return 1  # wind_portable.exe (可选)
    do_gen_data   "$outdir"            || return 1   # data/(下载词库 + unigram/pinyin + opencc)
    verify_dist_data "$outdir"         || return 1   # 硬门禁:词库/模型完整
    say "\n========== 全构建完成 ($profile) → $outdir =========="
    gray "内容即安装到 Program Files 的内容（无中间产物）；打包: dev.sh installer"
}

# ---------- 一键生成安装包（8 / 8s）----------
# do_full release → pack-installer.sh 出自解压 Setup.exe + sha256。
#   installer        完整重建 + 打包（对应 Go dev.ps1 的 8）
#   installer skip   跳过重建，直接打包现有 build/（对应 8s）
do_installer() {
    local skip="${1:-}"
    if [ "$skip" = "skip" ]; then
        say "\n跳过构建，直接打包现有 $BUILD_DIR/"
        [ -f "$BUILD_DIR/wind_input.exe" ] || {
            err "build/ 无产物；请先运行 'dev.sh installer'（不带 skip）或 'dev.sh 1'。"; return 1; }
    else
        do_full release || return 1
    fi
    say "\n=== 打包安装程序 ==="
    "$SCRIPT_DIR/pack-installer.sh" --version "$VERSION" || return 1
}

show_menu() {
    clear 2>/dev/null || true
    printf '%b============================================%b\n' "$C_CYAN" "$C_RESET"
    printf '%b  WindInput 开发菜单  v%s  (Linux→Win, MSVC)%b\n' "$C_CYAN" "$VERSION" "$C_RESET"
    printf '%b============================================%b\n\n' "$C_CYAN" "$C_RESET"
    printf '%b  全构建 (→ 项目根 build/，内容 == 安装到 Program Files):%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    1    Release 全构建: wind_input + tsf(x64/x86) + setting + portable + 词库数据"
    echo  "    d1   Debug 全构建 (→ build_dev/)"
    printf '\n%b  单模块构建 (前缀 d = dev):%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    m1   仅 tsf (x64+x86)        dm1"
    echo  "    m2   仅 wind_input (核心)     dm2"
    echo  "    m3   仅 wind_setting (../wind-setting)    dm3"
    echo  "    m4   仅 wind_portable (../wind-portable)  dm4"
    printf '\n%b  安装包:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    8    生成安装包 (= 1 + 打包 → Setup.exe + sha256)"
    echo  "    8s   跳过编译, 直接打包现有 build/"
    printf '\n%b  部署 → Windows (deploy.local 配 RELEASE/DEV 路径; SSH → %s):%b\n' "$C_YELLOW" "${WIND_REMOTE:-未配置}" "$C_RESET"
    echo  "    p1   push 全部 (release)        pd1   push 全部 (dev)"
    echo  "    pm1/pm2  push 模块(tsf/核心)    pdm1/pdm2 (dev)"
    printf '\n%b  代码质量:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    k=check  l=clippy  t=test  f=fmt  ci=fmt+clippy+test"
    printf '\n%b  远程数据 / 实测:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    r=repl(本机)  dl=pull-data  pc=pull-config  pl=pull-log(pla=全部)"
    printf '\n%b  杂项:%b\n' "$C_YELLOW" "$C_RESET"
    echo  "    gd=gen-data  clean  q=退出"
    printf '%b============================================%b\n' "$C_CYAN" "$C_RESET"
}

pause() { printf '\n'; read -e -r -p "按回车继续..." _; }

# 统一分发：菜单与命令行直调共用，命令已转小写。返回 1 表示无效命令。
dispatch() {
    case "$1" in
        1|release)        do_full release ;;
        d1|dev)         do_full dev ;;
        m1)               build_tsf_all release ;;
        dm1)              build_tsf_all dev ;;
        m2)               build_core release ;;
        dm2)              build_core dev ;;
        m3)               build_setting release ;;
        dm3)              build_setting dev ;;
        m4)               build_portable release ;;
        dm4)              build_portable dev ;;
        8|installer|pack) do_installer ;;
        8s|installer-skip) do_installer skip ;;
        p1)               do_push_full release ;;
        pd1)              do_push_full dev ;;
        pm1)              do_push_module release tsf ;;
        pm2)              do_push_module release core ;;
        pdm1)             do_push_module dev tsf ;;
        pdm2)             do_push_module dev core ;;
        k|check)          do_check ;;
        l|clippy)         do_clippy ;;
        t|test)           do_test ;;
        f|fmt)            do_fmt ;;
        fmt-check)        do_fmt_check ;;
        ci)               do_ci ;;
        clean)            do_clean ;;
        gd|gen-data)      do_gen_data ;;
        r|repl)           do_repl "${2:-}" ;;
        dl|pull-data)     do_pull_data "${2:-}" ;;
        pc|pull-config)   do_pull_config ;;
        pl|pull-log)      do_pull_log "${2:-}" ;;
        pla)              do_pull_log all ;;
        *)                return 127 ;;   # 哨兵:未知命令（区别于命令执行失败的非 0 返回）
    esac
}

menu_loop() {
    set -o history 2>/dev/null || true
    while true; do
        show_menu
        printf '\n'
        read -e -r -p "请输入选项: " choice
        [ -n "$choice" ] && history -s "$choice"
        choice="$(printf '%s' "$choice" | tr '[:upper:]' '[:lower:]')"
        case "$choice" in
            q) exit 0 ;;
            "") ;;
            *)
                dispatch "$choice"; local rc=$?
                if [ "$rc" -eq 127 ]; then
                    err "无效选项: $choice"; sleep 1     # 未知命令:短暂提示后刷新菜单
                else
                    [ "$rc" -ne 0 ] && err "\n命令 '$choice' 失败 (退出码 $rc)"
                    pause                                # 已知命令:无论成败都停下，让你看到输出/错误
                fi
                ;;
        esac
    done
}

# ---------- 命令行直调 ----------
# 与菜单同一套命令（如 './dev.sh 1'、'./dev.sh p1'、'./dev.sh m2'）；命令转小写以容错。
cmd="$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')"
case "$cmd" in
    ""|menu) menu_loop ;;
    -h|--help|help)
        grep '^#' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        ;;
    *)
        dispatch "$cmd" "${2:-}"; rc=$?
        if [ "$rc" -eq 127 ]; then
            err "未知命令: $1"
            echo "运行 './scripts/dev.sh --help' 查看可用命令"
            exit 1
        fi
        exit "$rc"   # 透传命令真实退出码（CLI/CI 用）
        ;;
esac
