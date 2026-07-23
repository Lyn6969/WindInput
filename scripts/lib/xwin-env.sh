#!/usr/bin/env bash
# ============================================================================
# xwin-env.sh — 统一 MSVC 交叉编译工具链的环境桥（dev.sh 与 pack-installer.sh 共用）
# ----------------------------------------------------------------------------
# cargo-xwin 按【无版本号】的名字搜索 clang-cl/lld-link/llvm-rc/llvm-lib/llvm-dlltool，
# 而 apt 的 llvm-<v> 包只提供带版本号的 llvm-lib-<v> 等。故在 $XWIN_BIN 建一层软链桥，
# 把无版本名逐个指向 -<v> 版本，使整条链（Rust + C 依赖 + 链接）统一走 clang-<v>。
#
# 【为什么必须共用这一份实现】
# 两个脚本写同一个 $XWIN_BIN 目录。若各自维护一份软链逻辑，后跑的会覆盖先跑的。
# 曾因此长期发版失败：pack-installer.sh 复制了一份，却把 clang-cl/lld-link/llvm-rc/
# llvm-lib/llvm-dlltool 五个名字一律软链到 clang，覆盖掉 dev.sh 建立的正确映射；
# 于是 wind-installer 的 zstd-sys 拿 clang 当归档器，报
#     llvm-lib: error: unknown argument: '-nologo'
# （前缀是 llvm-lib 只因 clang 打印 argv[0]）。主仓构建先跑故成功，安装器打包后跑故失败，
# 同一 job 内同版本 zstd-sys 一成一败，正是这个覆盖顺序造成的。
#
# 只有 clang-cl 可以指向 clang —— clang 按 argv[0] 进入 cl 兼容模式；
# 其余四个必须指向各自的 llvm 工具，否则参数风格对不上。
# ============================================================================

XWIN_BIN="${XWIN_BIN:-$HOME/.local/xwin-bin}"
WIND_LLVM_VER="${WIND_LLVM_VER:-19}"   # MSVC STL 要求 clang≥19；可覆盖切到 20

# 宿主脚本未提供 err() 时的纯文本兜底（pack-installer.sh 没有带颜色的输出函数）
command -v err >/dev/null 2>&1 || err() { printf '[ERROR] %s\n' "$*" >&2; }

setup_xwin_env() {
    local v="$WIND_LLVM_VER"
    if ! command -v "clang-$v" >/dev/null 2>&1; then
        err "未找到 clang-$v;请安装 clang-$v lld-$v llvm-$v(MSVC STL 要求 clang≥19)。"
        return 1
    fi
    if ! command -v cargo-xwin >/dev/null 2>&1; then
        err "未找到 cargo-xwin;请运行 'cargo install cargo-xwin'。"; return 1
    fi
    # 逐个映射，幂等。ln -sf 无条件覆盖 —— 不加"已存在就跳过"的守卫，正是为了让本函数
    # 成为 $XWIN_BIN 的唯一权威：任何一次调用后，软链一定是这里定义的这套。
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
