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
TARGET="x86_64-pc-windows-gnu"

# ---- 默认值 ----
VERSION="${WIND_VERSION:-}"
COMPRESSION="${WIND_COMPRESSION:-zstd}"
BUILD_DIR="${WIND_BUILD_DIR:-$PRODUCT_ROOT/wind_input/build}"
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

# ---- 编译安装器 stub(交叉)+ packer(原生)----
echo ">>> 交叉编译 wind-installer stub + 原生 wind-packer ..."
(
  cd "$INSTALLER_DIR"
  cargo build --release --target "$TARGET" --bin wind-installer --bin wind-uninstaller
  cargo build --release --bin wind-packer
)

STUB="$INSTALLER_DIR/target/$TARGET/release/wind-installer.exe"
UNINSTALLER="$INSTALLER_DIR/target/$TARGET/release/wind-uninstaller.exe"
PACKER="$INSTALLER_DIR/target/release/wind-packer"
for b in "$STUB" "$UNINSTALLER" "$PACKER"; do
  [[ -f "$b" ]] || { echo "[ERROR] 缺少编译产物: $b" >&2; exit 1; }
done

mkdir -p "$DIST_DIR"

# ---- 注入卸载程序到产物目录(打包后移除,保持干净)----
UNINSTALL_DEST="$BUILD_DIR/uninstall.exe"
cp -f "$UNINSTALLER" "$UNINSTALL_DEST"
cleanup() { rm -f "$UNINSTALL_DEST"; }
trap cleanup EXIT

# ---- 阶段一:压缩打包 ----
echo ">>> 阶段一:pack(算法 $COMPRESSION)..."
"$PACKER" pack --source "$BUILD_DIR" --output "$ARCHIVE" --compression "$COMPRESSION"

# ---- 阶段二:拼接 stub + archive ----
echo ">>> 阶段二:bundle ..."
"$PACKER" bundle --stub "$STUB" --archive "$ARCHIVE" --output "$OUTPUT"

rm -f "$ARCHIVE"
cleanup; trap - EXIT

SIZE="$(du -h "$OUTPUT" | cut -f1)"
echo "================================================"
echo "  ✅ 打包完成: $OUTPUT ($SIZE)"
echo "================================================"
