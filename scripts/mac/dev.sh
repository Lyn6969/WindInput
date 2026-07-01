#!/usr/bin/env bash
# WindInput macOS 开发一站式脚本 (原生构建, 对齐 Windows 的 scripts/dev.ps1)。
#
# 解决的痛点: 旧 app.sh / install_app.sh / install_service.sh 各管一段且默认不编译,
# 容易装上旧二进制 (service 才是渲染/定位/上屏的真身)。本脚本把「编译 + 安装」串成
# 一条命令, 一律先编再装, 杜绝旧二进制。原先散落的 app.sh / install_app.sh /
# install_service.sh / setup_signing.sh / pkg.sh 已全部内联进本脚本成为函数。
#
# 用法:
#   scripts/mac/dev.sh <命令> [--debug]
#
# 命令 (前缀缩写均可):
#   install | i      编译 + 安装 service(Rust) + app(Swift)  ← 改完代码就跑它
#   service | svc    编译 + 安装 service (改 Rust 渲染/协议/引擎时)
#   app     | a      编译 + 安装 app     (改 Swift 显示/IMKit 时)
#   build   | b      只编译两端 (service + .app bundle + codesign), 不安装
#   run     | r      重启 service (kickstart, 不重编)
#   logs    | l      跟踪 service + IME 日志
#   status  | st     诊断: service pid / socket / 签名 / 进程
#   data    | gd     把当前已装的 data/ 快照到 build_mac/data (作安装数据源)
#   uninstall | rm   卸载 service + app
#   sign-setup       命令行创建自签 Code Signing 证书 "WindInput Dev" (需 sudo + 钥匙串交互)
#                    可带子命令: sign-setup [create|check|grant|remove]
#   pkg              打 .pkg 安装器 (终端用户分发); pkg --build 先构建再打包
#
# 选项:
#   --debug          debug 变体 (WindInputDebug + target/debug + debug_variant 特性,
#                    与 release 变体可共存; 默认 release = WindInput)。
#   --data <dir>     指定词库数据源目录 (默认见下「数据解析」)。
#
# 数据解析顺序 (install_service 需要 data/): --data > build_mac/data > 当前已装的
#   service/data (自动快照到 build_mac/data) > 报错提示先组装数据。
# 完整词库 (重新下载/生成 unigram/opencc) 仍走 Linux 的 scripts/dev.sh gen-data;
# mac 日常开发数据稳定, 复用已装的即可。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUST_DIR="$REPO_DIR/wind_input"
MACOS_DIR="$REPO_DIR/wind_macos"
DATA_SNAPSHOT="$REPO_DIR/build_mac/data"

# 变体派生
VARIANT="release"          # release | debug
CARGO_PROFILE_FLAG="--release"
CARGO_FEATURES=()
APP_VARIANT_FLAG=()        # 传给内联的 app_build / install_* 函数
APP_SUPPORT="$HOME/Library/Application Support/WindInput"
LABEL="to.feng.windinput.service"
DATA_OVERRIDE=""

# 固定自签证书签名: macOS 26 的 IME 必须真 Authority (纯 ad-hoc 装上能切但 IMK 不拉起
# 控制器→无法输入); 且证书签名 cdhash 稳定, 重装不掉 TIS 注册, 不用每次去系统设置重加。
# 证书由 `scripts/mac/dev.sh sign-setup` 创建。可用环境变量覆盖 (SIGN_IDENTITY= 空则回退 ad-hoc)。
export SIGN_IDENTITY="${SIGN_IDENTITY-WindInput Dev}"

bold() { printf "\033[1m%s\033[0m\n" "$*"; }
info() { printf "  %s\n" "$*"; }
warn() { printf "\033[33m  %s\033[0m\n" "$*"; }
err()  { printf "\033[31m[错误] %s\033[0m\n" "$*" >&2; }

# ───────────────────────── 子步骤 ─────────────────────────

build_service() {
    bold "==> 编译 Rust service ($VARIANT)"
    ( cd "$RUST_DIR" && cargo build $CARGO_PROFILE_FLAG ${CARGO_FEATURES[@]+"${CARGO_FEATURES[@]}"} -p wind_service )
}

# ───────────── app_build (原 app.sh: 拼装 WindInput.app bundle) ─────────────
# build_macos_app.sh — 拼装 WindInput.app bundle (PR-A M2).
#
# SwiftPM 不直接产 .app, 这里:
#   1. swift build --product wind-input-app  (release, arm64)
#   2. 按标准 macOS .app 结构拼 Contents/{MacOS, Resources, Info.plist}
#   3. codesign --force --sign - (ad-hoc 签名, 让本机能加载; 上架走 PR-A.5 M6)
#
# 输出: wind_macos/build/WindInput.app
#
# 用法:
#   scripts/mac/dev.sh build            # release build + 签名
#   app_build --debug                   # debug build (swift build -c debug)
#   app_build --no-sign                 # 不 codesign (调试用)
#   app_build --universal               # arm64+x86_64 通用二进制 (分发/CI 用)
#   WIND_MAC_UNIVERSAL=1 ...            # 同上 (CI 走环境变量统一开关)
app_build() {
    # 变体: release → APP_NAME=WindInput; debug → APP_NAME=WindInputDebug (--debug 设置)。
    # .app 目录名/bundleID 按变体区分以支持共存; 可执行名 EXE_NAME 恒为 WindInput
    # (= CFBundleExecutable, 两变体同名, 仅所在 .app 路径不同)。
    local APP_BASE="WindInput"
    local VARIANT_SUFFIX=""        # debug 时 "Debug"
    local EXE_NAME="$APP_BASE"

    local SWIFT_CONFIG="release"
    local DO_SIGN=1
    # universal: arm64+x86_64 通用二进制. 环境变量 WIND_MAC_UNIVERSAL=1 或 --universal 开启.
    # 默认本机单架构 (本地/VM 快). CI 在 job 级设环境变量, 三件套脚本统一继承同一开关.
    local UNIVERSAL="${WIND_MAC_UNIVERSAL:-0}"
    # 默认 ad-hoc (-). 真实证书:
    #   SIGN_IDENTITY="WindInput Dev" scripts/mac/dev.sh build
    # 自签证书的创建方法见 scripts/mac/dev.sh sign-setup.
    # macOS 26 (Tahoe) 对 IME 强制要求 codesign 有真实 Authority, adhoc 被 TIS
    # 静默拒绝注册 — 本地开发期请用自签证书签名.
    SIGN_IDENTITY="${SIGN_IDENTITY:-}"
    local arg
    for arg in "$@"; do
        case "$arg" in
            --debug)     SWIFT_CONFIG="debug"; VARIANT_SUFFIX="Debug" ;;
            --no-sign)   DO_SIGN=0 ;;
            --universal) UNIVERSAL=1 ;;
            *) echo "[错误] 未知参数: $arg" >&2; exit 1 ;;
        esac
    done

    # 变体派生: APP_NAME = .app 目录名 + bundleID 后缀 (WindInput / WindInputDebug)。
    local APP_NAME="${APP_BASE}${VARIANT_SUFFIX}"
    local APP_BUNDLE="$MACOS_DIR/build/$APP_NAME.app"

    command -v swift    >/dev/null || { err "swift 未安装 (装 Xcode CLT)"; exit 1; }
    command -v codesign >/dev/null || { err "codesign 未安装 (装 Xcode CLT)"; exit 1; }

    bold "==> Build wind-input-app ($SWIFT_CONFIG$([[ $UNIVERSAL -eq 1 ]] && echo ", universal"))"
    cd "$MACOS_DIR"
    local BIN_PATH PROD_SUBDIR
    if [[ $UNIVERSAL -eq 1 ]]; then
        # 多架构: SwiftPM 直接产 universal 二进制, 但落点变为 .build/apple/Products/<config>/
        # (与单架构的 .build/<config>/ 不同), 需相应取路径.
        swift build -c "$SWIFT_CONFIG" --product wind-input-app --arch arm64 --arch x86_64
        # 多架构产物落在 .build/apple/Products/<Config>/ (首字母大写). 显式映射避免 ${x^}
        # 这种 bash 4+ 语法 (macOS 自带 /bin/bash 仍是 3.2, 会报错).
        case "$SWIFT_CONFIG" in
            release) PROD_SUBDIR="Release" ;;
            debug)   PROD_SUBDIR="Debug" ;;
            *)       PROD_SUBDIR="Release" ;;
        esac
        BIN_PATH="$MACOS_DIR/.build/apple/Products/$PROD_SUBDIR/wind-input-app"
    else
        swift build -c "$SWIFT_CONFIG" --product wind-input-app
        # SwiftPM 把二进制放在 .build/<config>/wind-input-app
        BIN_PATH="$MACOS_DIR/.build/$SWIFT_CONFIG/wind-input-app"
    fi
    [[ -x "$BIN_PATH" ]] || { err "二进制未找到: $BIN_PATH"; exit 1; }
    info "binary: $BIN_PATH ($(stat -f%z "$BIN_PATH") bytes)"
    [[ $UNIVERSAL -eq 1 ]] && info "arch: $(lipo -archs "$BIN_PATH" 2>/dev/null || echo '?')"

    bold "==> Assemble $APP_BUNDLE"
    rm -rf "$APP_BUNDLE"
    mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

    # 二进制 → Contents/MacOS/WindInput (与 Info.plist 的 CFBundleExecutable 对齐;
    # 两变体可执行同名, 仅 .app 路径不同)
    cp "$BIN_PATH" "$APP_BUNDLE/Contents/MacOS/$EXE_NAME"
    chmod +x "$APP_BUNDLE/Contents/MacOS/$EXE_NAME"

    # Info.plist
    cp "$MACOS_DIR/Sources/WindInputApp/Resources/Info.plist" "$APP_BUNDLE/Contents/Info.plist"

    # 变体注入 (debug): 全局把 bundleID 串换成 debug 变体 —— 一并改写 CFBundleIdentifier /
    # InputMethodConnectionName / ComponentInputModeDict 的 mode-id (作 dict key + TISInputSourceID
    # 值 + 有序数组项)。再把显示名 (CFBundleName/DisplayName/TISIconLabels) 加「开发版」。
    # 这样 debug .app 注册为独立输入源, 与 release 共存。
    if [[ -n "$VARIANT_SUFFIX" ]]; then
        bold "==> 变体注入 (debug): bundleID/mode/连接名/显示名 → $APP_NAME"
        sed -i '' \
            -e 's/to\.feng\.inputmethod\.WindInput/to.feng.inputmethod.WindInputDebug/g' \
            -e 's/清风输入法/清风输入法开发版/g' \
            "$APP_BUNDLE/Contents/Info.plist"
    fi

    # 版本贯通: 从仓库根 VERSION 文件 (CI 由 tag 写入) 注入 CFBundleShortVersionString /
    # CFBundleVersion. pkg 后续读 CFBundleShortVersionString 作 .pkg 文件名/版本/向导标题,
    # 故版本真源是 VERSION 文件. 无 VERSION 文件时保持 plist 原值 (0.0.0), 不破坏纯本地构建.
    local VERSION_FILE="$REPO_DIR/docs/VERSION"
    local APP_VERSION
    if [[ -f "$VERSION_FILE" ]]; then
        APP_VERSION=$(tr -d '\xef\xbb\xbf \t\r\n' < "$VERSION_FILE")
        if [[ -n "$APP_VERSION" ]]; then
            /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $APP_VERSION" "$APP_BUNDLE/Contents/Info.plist"
            /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $APP_VERSION" "$APP_BUNDLE/Contents/Info.plist"
            info "version: $APP_VERSION (来自 VERSION 文件)"
        fi
    fi

    # 本地化字符串 (输入法菜单名 / 应用显示名).
    # Resources/{zh-Hans,en}.lproj/InfoPlist.strings → Contents/Resources/<lang>.lproj/InfoPlist.strings
    local lproj lang
    for lproj in "$MACOS_DIR/Sources/WindInputApp/Resources"/*.lproj; do
        [[ -d "$lproj" ]] || continue
        lang=$(basename "$lproj")
        mkdir -p "$APP_BUNDLE/Contents/Resources/$lang"
        cp -R "$lproj"/* "$APP_BUNDLE/Contents/Resources/$lang/"
        # 变体注入 (debug): mode-id 键对齐 + 本地化显示名加「开发版」/「Debug」。
        if [[ -n "$VARIANT_SUFFIX" && -f "$APP_BUNDLE/Contents/Resources/$lang/InfoPlist.strings" ]]; then
            sed -i '' \
                -e 's/to\.feng\.inputmethod\.WindInput/to.feng.inputmethod.WindInputDebug/g' \
                -e 's/"清风输入法"/"清风输入法开发版"/g' \
                -e 's/"WindInput"/"WindInputDebug"/g' \
                "$APP_BUNDLE/Contents/Resources/$lang/InfoPlist.strings"
        fi
        info "lproj: $lang"
    done

    # 菜单栏图标 (单色 PDF 模板). plist 引用 menu_icon.pdf, 另带 _15 / _26 应对 Retina.
    # 源 SVG 在 Resources/wind-{15,26}.svg, 重新生成: rsvg-convert -f pdf -o menu_icon_15.pdf wind-15.svg
    local icon src
    for icon in menu_icon.pdf menu_icon_15.pdf menu_icon_26.pdf; do
        src="$MACOS_DIR/Sources/WindInputApp/Resources/$icon"
        if [[ -f "$src" ]]; then
            cp "$src" "$APP_BUNDLE/Contents/Resources/$icon"
            info "icon: $icon"
        else
            err "icon missing: $src (re-generate via rsvg-convert)"
            exit 1
        fi
    done

    # 应用图标 (.icns, Finder/安装器/关于面板). plist 经 CFBundleIconFile=AppIcon 引用.
    # 源 wind_setting/build/appicon.png (1024²), 重新生成 Resources/AppIcon.icns:
    #   ICONSET=$(mktemp -d)/AppIcon.iconset; mkdir -p "$ICONSET"
    #   for s in 16 32 128 256 512; do sips -z $s $s appicon.png --out "$ICONSET/icon_${s}x${s}.png"; \
    #     sips -z $((s*2)) $((s*2)) appicon.png --out "$ICONSET/icon_${s}x${s}@2x.png"; done
    #   iconutil -c icns "$ICONSET" -o wind_macos/Sources/WindInputApp/Resources/AppIcon.icns
    local APPICON="$MACOS_DIR/Sources/WindInputApp/Resources/AppIcon.icns"
    if [[ -f "$APPICON" ]]; then
        cp "$APPICON" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
        info "icon: AppIcon.icns"
    else
        err "AppIcon.icns missing: $APPICON (从 appicon.png 经 sips+iconutil 生成)"
        exit 1
    fi

    # 写一个空的 PkgInfo (传统 macOS 期望)
    printf "APPL????" > "$APP_BUNDLE/Contents/PkgInfo"

    # 校验 Info.plist
    plutil -lint "$APP_BUNDLE/Contents/Info.plist" >/dev/null

    # Ad-hoc 签名 + Hardened Runtime (本机加载够用).
    #
    # macOS Sequoia/Tahoe (26.x) 对未启用 hardened runtime 的第三方 IME 直接静默
    # 拒绝注册到 TIS, 即使 .app 已放进 /Library/Input Methods/. 对照 Qingg.app
    # (flags=0x10000 含 runtime) 与我们裸 ad-hoc (flags=0x2) 的 codesign 差异验证.
    # --options runtime 与 --sign - (ad-hoc) 可共存, 不需要 Developer ID 证书.
    if [[ $DO_SIGN -eq 1 ]]; then
        local ENTS="$MACOS_DIR/Sources/WindInputApp/Resources/WindInput.entitlements"
        local SIGN_ARGS
        if [[ -n "$SIGN_IDENTITY" ]]; then
            bold "==> codesign with identity \"$SIGN_IDENTITY\" + hardened runtime"
            SIGN_ARGS=(--force --sign "$SIGN_IDENTITY" --options runtime --timestamp=none)
        else
            # 纯 ad-hoc, **不加 --options runtime**: 实测「带 runtime 的 ad-hoc」IME 在 macOS 26
            # 上 IMK 不拉起控制器 → 装上能切但无法输入 (见 install 段说明 / commit 6a2c21a)。
            # hardened runtime 仅在真证书 (SIGN_IDENTITY) 路径配, 见上分支。
            bold "==> codesign ad-hoc (纯, 无 hardened runtime; 真证书请用 SIGN_IDENTITY)"
            SIGN_ARGS=(--force --sign - --timestamp=none)
        fi
        if [[ -f "$ENTS" ]]; then
            SIGN_ARGS+=(--entitlements "$ENTS")
        fi
        codesign "${SIGN_ARGS[@]}" "$APP_BUNDLE"
        codesign -dv --verbose=2 "$APP_BUNDLE" 2>&1 | sed 's/^/    /' | head -12
    fi

    bold "==> Done"
    info "Bundle: $APP_BUNDLE"
    info "下一步: scripts/mac/dev.sh app"
    info "       (会把 .app 复制到 ~/Library/Input Methods/ 并 killall 旧实例)"
}

build_app() {
    bold "==> 编译 Swift .app ($VARIANT)"
    app_build ${APP_VARIANT_FLAG[@]+"${APP_VARIANT_FLAG[@]}"}
}

# 解析安装用的 data 目录; 必要时快照已装数据到 build_mac/data。
resolve_data() {
    if [[ -n "$DATA_OVERRIDE" ]]; then echo "$DATA_OVERRIDE"; return; fi
    if [[ -d "$DATA_SNAPSHOT" ]]; then echo "$DATA_SNAPSHOT"; return; fi
    if [[ -d "$INSTALLED_DATA" ]]; then
        mkdir -p "$DATA_SNAPSHOT"
        cp -R "$INSTALLED_DATA/." "$DATA_SNAPSHOT/"
        warn "已把当前已装 data/ 快照到 build_mac/data (后续复用; 词库更新请重跑 data 命令)" >&2
        echo "$DATA_SNAPSHOT"; return
    fi
    err "找不到词库数据源 (--data / build_mac/data / 已装 service/data 均无)。"
    err "先用 Linux 的 scripts/dev.sh gen-data 组装, 或 --data 指定。"
    exit 1
}

# ───────────── app_install (原 install_app.sh: 装 .app 到 ~/Library/Input Methods/) ─────────────
# install_macos_app.sh — 把 WindInput.app 装到 ~/Library/Input Methods/ (用户域).
#
# 不需要 sudo (用户域安装). 装完后用户去 系统设置 → 键盘 → 文本输入 → 编辑 → + 号
# → 简体中文 → WindInput 添加一次, 后续就能在状态栏 IME 切换菜单看到.
#
# 为何用户域 (~/Library) 而非系统域 (/Library): 实测在 macOS 26 (Tahoe) 上, 用户域 +
# ad-hoc 签名的 IME 能正常进「可添加列表」(与 Fcitx5 一致); 且无需 sudo, 也避开了
# /Library 下 root 拥有 + spctl 策略的一堆坑.
#
# 参数:
#   (无)            装 release build
#   --debug         装 debug build (路径同)
#   --build         先 build 再装
#   --uninstall     卸载
#   --from <dir>    从指定目录装 (内含 <APP_NAME>.app), 供 .pkg postinstall 等离仓库场景.
app_install() {
    # 变体: release → WindInput; debug → WindInputDebug (--debug)。两变体可作为独立输入法共存。
    # EXE_NAME 恒为 WindInput (= CFBundleExecutable): 两变体进程同名, 必须按 .app 路径定位进程,
    # 否则装/卸 debug 会误杀正在使用的 release (反之亦然)。
    local APP_NAME="WindInput"
    local EXE_NAME="WindInput"
    local BUNDLE_ID="to.feng.inputmethod.WindInput"
    local INSTALL_DIR="$HOME/Library/Input Methods"
    local SRC_DIR=""

    local DO_BUILD=0
    local DO_UNINSTALL=0
    local BUILD_ARGS=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --build) DO_BUILD=1 ;;
            --debug) BUILD_ARGS+=("--debug"); APP_NAME="WindInputDebug"; BUNDLE_ID="to.feng.inputmethod.WindInputDebug" ;;
            --uninstall) DO_UNINSTALL=1 ;;
            # --from <dir>: 从指定目录装 (内含 <APP_NAME>.app), 供 .pkg postinstall 等离仓库场景.
            --from) shift; SRC_DIR="${1:-}"; [[ -n "$SRC_DIR" ]] || { echo "[错误] --from 缺目录参数" >&2; exit 1; } ;;
            *) echo "[错误] 未知参数: $1" >&2; exit 1 ;;
        esac
        shift
    done

    # 变体派生 (在 --debug/--from 解析后, 不依赖参数顺序)。
    local APP_BUNDLE="${SRC_DIR:-$MACOS_DIR/build}/$APP_NAME.app"
    local INSTALL_APP="$INSTALL_DIR/$APP_NAME.app"

    # 用户域安装一律以普通用户运行 (不要 sudo): ~/Library 归属当前用户, 用 sudo 反而会让
    # .app / register 进程变成 root 拥有, 引发权限错乱.
    if [[ $EUID -eq 0 ]]; then
        err "请以普通用户运行 (用户域 ~/Library 安装, 不要 sudo)."
        exit 1
    fi

    # -------- uninstall (完整清理) --------
    # 仅 rm .app 是不够的: register 守护进程残留 / HIToolbox plist 启用项 / TIS LS DB
    # 缓存 / Caches & Application Support 都可能残留, 导致系统设置里出现幽灵条目.
    # 这里一次清干净.
    if [[ $DO_UNINSTALL -eq 1 ]]; then
        bold "==> Uninstall $APP_NAME (full purge)"

        # 1. 杀本变体的 IME 进程 (含 --register-input-source 后台守护)。按 .app 路径定位,
        #    两变体进程同名 WindInput, 不能用进程名匹配 (会误杀另一变体)。
        info "kill $APP_NAME processes"
        pkill -9 -f "$APP_NAME.app/Contents/MacOS/$EXE_NAME" 2>/dev/null || true
        rm -f /tmp/wind_register.log

        # 2. 删 .app (用户域旧路径 + 历史可能装过的系统域 /Library 都尝试清)
        local app
        for app in "$INSTALL_APP" "/Library/Input Methods/$APP_NAME.app"; do
            if [[ -d "$app" ]]; then
                if [[ -w "$(dirname "$app")" ]]; then
                    rm -rf "$app" && info "removed $app"
                else
                    info "(跳过 $app: 无写权限, 如需删请手动 sudo rm -rf)"
                fi
            fi
        done

        # 3. 清 HIToolbox plist 内启用项 / 选中项 (本 bundleID 相关)
        #    显式走 /usr/bin/python3 (Apple framework, plistlib 稳定);
        #    用户 PATH 上的 Homebrew python3.14 可能 libexpat ABI 不匹配, plistlib 起不来.
        info "clean HIToolbox enabled/selected entries"
        /usr/bin/python3 - <<PY
import plistlib, os, sys
path = os.path.expanduser('~/Library/Preferences/com.apple.HIToolbox.plist')
bid = "$BUNDLE_ID"
try:
    with open(path, 'rb') as f: plist = plistlib.load(f)
except FileNotFoundError:
    sys.exit(0)
changed = False
for key in ('AppleEnabledInputSources', 'AppleSelectedInputSources', 'AppleInputSourceHistory'):
    if key in plist and isinstance(plist[key], list):
        before = len(plist[key])
        plist[key] = [s for s in plist[key] if (s.get('Bundle ID') if isinstance(s, dict) else None) != bid]
        if len(plist[key]) != before:
            print(f"    {key}: {before} -> {len(plist[key])}")
            changed = True
if changed:
    with open(path, 'wb') as f: plistlib.dump(plist, f)
    print("    HIToolbox plist updated")
else:
    print("    (no HIToolbox entries matched)")
PY

        # 4. 清缓存 / state (变体目录: debug 用 Caches/WindInputDebug + App Support/WindInput_debug,
        #    与 Go buildvariant AppName()/Suffix() 对齐; release 用不带后缀的)。
        local PURGE_DIRS d
        if [[ "$APP_NAME" == "WindInputDebug" ]]; then
            PURGE_DIRS=("$HOME/Library/Caches/WindInputDebug" "$HOME/Library/Application Support/WindInput_debug")
        else
            PURGE_DIRS=("$HOME/Library/Caches/WindInput" "$HOME/Library/Application Support/WindInput")
        fi
        for d in "${PURGE_DIRS[@]}"; do
            if [[ -d "$d" ]]; then
                rm -rf "$d"
                info "removed $d"
            fi
        done

        # 5. *绝不* 跑 lsregister -u / -kill (血泪教训).
        #    - lsregister -u <已删除路径>: 行为未定义, 会污染 LaunchServices DB, 导致系统设置
        #      "添加输入法" picker 对所有用户(含全新账户)报 "键盘布局不可用". 实测后果严重.
        #    - lsregister -kill -r: 新版 macOS 已移除该选项 (官方说法: dangerous & no longer useful).
        #    安全做法: .app 已删 + HIToolbox plist 已清 + cfprefsd reload, 足以让 TIS 失忆;
        #    残留 LS 索引在下次扫描自然失效. 若仍需强制刷新, 只用 `lsregister -f -R <现存路径>`
        #    (-f 重新登记, 非破坏性), 绝不对已删除路径操作.

        # 6. 重启 input source UI agents (让菜单栏 / 系统设置面板重扫).
        #    踩过的坑: killall -9 (SIGKILL) 这些 LaunchAgent 在 macOS 26 SIP 下不能
        #    用 launchctl kickstart 手动重启; 必须只发 SIGTERM, 靠 launchd 自动 respawn.
        info "restart text input agents (SIGTERM, launchd auto-respawn)"
        killall -HUP cfprefsd 2>/dev/null || true
        killall TextInputMenuAgent 2>/dev/null || true
        killall TextInputSwitcher 2>/dev/null || true
        killall imklaunchagent 2>/dev/null || true

        bold "==> Done"
        info "如果系统设置里还残留, 注销重登一次系统让 TextInputSources 全量重扫"
        exit 0
    fi

    # -------- build (可选) --------
    if [[ $DO_BUILD -eq 1 ]]; then
        # 空数组 + set -u 在 bash 5 之前展开会报 unbound; 用 ${arr[@]+"${arr[@]}"} 形式
        # 在数组未设/空时整体不展开任何参数, 非空时正常按数组逐项展开.
        app_build ${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}
    fi

    [[ -d "$APP_BUNDLE" ]] || { err "未找到 $APP_BUNDLE, 先跑 scripts/mac/dev.sh build"; exit 1; }

    # -------- install --------
    bold "==> Install $APP_BUNDLE -> $INSTALL_APP"

    # 1. 关掉本变体的旧实例 (IMKit 进程通常常驻; 不杀的话 cp 会被持锁)。
    #    按 .app 路径定位: 两变体进程同名 WindInput, 进程名匹配会误杀另一变体。
    if pgrep -f "$APP_NAME.app/Contents/MacOS/$EXE_NAME" >/dev/null; then
        info "停止旧 $APP_NAME 进程"
        pkill -9 -f "$APP_NAME.app/Contents/MacOS/$EXE_NAME" 2>/dev/null || true
        sleep 0.5
    fi

    # 2. 复制 .app
    mkdir -p "$INSTALL_DIR"
    rm -rf "$INSTALL_APP"
    cp -R "$APP_BUNDLE" "$INSTALL_DIR/"
    info "已复制 $INSTALL_APP"

    # 3. ad-hoc 产物: 就地去 hardened-runtime 重签 (实测必要).
    #    app_build 默认产出 `flags=0x10002(adhoc,runtime)` (ad-hoc + hardened runtime).
    #    实测可正常进可添加列表 + 能被 IMK 拉起的配置是「纯 ad-hoc」(flags=0x2, 无 runtime 标志,
    #    与 Fcitx5 一致); 带 runtime 的 ad-hoc 在 macOS 26 上行为存疑. 这里对 ad-hoc 产物原地
    #    重签去掉 runtime 标志.
    #    注: ad-hoc 重签 (`-s -`) 不涉及 keychain/证书, 普通用户即可, 幂等.
    #    若 build 用了真实证书 (SIGN_IDENTITY / 已公证), 则用该证书重签, 但同样去 hardened-runtime.
    # 检测须用 --verbose=2: 默认 -dv (verbose=1) 不打印 "Signature=adhoc" 行 (踩过的坑).
    # 判据: CodeDirectory flags 里含 adhoc / 或 Signature=adhoc; 真证书则有 Authority=Developer ID.
    # SIGN_IDENTITY 非空: 用固定自签证书重签 (csreq 基于证书身份而非 cdhash, 重新部署 .app
    #   后辅助功能/TCC 授权不失效; 证书由 sign-setup 在本机创建)。
    #   去 hardened-runtime (Sequoia 上 ad-hoc 路径一致行为), 仅换签名身份。
    # 注意: 不要给 IME 加 --options runtime。旧 Go 仓实测「带 runtime 的 ad-hoc」反而异常;
    #   能稳定被 IMK 拉起控制器的是纯 ad-hoc/无 runtime (与 Fcitx5 一致)。多数「装上、列表里有、
    #   能切但无法输入」的根因不是签名, 而是 TIS 注册缓存污染 → 需注销重登做一次全量重扫。
    if [[ -n "${SIGN_IDENTITY:-}" ]]; then
        # 无头 ssh 部署: login keychain 在该 security session 默认锁定, codesign 访问私钥
        # 会 errSecInternalComponent。提供 SIGN_KEYCHAIN_PW 则先解锁 (本地 GUI 部署无需)。
        if [[ -n "${SIGN_KEYCHAIN_PW:-}" ]]; then
            security unlock-keychain -p "$SIGN_KEYCHAIN_PW" "$HOME/Library/Keychains/login.keychain-db" 2>/dev/null \
                && info "已解锁 login keychain (供 codesign)" || info "解锁 login keychain 失败"
        fi
        info "固定证书重签 .app: \"$SIGN_IDENTITY\" (去 hardened-runtime, 仅换签名身份)"
        codesign --force --sign "$SIGN_IDENTITY" --deep "$INSTALL_APP" 2>&1 | sed 's/^/    /' || true
    elif codesign -dv --verbose=2 "$INSTALL_APP" 2>&1 | grep -qi "adhoc"; then
        info "ad-hoc 产物: 去 hardened-runtime 重签 (codesign --force --sign -)"
        codesign --force --sign - --deep "$INSTALL_APP" 2>&1 | sed 's/^/    /' || true
    else
        info "(检测到真实证书签名, 保留原签名不重签)"
    fi
    codesign -dv --verbose=2 "$INSTALL_APP" 2>&1 | grep -E "Authority|flags|Signature" | sed 's/^/    /'

    # 3a. (已移除 spctl --add 白名单步骤 — 踩过的坑 + 证伪)
    #     早期以为 ad-hoc IME 被 spctl reject 会导致 TIS 不收录, 故加 spctl --add 白名单.
    #     实测证伪: Fcitx5 同为 ad-hoc + `spctl -a` rejected, 仍能正常进可添加列表; 而
    #     macOS 26 (Tahoe) 已移除该能力 (`spctl --add` 报 "This operation is no longer
    #     supported"). 真正决定能否进列表的是 Info.plist 不再带 tsInputModeDefaultStateKey
    #     (见 wind_macos .../Resources/Info.plist 内说明), 与签名 / spctl 无关. 不再做 spctl 操作.

    # 4. 让系统重新发现 IME bundle.
    #    macOS 改 IME plist 后, 仅 cp 进 Input Methods/ 不足以让系统刷新 "输入源" 列表 ——
    #    LaunchServices 用 ChangeCount 缓存 bundle 信息, 不会因为 .app 替换而主动失效.
    #    必须显式跑 lsregister -f 强制重读, 才能让新字段 (ComponentInputModeDict 等) 进入索引.
    #    这是 Big Sur+ 上很多自打包 IME 装完看不见的真因.

    local LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

    # 4a. 强制 lsregister 重读本 bundle 元数据 (LaunchServices DB).
    if [[ -x "$LSREGISTER" ]]; then
        info "lsregister -f $INSTALL_APP"
        "$LSREGISTER" -f -R "$INSTALL_APP" 2>&1 | tail -3 | sed 's/^/    /'
    else
        info "(lsregister 不在标准位置, 跳过)"
    fi

    # 4b. 杀缓存进程, 让它们重启时按新 LS DB 重扫 Input Methods/.
    #    只发 SIGTERM (不要 -9): SIP 下这些 LaunchAgent 不能 launchctl kickstart 手动重启,
    #    必须靠 launchd 在收到 SIGTERM 后自动 respawn; SIGKILL 可能让它不被重启.
    killall -HUP cfprefsd 2>/dev/null || true
    killall TextInputMenuAgent 2>/dev/null || true
    killall TextInputSwitcher  2>/dev/null || true
    killall imklaunchagent 2>/dev/null || true

    # 4c. 触发一次 input sources 重读
    defaults read com.apple.HIToolbox AppleEnabledInputSources >/dev/null 2>&1 || true

    # 4d. 调本 .app 自身 binary 的 --register-input-source 立即注册 (免重启即可在 picker 出现).
    #     macOS Tahoe (26) 起 TIS 仅接受来自 IME 自身进程的 TISRegisterInputSource 调用
    #     (校验 codesign identity 匹配 bundleID), 外部 swift CLI 调 silently no-op.
    #     (用户域无 sudo, 直接以当前用户跑.)
    local APP_EXEC="$INSTALL_APP/Contents/MacOS/WindInput"
    local REGISTER_PID
    if [[ -x "$APP_EXEC" ]]; then
        # 重要: register 进程保持运行以维持 TIS 注册 (踩过的坑: register 完立刻 exit 后
        # mode 可能被系统在几秒内清掉). 后台 fork, 主流程不阻塞.
        info "$APP_EXEC --register-input-source (后台常驻维持注册)"
        "$APP_EXEC" --register-input-source > /tmp/wind_register.log 2>&1 &
        REGISTER_PID=$!
        sleep 1  # 等 TIS DB 写完
        info "    PID=$REGISTER_PID (要停止后台 register: kill $REGISTER_PID)"
        head -2 /tmp/wind_register.log 2>/dev/null | sed 's/^/    /'
    fi

    bold "==> Done"
    cat <<EOF

  下一步:
    1. 打开 系统设置 → 键盘 → 文本输入 → 编辑 → 添加 (+) → 简体中文 → 选 WindInput
       如果列表里看不到 WindInput, 按下面顺序排查:
         a) ls -la "$INSTALL_APP" 看 .app 是否真的在
         b) /usr/libexec/PlistBuddy -c "Print" "$INSTALL_APP/Contents/Info.plist" | head -40
            必须有 InputMethodConnectionName / InputMethodServerControllerClass /
            ComponentInputModeDict / LSUIElement=true (不能是 LSBackgroundOnly);
            *不应* 出现 tsInputModeDefaultStateKey (有的话该 mode 会被「+」列表过滤掉)
         c) codesign -dv "$INSTALL_APP" 应输出 adhoc 签名信息 (flags 不含 runtime)
         d) 注销重登一次系统 (最暴力但有效, 让 TextInputSources 全量重扫)
    2. 切到 WindInput (Ctrl+Space 或菜单栏 IME 切换)
    3. 在任意文本框敲一个字母键, 然后:

         tail -F "\$HOME/Library/Logs/WindInput/wind_input.log"
         log stream --predicate 'process == "WindInput"' --info --debug

       应看到:
         Go 端 : "bridge client connected connID=N"
         IME 端: "WindInput[InputController] bridge connected"
                "WindInput[handle] ..." 或 PassThrough/Consumed 路径

  卸载:    scripts/mac/dev.sh uninstall

EOF
}

# ───────────── service_install (原 install_service.sh: 装 Rust 服务 + LaunchAgent) ─────────────
# install_service.sh — 把 Rust 服务 (wind_input + data/) 装到 per-user 目录,
# 并以 LaunchAgent 形式注册为开机自启常驻进程 (移植自旧 Go 仓 scripts_mac/deploy)。
#
# 服务定位词库用 exeDir/data (见 wind-config Config::data_dir = current_exe()/data),
# 所以二进制和 data/ 必须同目录 (本函数装到 INSTALL_ROOT 与 INSTALL_ROOT/data)。
# 用户数据 (config.toml / userdata.redb / socket) 走运行时目录
# (~/Library/Application Support/WindInput{_debug}), 与 service/ 子目录互不干扰。
#
# 以普通用户运行 (LaunchAgent 是 per-user gui domain, 不要 sudo)。
#
# 参数:
#   (无)            装 release 产物 (target/release)
#   --debug         装 debug 产物 (target/debug, 需 debug_variant 特性构建)
#   --data <dir>    指定 data/ 源目录 (默认 build_debug/data)
#   --from <dir>    从指定目录装 (内含 wind_input + data), 供 .pkg postinstall 等场景
#   --uninstall     卸载服务 (保留用户数据)
#
# 注: release .app (bundleID …WindInput) 连 release 服务 (suffix=""); debug .app
#     (…WindInputDebug) 连 debug 服务 (suffix="_debug")。debug 服务二进制须用
#     `cargo build --features debug_variant` 构建, 否则 suffix 仍为空、连不上 debug .app。
service_install() {
    local RUST_TARGET="$REPO_DIR/wind_input/target"
    local LOG_DIR="$HOME/Library/Logs"
    local GUI_DOMAIN="gui/$(id -u)"

    local DEBUG_VARIANT=0
    local DO_UNINSTALL=0
    local SRC_DIR=""
    local DATA_DIR=""
    local EXE_NAME="wind_input"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --debug)     DEBUG_VARIANT=1 ;;
            --uninstall) DO_UNINSTALL=1 ;;
            --data)      shift; DATA_DIR="${1:-}"; [[ -n "$DATA_DIR" ]] || { echo "[错误] --data 缺目录参数" >&2; exit 1; } ;;
            --from)      shift; SRC_DIR="${1:-}"; [[ -n "$SRC_DIR" ]] || { echo "[错误] --from 缺目录参数" >&2; exit 1; } ;;
            *) echo "[错误] 未知参数: $1" >&2; exit 1 ;;
        esac
        shift
    done

    # 变体派生: debug 用独立 LaunchAgent label + 运行时目录 (WindInput_debug, 与
    # wind-config debug_variant 的 PIPE_SUFFIX 及 .app BridgeEndpoints.runtimeDir 对齐),
    # 让 debug/release 两套服务共存, 各连各自的 .app socket。
    # 安装后可执行名装为中文名: macOS 后台列表 (BTM) 对无 Developer ID 的 legacy agent
    # 直接显示可执行文件名 (AssociatedBundleIdentifiers 被忽略)。二进制改名不影响功能。
    local LABEL APP_SUPPORT SVC_EXE_NAME ASSOC_BUNDLE LOG_TAG
    if [[ $DEBUG_VARIANT -eq 1 ]]; then
        LABEL="to.feng.windinput.service.debug"
        APP_SUPPORT="$HOME/Library/Application Support/WindInput_debug"
        SVC_EXE_NAME="清风输入法服务开发版"
        ASSOC_BUNDLE="to.feng.inputmethod.WindInputDebug"
        LOG_TAG="windinput_debug"
    else
        LABEL="to.feng.windinput.service"
        APP_SUPPORT="$HOME/Library/Application Support/WindInput"
        SVC_EXE_NAME="清风输入法服务"
        ASSOC_BUNDLE="to.feng.inputmethod.WindInput"
        LOG_TAG="windinput"
    fi
    local INSTALL_ROOT="$APP_SUPPORT/service"
    local INSTALL_EXE="$INSTALL_ROOT/$SVC_EXE_NAME"
    local PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
    local OUT_LOG="$LOG_DIR/$LOG_TAG.out.log"
    local ERR_LOG="$LOG_DIR/$LOG_TAG.err.log"
    local PUSH_SOCK="$APP_SUPPORT/bridge_push.sock"

    if [[ $EUID -eq 0 ]]; then
        err "请以普通用户运行 (LaunchAgent 是 per-user gui domain). 不要 sudo."
        exit 1
    fi

    # -------- uninstall --------
    if [[ $DO_UNINSTALL -eq 1 ]]; then
        bold "==> Uninstall Rust service ($LABEL)"
        if launchctl print "$GUI_DOMAIN/$LABEL" >/dev/null 2>&1; then
            launchctl bootout "$GUI_DOMAIN/$LABEL" 2>/dev/null || true
            info "bootout $GUI_DOMAIN/$LABEL"
        else
            info "(service 未加载)"
        fi
        [[ -f "$PLIST" ]] && { rm -f "$PLIST"; info "removed $PLIST"; } || info "(no $PLIST)"
        # 只删 service/ 子目录 (二进制+预制词库), 保留用户数据 (../config.toml, userdata.redb)。
        [[ -d "$INSTALL_ROOT" ]] && { rm -rf "$INSTALL_ROOT"; info "removed $INSTALL_ROOT"; } || info "(no $INSTALL_ROOT)"
        bold "==> Done (用户数据保留在 $APP_SUPPORT/)"
        exit 0
    fi

    # -------- 解析源目录 --------
    if [[ -z "$SRC_DIR" ]]; then
        if [[ $DEBUG_VARIANT -eq 1 ]]; then
            SRC_DIR="$RUST_TARGET/debug"
        else
            SRC_DIR="$RUST_TARGET/release"
        fi
    fi
    local SRC_EXE="$SRC_DIR/$EXE_NAME"
    # data 源: 优先 --data; 否则 SRC_DIR/data (与二进制同目录); 再否则 build_debug/data (dev.sh gen-data 产物)。
    if [[ -z "$DATA_DIR" ]]; then
        if [[ -d "$SRC_DIR/data" ]]; then DATA_DIR="$SRC_DIR/data"; else DATA_DIR="$REPO_DIR/build_debug/data"; fi
    fi

    [[ -f "$SRC_EXE" ]]  || { err "未找到二进制 $SRC_EXE, 先跑 cargo build$([[ $DEBUG_VARIANT -eq 0 ]] && echo ' --release') -p wind_service"; exit 1; }
    [[ -d "$DATA_DIR" ]] || { err "未找到词库目录 $DATA_DIR, 先跑 scripts/mac/dev.sh gd 组装 data"; exit 1; }

    # -------- install --------
    bold "==> Install Rust service -> $INSTALL_ROOT"

    # 1. 停旧服务实例。
    if launchctl print "$GUI_DOMAIN/$LABEL" >/dev/null 2>&1; then
        info "停止旧服务实例"
        launchctl bootout "$GUI_DOMAIN/$LABEL" 2>/dev/null || true
    fi
    # 清理孤儿进程 (前台跑过或上次 bootout 漏网的旧 wind_input 会占着 socket)。
    # 按 service 目录精确匹配, 不误杀同名其它进程。
    if pgrep -f "$INSTALL_ROOT/" >/dev/null 2>&1; then
        info "清理残留的旧服务进程"
        pkill -f "$INSTALL_ROOT/" 2>/dev/null || true
        sleep 1
    fi
    rm -f "$INSTALL_ROOT/wind_input"  # 删旧文件名残留 (升级到中文名时)

    # 2. 复制二进制 + 词库 (data/ 用 rsync --delete 与源一致)。
    mkdir -p "$INSTALL_ROOT" "$LOG_DIR" "$HOME/Library/LaunchAgents"
    cp -f "$SRC_EXE" "$INSTALL_EXE"
    chmod +x "$INSTALL_EXE"
    # 原地重签: 跨机/同路径部署时内核 amfi 缓存上版 cdhash, 新二进制经 launchd 起来会校验失配。
    # --force 重签刷新; ad-hoc 幂等。SIGN_IDENTITY 设则用固定证书 (无头 ssh 需 SIGN_KEYCHAIN_PW 解锁)。
    if command -v codesign >/dev/null; then
        if [[ -n "${SIGN_IDENTITY:-}" ]]; then
            if [[ -n "${SIGN_KEYCHAIN_PW:-}" ]]; then
                security unlock-keychain -p "$SIGN_KEYCHAIN_PW" "$HOME/Library/Keychains/login.keychain-db" 2>/dev/null || true
            fi
            codesign --force -s "$SIGN_IDENTITY" "$INSTALL_EXE" 2>/dev/null \
                && info "固定证书重签服务二进制: \"$SIGN_IDENTITY\"" \
                || info "codesign 重签跳过 (非致命)"
        else
            codesign --force -s - "$INSTALL_EXE" 2>/dev/null \
                && info "ad-hoc 重签服务二进制" \
                || info "codesign 重签跳过 (非致命)"
        fi
    fi
    if command -v rsync >/dev/null; then
        rsync -a --delete "$DATA_DIR/" "$INSTALL_ROOT/data/"
    else
        rm -rf "$INSTALL_ROOT/data"; cp -R "$DATA_DIR" "$INSTALL_ROOT/data"
    fi
    info "已复制 服务二进制 + data/ ($(find "$INSTALL_ROOT/data" -type f | wc -l | tr -d ' ') 个数据文件)"

    # 3. 写 LaunchAgent plist (RunAtLoad 开机自启 + KeepAlive 崩溃自拉起)。
    cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>AssociatedBundleIdentifiers</key>
    <string>$ASSOC_BUNDLE</string>
    <key>ProgramArguments</key>
    <array>
        <string>$INSTALL_EXE</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>$OUT_LOG</string>
    <key>StandardErrorPath</key>
    <string>$ERR_LOG</string>
</dict>
</plist>
PLIST_EOF
    info "已写 $PLIST"

    # 4. 加载 + 启用 + 启动。
    launchctl bootstrap "$GUI_DOMAIN" "$PLIST" 2>/dev/null || {
        err "bootstrap 失败, 重试一次 (可能旧实例未完全退出)"
        launchctl bootout "$GUI_DOMAIN/$LABEL" 2>/dev/null || true
        launchctl bootstrap "$GUI_DOMAIN" "$PLIST"
    }
    launchctl enable "$GUI_DOMAIN/$LABEL" 2>/dev/null || true
    launchctl kickstart -k "$GUI_DOMAIN/$LABEL" 2>/dev/null || true
    info "bootstrap + enable + kickstart 完成"

    # 5. 验证 (等服务起 socket)。
    bold "==> Verify"
    local i
    for i in 1 2 3 4 5 6 7 8 9 10; do
        [[ -S "$PUSH_SOCK" ]] && break
        sleep 0.3
    done
    local STATE PID
    STATE=$(launchctl print "$GUI_DOMAIN/$LABEL" 2>/dev/null | grep -E '^[[:space:]]*state =' | head -1 | sed 's/^[[:space:]]*//' || true)
    PID=$(launchctl print "$GUI_DOMAIN/$LABEL" 2>/dev/null | grep -E '^[[:space:]]*pid =' | head -1 | sed 's/^[[:space:]]*//' || true)
    info "launchd: ${STATE:-未知} ${PID:-}"
    if [[ -S "$PUSH_SOCK" ]]; then
        info "✓ push socket 存在: $PUSH_SOCK"
    else
        err "✗ push socket 未出现: $PUSH_SOCK (看 $ERR_LOG)"
    fi
    if [[ -s "$ERR_LOG" ]]; then
        info "err.log 尾部:"; tail -5 "$ERR_LOG" | sed 's/^/    /'
    else
        info "✓ err.log 为空"
    fi

    bold "==> Done"
    cat <<EOF

  服务已注册为开机自启 ($LABEL).
  状态: launchctl print $GUI_DOMAIN/$LABEL | grep -E 'state|pid'
  重启: launchctl kickstart -k $GUI_DOMAIN/$LABEL
  日志: $OUT_LOG / $ERR_LOG
  卸载: scripts/mac/dev.sh uninstall
EOF
}

install_service() {
    build_service
    local data; data="$(resolve_data)"
    bold "==> 安装 service ($VARIANT, data=$data)"
    service_install ${APP_VARIANT_FLAG[@]+"${APP_VARIANT_FLAG[@]}"} --data "$data"
}

install_app() {
    build_app
    bold "==> 安装 app ($VARIANT)"
    app_install ${APP_VARIANT_FLAG[@]+"${APP_VARIANT_FLAG[@]}"}
    # 防复发: 删掉 build/ 里的 .app 并注销其 LS 登记。它与 ~/Library 里的真身同 bundle-ID,
    # 留着会被 LaunchServices 自动登记成「重复输入源」, TIS 可能错指向它(尤其路径后被删=
    # 幽灵)→ 控制器拉不起 → 无法输入。真身已装在 ~/Library, build/ 仅中间产物, 可删。
    local appname="WindInput"; [[ "$VARIANT" == debug ]] && appname="WindInputDebug"
    local built="$REPO_DIR/wind_macos/build/$appname.app"
    if [[ -d "$built" ]]; then
        local lsreg="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
        [[ -x "$lsreg" ]] && "$lsreg" -u "$built" 2>/dev/null || true
        rm -rf "$built"
        info "已清理 build/ 重复 .app(防 LS 重复登记导致无法输入)"
    fi
}

do_run() {
    bold "==> 重启 service ($LABEL)"
    launchctl kickstart -k "gui/$(id -u)/$LABEL" && info "kickstart 完成" || err "kickstart 失败 (service 未安装?)"
}

do_logs() {
    bold "==> 跟踪日志 (Ctrl-C 退出)"
    info "service: ~/Library/Logs/windinput.out.log | IME: log stream process==WindInput"
    # 同时跟 service 文件日志 + IME 系统日志 (renderFrame/forwarder)。
    ( log stream --predicate 'process == "WindInput"' --info 2>/dev/null | grep --line-buffered -E 'renderFrame|forwarder|bridge|handle|caret' & )
    tail -F "$HOME/Library/Logs/windinput.out.log"
}

do_status() {
    bold "==> service 状态"
    launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null | grep -E 'state =|pid =' | head || warn "service 未注册"
    info "二进制: $(ls -la "$APP_SUPPORT/service/"*服务* 2>/dev/null | awk '{print $6,$7,$8}' | head -1)"
    info "push socket: $([[ -S "$APP_SUPPORT/bridge_push.sock" ]] && echo 存在 || echo 缺失)"
    local app="$HOME/Library/Input Methods/WindInput.app"
    [[ "$VARIANT" == debug ]] && app="$HOME/Library/Input Methods/WindInputDebug.app"
    info ".app 签名: $(codesign -dv --verbose=2 "$app" 2>&1 | grep -E 'flags=' | sed 's/.*flags/flags/' || echo 未装)"
    info "IME 进程: $(pgrep -fl 'Input Methods/WindInput' | head -1 || echo 未运行)"
}

do_data() {
    [[ -d "$INSTALLED_DATA" ]] || { err "当前未装 service data ($INSTALLED_DATA), 无法快照"; exit 1; }
    rm -rf "$DATA_SNAPSHOT"; mkdir -p "$DATA_SNAPSHOT"
    cp -R "$INSTALLED_DATA/." "$DATA_SNAPSHOT/"
    info "已快照 data/ → build_mac/data ($(find "$DATA_SNAPSHOT" -type f | wc -l | tr -d ' ') 文件)"
}

do_uninstall() {
    bold "==> 卸载 service + app ($VARIANT)"
    # 内联函数在卸载分支用 `exit 0` 终止; 放进子 shell 以免终止整个 dev.sh (此处需顺序卸两端)。
    ( service_install ${APP_VARIANT_FLAG[@]+"${APP_VARIANT_FLAG[@]}"} --uninstall ) || true
    ( app_install     ${APP_VARIANT_FLAG[@]+"${APP_VARIANT_FLAG[@]}"} --uninstall ) || true
}

# ───────────── sign_setup (原 setup_signing.sh: 命令行建自签证书) ─────────────
# setup_signing.sh — 命令行创建自签 Code Signing 证书并 import 到 login keychain.
# 用 openssl + security cli, 完全跳过 Keychain Access GUI.
#
# 输出: 一个名为 "WindInput Dev" 的可用于 codesign 的本机证书.
# 用法: scripts/mac/dev.sh sign-setup        # 创建
#       scripts/mac/dev.sh sign-setup check  # 仅检查现状
#       scripts/mac/dev.sh sign-setup grant  # 授权 codesign 非交互访问私钥
#       scripts/mac/dev.sh sign-setup remove # 删掉证书
sign_setup() {
    # 原 setup_signing.sh 用 `set -uo pipefail` (无 errexit): 多处依赖命令失败继续
    # (find/delete 探测、清理循环)。这里在函数内关掉 errexit 以原样保留其控制流。
    set +e

    local CERT_NAME="WindInput Dev"
    local WORK_DIR="${TMPDIR:-/tmp}/wind_input_cert"
    local CFG_FILE="$WORK_DIR/openssl.cnf"
    local KEY_FILE="$WORK_DIR/cert.key"
    local CRT_FILE="$WORK_DIR/cert.crt"
    local P12_FILE="$WORK_DIR/cert.p12"
    local P12_PASS="windinput-dev"

    # purge_cert — 删除所有同名证书, 带次数上限防死循环。
    # 关键: 残留可能在 System keychain (如某次 add-trusted-cert 部分成功留下 cert),
    # 普通 delete-certificate 删不掉受保护的 System keychain 条目 → 原先 while 会死循环。
    # 这里 login + System (sudo) 都试, 且 20 次封顶。
    purge_cert() {
        local i=0
        while security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; do
            security delete-certificate -c "$CERT_NAME" >/dev/null 2>&1
            sudo security delete-certificate -c "$CERT_NAME" /Library/Keychains/System.keychain >/dev/null 2>&1
            i=$((i + 1))
            if [[ $i -ge 20 ]]; then
                err "清理 \"$CERT_NAME\" 超过 20 次仍残留, 放弃 (手动检查 login/System keychain)"
                break
            fi
        done
    }

    local SUB="${1:-create}"

    # ---------------- check ----------------
    if [[ "$SUB" == "check" ]]; then
        bold "查询当前 codesigning identity"
        security find-identity -v -p codesigning
        exit 0
    fi

    # ---------------- grant ----------------
    # 授权 codesign 非交互访问私钥 (set-key-partition-list)。import -A 仍不足以让
    # codesign 在无 GUI 授权上下文 (如 ssh 部署会话) 访问私钥 → errSecInternalComponent;
    # 设 partition-list 后 apple/codesign 工具可免授权使用, 无头 ssh 部署才能用证书签名。
    if [[ "$SUB" == "grant" ]]; then
        local KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
        bold "授权 codesign 非交互访问 \"$CERT_NAME\" 私钥"
        printf "  输入此 Mac 的登录密码 (解锁 login keychain): "
        local PW; read -rs PW; echo
        if ! security unlock-keychain -p "$PW" "$KEYCHAIN"; then
            err "解锁 login keychain 失败 (密码错?)"; unset PW; exit 1
        fi
        if security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PW" "$KEYCHAIN" >/dev/null 2>&1; then
            bold "成功: codesign 现可非交互访问私钥 (ssh 无头部署可用此证书签名)"
        else
            err "set-key-partition-list 失败"
        fi
        unset PW
        exit 0
    fi

    # ---------------- remove ----------------
    if [[ "$SUB" == "remove" ]]; then
        bold "删 \"$CERT_NAME\" 证书 (所有同名条目, 含 System keychain)"
        purge_cert
        # 删 trust 设置 (admin trust 与 user trust)
        sudo security remove-trusted-cert -d -p codeSign 2>/dev/null || true
        bold "remove 完成"
        exit 0
    fi

    # ---------------- create ----------------
    command -v openssl  >/dev/null || { err "openssl 未安装"; exit 1; }
    command -v security >/dev/null || { err "security cli 未安装"; exit 1; }

    # 清理已有同名证书 (踩过的坑: 失败的 import 也会留条目, 重复后 codesign ambiguous;
    # 残留可能在 System keychain 需 sudo 删, 普通 delete 删不掉会死循环, 见 purge_cert)
    if security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
        bold "发现已有 \"$CERT_NAME\" 证书, 清掉重建"
        purge_cert
    fi

    mkdir -p "$WORK_DIR"
    chmod 700 "$WORK_DIR"

    bold "1. 生成 openssl 配置 (X509 extensions for code signing)"
    cat > "$CFG_FILE" <<EOF
[ req ]
distinguished_name = req_distinguished_name
prompt             = no
x509_extensions    = v3_self

[ req_distinguished_name ]
CN = $CERT_NAME
O  = WindInput Local
C  = CN

[ v3_self ]
basicConstraints       = critical, CA:false
keyUsage               = critical, digitalSignature
extendedKeyUsage       = critical, codeSigning
subjectKeyIdentifier   = hash
EOF
    info "$CFG_FILE"

    bold "2. 生成 RSA 2048 私钥 + 自签 X509 证书 (有效期 10 年)"
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$KEY_FILE" -out "$CRT_FILE" \
        -days 3650 -config "$CFG_FILE" -sha256 2>&1 | tail -3 | sed 's/^/  /'
    [[ -f "$CRT_FILE" ]] || { err "openssl 生成失败"; exit 1; }

    bold "3. 打成 PKCS12 (.p12, legacy 格式) 以便 security import"
    # OpenSSL 3.x 默认 PBES2 (PBKDF2 + AES) macOS security import 不识别, 必须 -legacy
    # 回退老的 PKCS12 RC2-40 + SHA-1 (本地用安全够了)。
    # 但 macOS 自带 LibreSSL 不认识 -legacy 标志 (会报错且不生成 p12), 且其默认就是老格式 →
    # 仅对 OpenSSL 3.x 加 -legacy。不加引号: 空时不产生空参数 (兼容 bash 3.2 + set -u)。
    local P12_LEGACY=""
    if openssl version 2>/dev/null | grep -qi "^OpenSSL 3"; then
        P12_LEGACY="-legacy"
    fi
    openssl pkcs12 -export $P12_LEGACY -inkey "$KEY_FILE" -in "$CRT_FILE" \
        -out "$P12_FILE" -name "$CERT_NAME" -passout pass:"$P12_PASS" 2>&1 | tail -3 | sed 's/^/  /'
    [[ -f "$P12_FILE" ]] || { err "pkcs12 生成失败 (openssl 版本/参数不兼容?), 终止"; exit 1; }

    bold "4a. unlock login keychain (会弹一次密码框)"
    local KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
    security unlock-keychain "$KEYCHAIN" || {
        err "解锁失败. 请手动跑: security unlock-keychain ~/Library/Keychains/login.keychain-db"
        exit 1
    }

    bold "4b. import 到 login keychain (允许 codesign 直接用)"
    # -T /usr/bin/codesign: 把 codesign 加入私钥 ACL, 后续 codesign 不再弹框
    # -A: 允许所有应用使用此私钥 (开发期方便, 否则每次 codesign 都要点 Always Allow)
    security import "$P12_FILE" -k "$KEYCHAIN" \
        -P "$P12_PASS" -A 2>&1 | sed 's/^/  /'

    bold "5. 把证书加为 trusted code-signing root (这一步要 sudo)"
    # 没有 trust, codesign 用上后系统仍判 CSSMERR_TP_NOT_TRUSTED 等同 ad-hoc, IME 注册照样拒
    # -d: 加到 admin trust domain (System keychain)
    # -r trustRoot: 当 root CA trust
    # -p codeSign: 仅信任此 cert 的 code signing 用途, 不开成全能 root
    sudo security add-trusted-cert -d -r trustRoot -p codeSign \
        -k "/Library/Keychains/System.keychain" "$CRT_FILE" 2>&1 | sed 's/^/  /'

    bold "6. 验证 identity 可用 (Valid identities only 段应出现 \"$CERT_NAME\")"
    security find-identity -v -p codesigning | sed 's/^/  /'

    if security find-identity -v -p codesigning | grep -q "\"$CERT_NAME\""; then
        bold "成功"
        info "现在跑:"
        info "  SIGN_IDENTITY=\"$CERT_NAME\" scripts/mac/dev.sh build"
        info "  scripts/mac/dev.sh uninstall"
        info "  SIGN_IDENTITY=\"$CERT_NAME\" scripts/mac/dev.sh app"
    else
        err "证书仍未 valid. 看上面 add-trusted-cert 输出"
        exit 1
    fi

    rm -rf "$WORK_DIR"
}

# ───────────── pkg_build (原 pkg.sh: 打 .pkg 安装器) ─────────────
# pkg.sh — 把 IME (.app) + Rust 服务 (wind_input + data) [+ 可选 设置 app] 打成单个
# .pkg 安装器 (面向终端用户分发)。移植自旧 Go 仓 scripts_mac/build/pkg.sh。
#
# 为何 .pkg: 多组件 (输入法 + 后台服务 LaunchAgent [+ 设置 app]) 装到多个 per-user 目录,
# .pkg 的 payload + postinstall 是标准方案; postinstall 复用本脚本内联的 install_*.
#
# 产物: wind_macos/dist/WindInput-<版本>-macOS.pkg
#
# 用法:
#   scripts/mac/dev.sh pkg             # 用现有构建产物打包 (需先备齐 .app + release 服务 + data)
#   scripts/mac/dev.sh pkg --build     # 先构建 (cargo release + dev.sh gd + app_build) 再打包
#
# 设置 app (wind_setting.app) 可选: 存在则纳入并装到 ~/Applications, 不存在则跳过。
#
# 注意 (未公证版): .pkg 未签名 → Gatekeeper 首启拦截需绕过; macOS 26 Tahoe 对非公证
# IME 有系统设置 UI 硬墙, 真正可分发需 Developer ID + 公证 (下方预留环境变量)。
#
# 公证 (预留): 配齐则 productbuild 后自动 productsign + notarytool + staple:
#   MACOS_DEVELOPER_ID_INSTALLER / MACOS_NOTARY_APPLE_ID / MACOS_NOTARY_PASSWORD / MACOS_NOTARY_TEAM_ID
pkg_build() {
    local DEPLOY_DIR="$SCRIPT_DIR"   # 安装脚本/postinstall 资源与本脚本同目录 (scripts/mac)

    local APP_BUNDLE="$MACOS_DIR/build/WindInput.app"
    local SETTING_APP="$REPO_DIR/wind_setting/build/bin/wind_setting.app"   # 可选
    local SERVICE_BIN="$REPO_DIR/wind_input/target/release/wind_input"
    local SERVICE_DATA="$REPO_DIR/build_debug/data"                        # dev.sh gd 产物 (变体无关)

    local DIST_DIR="$MACOS_DIR/dist"
    local PKG_ID="to.feng.windinput.installer"
    local STAGE_REL="Library/Application Support/WindInputInstaller"

    local DO_BUILD=0
    local arg
    for arg in "$@"; do
        case "$arg" in
            --build) DO_BUILD=1 ;;
            *) echo "[错误] 未知参数: $arg" >&2; exit 1 ;;
        esac
    done

    # install/app 装完会删 build/WindInput.app（防 LaunchServices 重复登记），故 pkg 多半
    # 找不到现成 .app。缺 .app 时自动转构建模式（发行包本就应全新构建），免去手动 --build。
    if [[ $DO_BUILD -eq 0 && ! -d "$MACOS_DIR/build/WindInput.app" ]]; then
        info "未找到 build/WindInput.app → 自动构建 (等同 pkg --build)"
        DO_BUILD=1
    fi

    command -v pkgbuild >/dev/null || { err "pkgbuild 未找到 (macOS 自带 Xcode CLT)"; exit 1; }

    # -------- (可选) 构建 --------
    if [[ $DO_BUILD -eq 1 ]]; then
        bold "==> 构建 IME + 服务 + 词库"
        ( cd "$REPO_DIR" && cargo build --release --manifest-path wind_input/Cargo.toml -p wind_service )
        "$REPO_DIR/scripts/dev.sh" gd          # 组装 data/ → build_debug/data
        app_build                              # IME .app
    fi

    # -------- 校验必备产物 (设置 app 可选) --------
    local miss=0 p
    for p in "$APP_BUNDLE" "$SERVICE_BIN" "$SERVICE_DATA"; do
        [[ -e "$p" ]] || { err "缺产物: $p"; miss=1; }
    done
    [[ $miss -eq 0 ]] || { err "请先跑 scripts/mac/dev.sh pkg --build (或手动构建各组件)"; exit 1; }
    local HAVE_SETTING=0
    [[ -e "$SETTING_APP" ]] && HAVE_SETTING=1 || info "(无 wind_setting.app, 跳过设置组件)"

    local VERSION
    VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$APP_BUNDLE/Contents/Info.plist" 2>/dev/null || echo "0.0.0")
    local PKG_PATH="$DIST_DIR/WindInput-${VERSION}-macOS.pkg"

    # -------- 组 payload root --------
    bold "==> 组装 payload (版本 $VERSION)"
    # 不用 local：EXIT trap 在 pkg_build 返回后、脚本退出时才触发，若是 local 则那时已出
    # 作用域 → set -u 报 "PKGROOT: unbound variable"（产物其实已生成，仅清理时报错）。
    # 设为脚本级全局，trap 退出时仍可引用、正常清理临时暂存目录。
    PKGROOT=$(mktemp -d)
    SCRIPTS=$(mktemp -d)
    trap 'rm -rf "${PKGROOT:-}" "${SCRIPTS:-}" 2>/dev/null || true' EXIT

    local DEST="$PKGROOT/$STAGE_REL"
    mkdir -p "$DEST/service"
    cp -R "$APP_BUNDLE"   "$DEST/"
    cp    "$SERVICE_BIN"  "$DEST/service/wind_input"
    cp -R "$SERVICE_DATA" "$DEST/service/data"
    # 安装脚本: 原 install_app.sh / install_service.sh 已合并进 dev.sh。把 dev.sh 本身放进
    # payload, 再生成两个薄包装脚本 (postinstall 仍按 install_app.sh / install_service.sh
    # 这两个名字调用)。包装脚本 re-exec dev.sh 的内部入口 __app_install / __service_install,
    # 绕过 dev.sh 面向交互的 flag 解析, 完整保留原 install_* 的 --from/--data/--uninstall 行为。
    # 包装里把 SIGN_IDENTITY 默认设为空 (终端用户机无自签证书 → 走 ad-hoc 路径, 不继承
    # dev.sh 顶部的 "WindInput Dev" 默认; 若打包方在环境里显式设了证书则照常继承)。
    cp "$DEPLOY_DIR/dev.sh" "$DEST/dev.sh"
    cat > "$DEST/install_service.sh" <<'WRAP'
#!/bin/bash
export SIGN_IDENTITY="${SIGN_IDENTITY-}"
exec "$(cd "$(dirname "$0")" && pwd)/dev.sh" __service_install "$@"
WRAP
    cat > "$DEST/install_app.sh" <<'WRAP'
#!/bin/bash
export SIGN_IDENTITY="${SIGN_IDENTITY-}"
exec "$(cd "$(dirname "$0")" && pwd)/dev.sh" __app_install "$@"
WRAP
    local INSTALL_SCRIPTS="install_app.sh install_service.sh"
    if [[ $HAVE_SETTING -eq 1 ]]; then
        cp -R "$SETTING_APP" "$DEST/"
        [[ -f "$DEPLOY_DIR/install_setting.sh" ]] && { cp "$DEPLOY_DIR/install_setting.sh" "$DEST/"; INSTALL_SCRIPTS="$INSTALL_SCRIPTS install_setting.sh"; }
    fi
    chmod +x "$DEST"/*.sh "$DEST/service/wind_input"
    info "payload: WindInput.app + service(wind_input+data)$([[ $HAVE_SETTING -eq 1 ]] && echo ' + wind_setting.app') + 安装脚本"

    # -------- postinstall --------
    cp "$SCRIPT_DIR/pkg_resources/postinstall" "$SCRIPTS/postinstall"
    chmod +x "$SCRIPTS/postinstall"

    # -------- component plist: 关掉 BundleIsRelocatable --------
    local COMP="$SCRIPTS/components.plist"
    pkgbuild --analyze --root "$PKGROOT" "$COMP" >/dev/null
    /usr/bin/python3 - "$COMP" <<'PY'
import plistlib, sys
p = sys.argv[1]
with open(p, "rb") as f:
    arr = plistlib.load(f)
for c in arr:
    c["BundleIsRelocatable"] = False
with open(p, "wb") as f:
    plistlib.dump(arr, f)
PY
    info "已关闭 BundleIsRelocatable (锁定到暂存路径)"

    # -------- pkgbuild + productbuild --------
    bold "==> pkgbuild + productbuild"
    mkdir -p "$DIST_DIR"
    rm -f "$PKG_PATH"
    local HOST_ARCHS="arm64"   # 本机单架构; universal 分发需自行扩展为 arm64,x86_64
    info "hostArchitectures: $HOST_ARCHS"

    local COMPONENT_PKG="$SCRIPTS/WindInput-component.pkg"
    pkgbuild \
        --root "$PKGROOT" \
        --component-plist "$COMP" \
        --scripts "$SCRIPTS" \
        --identifier "$PKG_ID" \
        --version "$VERSION" \
        --install-location "/" \
        "$COMPONENT_PKG"

    local DIST_XML="$SCRIPTS/distribution.xml"
    cat > "$DIST_XML" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>清风输入法 $VERSION</title>
    <options customize="never" require-scripts="true" hostArchitectures="$HOST_ARCHS"/>
    <domains enable_anywhere="false" enable_currentUserHome="false" enable_localSystem="true"/>
    <choices-outline><line choice="default"/></choices-outline>
    <choice id="default" title="清风输入法"><pkg-ref id="$PKG_ID"/></choice>
    <pkg-ref id="$PKG_ID" version="$VERSION" onConclusion="none">$(basename "$COMPONENT_PKG")</pkg-ref>
</installer-gui-script>
XML

    productbuild \
        --distribution "$DIST_XML" \
        --package-path "$SCRIPTS" \
        "$PKG_PATH"

    # -------- (预留) Developer ID 签名 + 公证 --------
    local NOTARIZED=0
    if [[ -n "${MACOS_DEVELOPER_ID_INSTALLER:-}" ]]; then
        bold "==> productsign (Developer ID Installer)"
        local SIGNED_PKG="${PKG_PATH%.pkg}-signed.pkg"
        productsign --sign "$MACOS_DEVELOPER_ID_INSTALLER" "$PKG_PATH" "$SIGNED_PKG"
        mv -f "$SIGNED_PKG" "$PKG_PATH"
        info "已签名: $PKG_PATH"
        if [[ -n "${MACOS_NOTARY_APPLE_ID:-}" && -n "${MACOS_NOTARY_PASSWORD:-}" && -n "${MACOS_NOTARY_TEAM_ID:-}" ]]; then
            bold "==> notarytool submit --wait + stapler staple"
            xcrun notarytool submit "$PKG_PATH" \
                --apple-id "$MACOS_NOTARY_APPLE_ID" --password "$MACOS_NOTARY_PASSWORD" \
                --team-id "$MACOS_NOTARY_TEAM_ID" --wait
            xcrun stapler staple "$PKG_PATH"
            NOTARIZED=1
            info "已公证 + staple: $PKG_PATH"
        else
            info "(已签名但未配 notarytool 凭据, 跳过公证)"
        fi
    else
        info "(未配 MACOS_DEVELOPER_ID_INSTALLER, 保持 ad-hoc 产物)"
    fi

    bold "==> Done"
    info "PKG: $PKG_PATH ($(du -h "$PKG_PATH" | cut -f1))"
    info "安装: sudo installer -pkg \"$PKG_PATH\" -target /   (或双击走向导)"
    if [[ $NOTARIZED -eq 0 ]]; then
        info "(未公证版首启需 右键→打开 绕过 Gatekeeper; Tahoe 系统设置 UI 硬墙需公证才解)"
    fi
}

usage() {
    cat <<'EOF'
WindInput macOS 开发一站式脚本

用法: scripts/mac/dev.sh <命令> [--debug] [--data <dir>]

命令 (前缀缩写均可):
  install | i      编译 + 安装 service(Rust) + app(Swift)  ← 改完代码就跑它
  service | svc    编译 + 安装 service (改 Rust 渲染/协议/引擎时)
  app     | a      编译 + 安装 app     (改 Swift 显示/IMKit 时)
  build   | b      只编译两端 (service + .app bundle + codesign), 不安装
  run     | r      重启 service (kickstart, 不重编)
  logs    | l      跟踪 service + IME 日志
  status  | st     诊断: service pid / socket / 签名 / 进程
  data    | gd     把当前已装的 data/ 快照到 build_mac/data (作安装数据源)
  uninstall | rm   卸载 service + app
  sign-setup       命令行建自签证书 "WindInput Dev" (需 sudo + 钥匙串交互)
                   子命令: sign-setup [create|check|grant|remove]
  pkg              打 .pkg 安装器 (终端用户分发); pkg --build 先构建再打包
  help             显示本帮助

选项:
  --debug          debug 变体 (WindInputDebug + target/debug + debug_variant 特性,
                   与 release 共存; 默认 release = WindInput)。
  --data <dir>     指定词库数据源目录。

环境变量:
  SIGN_IDENTITY    codesign 身份 (默认 "WindInput Dev"; 设空则回退纯 ad-hoc)。
  WIND_MAC_UNIVERSAL=1   构建 arm64+x86_64 通用二进制 (分发/CI)。
  SIGN_KEYCHAIN_PW       无头 ssh 部署解锁 login keychain 用。
  MACOS_DEVELOPER_ID_INSTALLER / MACOS_NOTARY_*   pkg 签名 + 公证 (预留)。
EOF
}

# .pkg postinstall 经 payload 内的 install_app.sh / install_service.sh 薄包装脚本
# re-exec 本脚本时走这里: 原样把参数 (--from/--data/--uninstall) 交给内联函数, 绕过下方
# 面向交互的 flag 解析 (尤其全局 --data), 完整保留原 install_* 的参数行为。
case "${1:-}" in
    __service_install) shift; service_install "$@"; exit $? ;;
    __app_install)     shift; app_install "$@"; exit $? ;;
esac

CMD=""
PASS_ARGS=()
# 解析: 第一个非选项参数为命令; 全局 --debug/--data 任意位置生效; 其余参数 (如 pkg --build,
# sign-setup check) 收进 PASS_ARGS 透传给对应子命令函数。
while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)
            VARIANT="debug"
            CARGO_PROFILE_FLAG=""               # debug = 默认 profile
            CARGO_FEATURES=(--features debug_variant)
            APP_VARIANT_FLAG=(--debug)
            APP_SUPPORT="$HOME/Library/Application Support/WindInput_debug"
            LABEL="to.feng.windinput.service.debug"
            ;;
        --data) shift; DATA_OVERRIDE="${1:-}"; [[ -n "$DATA_OVERRIDE" ]] || { err "--data 缺目录"; exit 1; } ;;
        -h|--help) CMD="help" ;;
        *)  if [[ -z "$CMD" ]]; then CMD="$1"; else PASS_ARGS+=("$1"); fi ;;
    esac
    shift
done

INSTALLED_DATA="$APP_SUPPORT/service/data"

# ───────────────────────── 分发 ─────────────────────────
case "$CMD" in
    install|i)        install_service; install_app; bold "==> 完成 — 切到 WindInput 试输入" ;;
    service|svc|s)    install_service; do_run; bold "==> service 已更新重启" ;;
    app|a)            install_app; bold "==> app 已更新 (若 IMKit 未刷新, 切走再切回)" ;;
    build|b)          build_service; build_app; bold "==> 仅编译完成 (未安装)" ;;
    run|r)            do_run ;;
    logs|log|l)       do_logs ;;
    status|st)        do_status ;;
    data|gd)          do_data ;;
    uninstall|rm)     do_uninstall ;;
    sign-setup)       sign_setup ${PASS_ARGS[@]+"${PASS_ARGS[@]}"} ;;
    pkg)              pkg_build ${PASS_ARGS[@]+"${PASS_ARGS[@]}"} ;;
    help|"")          usage ;;
    *)                err "未知命令: $CMD"; usage; exit 1 ;;
esac
