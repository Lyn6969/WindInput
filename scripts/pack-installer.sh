#!/usr/bin/env bash
# ============================================================================
# pack-installer.sh — 把 WindInput 构建产物打包成自解压安装程序
# ----------------------------------------------------------------------------
# 全 Linux 流水线:交叉编译 wind-installer stub(x86_64-pc-windows-gnu)+ 原生
# 构建 wind-packer(纯 IO,无需 wine),再 pack→bundle 出单文件 Setup.exe。
#
# 前置:BUILD_DIR 内已含 wind_input.exe / wind_tsf.dll / wind_tsf_x86.dll / data/
#      (由 dev.sh release + build_tsf + assemble_data 产出)。
#
# 用法:
#   scripts/pack-installer.sh [--version X.Y.Z] [--compression zstd|lzma]
#                             [--build-dir DIR] [--installer-dir DIR]
#                             [--output FILE]
#
# 环境变量(等价覆盖,命令行参数优先):
#   WIND_VERSION / WIND_COMPRESSION / WIND_BUILD_DIR / WIND_INSTALLER_DIR
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PRODUCT_ROOT="$(dirname "$SCRIPT_DIR")"            # WindInput/
TARGET="x86_64-pc-windows-msvc"   # cargo-xwin 交叉编译 stub/uninstaller;packer 原生编

# ---- 默认值 ----
VERSION="${WIND_VERSION:-}"
COMPRESSION="${WIND_COMPRESSION:-zstd}"
BUILD_DIR="${WIND_BUILD_DIR:-$PRODUCT_ROOT/build}"
INSTALLER_DIR="${WIND_INSTALLER_DIR:-$PRODUCT_ROOT/../wind-installer}"
OUTPUT=""

# ---- 解析参数 ----
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)       VERSION="$2"; shift 2 ;;
    --compression)   COMPRESSION="$2"; shift 2 ;;
    --build-dir)     BUILD_DIR="$2"; shift 2 ;;
    --installer-dir) INSTALLER_DIR="$2"; shift 2 ;;
    --output)        OUTPUT="$2"; shift 2 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

# ---- 版本号:缺省取 docs/VERSION ----
if [[ -z "$VERSION" ]]; then
  if [[ -f "$PRODUCT_ROOT/docs/VERSION" ]]; then
    VERSION="$(tr -d '[:space:]' < "$PRODUCT_ROOT/docs/VERSION")"
  else
    VERSION="0.0.0-dev"
  fi
fi

DIST_DIR="$INSTALLER_DIR/dist"
[[ -z "$OUTPUT" ]] && OUTPUT="$DIST_DIR/WindInput-${VERSION}-Setup.exe"
ARCHIVE="$DIST_DIR/WindInput-${VERSION}.bin"

echo "================================================"
echo "  WindInput 安装程序打包"
echo "  版本:     $VERSION"
echo "  压缩:     $COMPRESSION"
echo "  产物目录: $BUILD_DIR"
echo "  安装器:   $INSTALLER_DIR"
echo "  输出:     $OUTPUT"
echo "================================================"

# ---- 校验构建产物 ----
required=(wind_input.exe wind_tsf.dll wind_tsf_x86.dll)
missing=()
for f in "${required[@]}"; do
  [[ -f "$BUILD_DIR/$f" ]] || missing+=("$f")
done
[[ -d "$BUILD_DIR/data" ]] || missing+=("data/")
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "[ERROR] BUILD_DIR 缺少产物: ${missing[*]}" >&2
  echo "        请先运行 dev.sh(release + build_tsf + build_tsf x86 + assemble_data)" >&2
  exit 1
fi
[[ -d "$INSTALLER_DIR" ]] || { echo "[ERROR] 找不到 wind-installer: $INSTALLER_DIR" >&2; exit 1; }

# ---- 编译安装器 stub(cargo-xwin/MSVC 交叉)+ packer(原生)----
echo ">>> 交叉编译 wind-installer stub (cargo-xwin/MSVC) + 原生 wind-packer ..."
# clang-cl 软链(cargo-xwin 需要;clang 改名调用,幂等)
XWIN_BIN="$HOME/.local/xwin-bin"
if ! command -v clang-cl >/dev/null 2>&1 && command -v clang >/dev/null 2>&1; then
  mkdir -p "$XWIN_BIN"; ln -sf "$(command -v clang)" "$XWIN_BIN/clang-cl"
  export PATH="$XWIN_BIN:$PATH"
fi
export XWIN_ACCEPT_LICENSE="${XWIN_ACCEPT_LICENSE:-1}"
(
  cd "$INSTALLER_DIR"
  # stub/uninstaller 交叉编 MSVC(+crt-static 自包含);packer 原生编(纯 IO,跑在主机)
  RUSTFLAGS="-C target-feature=+crt-static" \
    cargo xwin build --release --target "$TARGET" --bin wind-installer --bin wind-uninstaller
  cargo build --release --bin wind-packer
)

STUB="$INSTALLER_DIR/target/$TARGET/release/wind-installer.exe"
UNINSTALLER="$INSTALLER_DIR/target/$TARGET/release/wind-uninstaller.exe"
PACKER="$INSTALLER_DIR/target/release/wind-packer"
for b in "$STUB" "$UNINSTALLER" "$PACKER"; do
  [[ -f "$b" ]] || { echo "[ERROR] 缺少编译产物: $b" >&2; exit 1; }
done

mkdir -p "$DIST_DIR"

# ---- 组装干净 staging 目录(只含分发文件,排除 obj/ 等 TSF 中间产物)----
# pack --source 会递归打包整个目录,故不能直接打 BUILD_DIR(含 obj/)。
STAGE="$DIST_DIR/stage"
rm -rf "$STAGE"; mkdir -p "$STAGE"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

cp -f  "$BUILD_DIR/wind_input.exe"   "$STAGE/"
cp -f  "$BUILD_DIR/wind_tsf.dll"     "$STAGE/"
cp -f  "$BUILD_DIR/wind_tsf_x86.dll" "$STAGE/"
cp -rf "$BUILD_DIR/data"             "$STAGE/"
cp -f  "$UNINSTALLER"                "$STAGE/uninstall.exe"

# 跨平台限制：Linux 交叉编不出 Tauri Windows 包，故可选——存在即随包，缺失仅告警。
else
fi

# ---- 阶段一:压缩打包 ----
echo ">>> 阶段一:pack(算法 $COMPRESSION)..."
"$PACKER" pack --source "$STAGE" --output "$ARCHIVE" --compression "$COMPRESSION"

# ---- 阶段二:拼接 stub + archive ----
echo ">>> 阶段二:bundle ..."
"$PACKER" bundle --stub "$STUB" --archive "$ARCHIVE" --output "$OUTPUT"

rm -f "$ARCHIVE"
cleanup; trap - EXIT

SIZE="$(du -h --apparent-size "$OUTPUT" | cut -f1)"

# ---- 生成 SHA256 校验和(安装器未签名,供用户校验完整性;sha256sum -c 可校验)----
SHA_FILE="$OUTPUT.sha256"
( cd "$(dirname "$OUTPUT")" && sha256sum "$(basename "$OUTPUT")" > "$(basename "$SHA_FILE")" )

echo "================================================"
echo "  ✅ 打包完成: $OUTPUT ($SIZE)"
echo "  🔒 校验和:   $SHA_FILE"
echo "================================================"
