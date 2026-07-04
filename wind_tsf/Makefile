# ============================================================================
# wind_tsf — clang + cargo-xwin SDK(Linux→Windows MSVC 交叉编译) Makefile
# ----------------------------------------------------------------------------
# 用 clang(--target=*-pc-windows-msvc)+ lld-link + llvm-rc，配合 cargo-xwin
# 下载的 MSVC CRT/Windows SDK(默认 ~/.cache/cargo-xwin/xwin)，把 C++ TSF 核心层
# 编译为 wind_tsf.dll。与 Rust 侧(cargo-xwin / x86_64-pc-windows-msvc)统一工具链。
# 用真 Windows SDK 头，故不再需要 MinGW 兼容垫片。
# MSVC/Windows 原生构建仍可用 CMakeLists.txt（见 README.md）。
#
# 用法：
#   make                      # Release x64: build/wind_tsf.dll
#   make x86                  # x86(32 位): build/wind_tsf_x86.dll（32 位宿主用）
#   make DEV_VARIANT=1      # 调试变体: build_dev/wind_tsf_dev.dll
#   make OUTDIR=/path         # 指定输出目录
#   make VERSION=1.2.3        # 指定版本号（写入版本资源）
#   make clean
#
# 依赖：clang-19+ / lld-19 / llvm-19（llvm-rc）。MSVC STL 要求 clang≥19。
#   MSVC CRT/SDK 由 cargo-xwin 下载到 XWIN_ROOT，需含 x86_64 + x86 两种 splat
#   （cargo-xwin 构建时设 XWIN_ARCH=x86,x86_64；dev.sh 的 setup_xwin_env 已默认设置）。
# ============================================================================

# ---- 工具链 ----
CLANG     ?= clang++-19
LLVM_RC   ?= llvm-rc-19
XWIN_ROOT ?= $(HOME)/.cache/cargo-xwin/xwin

# ---- 变体 / 版本 ----
DEV_VARIANT ?= 0
VERSION       ?= 0.0.0-dev

# ---- 架构（ARCH=x64|x86；x86 也可用 `make x86`）----
ARCH ?= x64
ifeq ($(ARCH),x86)
  TRIPLE     := i686-pc-windows-msvc
  LIBARCH    := x86
  ARCHSUFFIX := _x86
else
  TRIPLE     := x86_64-pc-windows-msvc
  LIBARCH    := x86_64
  ARCHSUFFIX :=
endif

# 产物命名按 arch + variant 组合：wind_tsf[_x86][_dev].dll
ifeq ($(DEV_VARIANT),1)
  OUTDIR   ?= build_dev
  VARFLAGS := -DWIND_DEV_VARIANT
  DBGSUFFIX := _dev
else
  OUTDIR   ?= build
  VARFLAGS :=
  DBGSUFFIX :=
endif
# 命名顺序对齐 Go 版：base + _x86 + _dev（如 wind_tsf_x86_dev.dll）
TARGET := wind_tsf$(ARCHSUFFIX)$(DBGSUFFIX)

# 版本号拆分（X.Y.Z[-suffix] → MAJOR/MINOR/PATCH）
VER_CLEAN := $(firstword $(subst -, ,$(VERSION)))
VER_MAJOR := $(word 1,$(subst ., ,$(VER_CLEAN)))
VER_MINOR := $(word 2,$(subst ., ,$(VER_CLEAN)))
VER_PATCH := $(word 3,$(subst ., ,$(VER_CLEAN)))
VER_MAJOR := $(or $(VER_MAJOR),0)
VER_MINOR := $(or $(VER_MINOR),0)
VER_PATCH := $(or $(VER_PATCH),0)

# 中间产物目录（按 arch+variant 隔离）；可被 dev.sh 重定向到 .cache 以保持 OUTDIR 干净
OBJDIR ?= $(OUTDIR)/obj/$(ARCH)$(DBGSUFFIX)

# ---- SDK include / lib 路径（cargo-xwin splat 布局：crt/{include,lib}, sdk/{include,lib}）----
INCFLAGS := -isystem $(XWIN_ROOT)/crt/include \
            -isystem $(XWIN_ROOT)/sdk/include/um \
            -isystem $(XWIN_ROOT)/sdk/include/shared \
            -isystem $(XWIN_ROOT)/sdk/include/ucrt
LIBFLAGS := -Xlinker /LIBPATH:$(XWIN_ROOT)/crt/lib/$(LIBARCH) \
            -Xlinker /LIBPATH:$(XWIN_ROOT)/sdk/lib/um/$(LIBARCH) \
            -Xlinker /LIBPATH:$(XWIN_ROOT)/sdk/lib/ucrt/$(LIBARCH)

# ---- 编译 / 链接选项 ----
# -fms-runtime-lib=static：静态链接 MSVC CRT，DLL 自包含（无需目标机装 VC++ 运行库）。
# 不带 -g：无调试信息，DLL 本体即已精简（无需再 strip）。
CXXFLAGS := --target=$(TRIPLE) -fms-runtime-lib=static -std=c++17 -O2 \
  -DUNICODE -D_UNICODE -D_WINDOWS -D_USRDLL $(VARFLAGS) \
  -D_WIN32_WINNT=0x0A00 -DWINVER=0x0A00 -DNTDDI_VERSION=0x0A000000 \
  -Iinclude -Isrc $(INCFLAGS)

LDFLAGS := --target=$(TRIPLE) -fms-runtime-lib=static -fuse-ld=lld -shared \
  $(LIBFLAGS) -Xlinker /OPT:REF,ICF

LIBS := -ladvapi32 -ldwrite -lgdi32 -lole32 -loleaut32 -lshlwapi -luser32 -luuid -lkernel32 -lshell32

# llvm-rc 需 SDK include 解析 <winver.h> 等
RCFLAGS := /C 65001 /I include /I res \
  /I $(XWIN_ROOT)/sdk/include/um /I $(XWIN_ROOT)/sdk/include/shared /I $(XWIN_ROOT)/crt/include

SRCS := $(wildcard src/*.cpp)
OBJS := $(patsubst src/%.cpp,$(OBJDIR)/%.o,$(SRCS))

DLL     := $(OUTDIR)/$(TARGET).dll
DEF     := $(OBJDIR)/$(TARGET).def
VERRC   := $(OBJDIR)/version.rc
RESOBJ  := $(OBJDIR)/resource.res
VEROBJ  := $(OBJDIR)/version.res

.PHONY: all clean x86
all: $(DLL)
	@echo "✅ $(DLL) ($(VERSION))"

# x86(32 位)构建 → $(OUTDIR)/wind_tsf_x86[_dev].dll
# 32 位宿主程序(部分旧软件)需要对应位数的 TSF DLL 才能显示输入法。
# 透传 DEV_VARIANT/OUTDIR/VERSION，使 release/debug 与输出目录一致。
x86:
	$(MAKE) ARCH=x86 DEV_VARIANT=$(DEV_VARIANT) OUTDIR=$(OUTDIR) VERSION=$(VERSION) all

# 目标文件
$(OBJDIR)/%.o: src/%.cpp | $(OBJDIR)
	$(CLANG) $(CXXFLAGS) -c $< -o $@

# 模块定义文件（由 .def.in 生成，LIBRARY 名随 TARGET 变化）
$(DEF): wind_tsf.def.in | $(OBJDIR)
	sed -e 's/@TSF_TARGET_NAME@/$(TARGET)/g' $< > $@

# 版本资源（由 .rc.in 生成）
$(VERRC): res/version.rc.in | $(OBJDIR)
	sed -e 's/@APP_VERSION_MAJOR@/$(VER_MAJOR)/g' \
	    -e 's/@APP_VERSION_MINOR@/$(VER_MINOR)/g' \
	    -e 's/@APP_VERSION_PATCH@/$(VER_PATCH)/g' \
	    -e 's/@APP_VERSION_BUILD@/0/g' \
	    -e 's/@APP_VERSION_STR@/$(VERSION)/g' $< > $@

# 资源编译（llvm-rc → .res）
$(RESOBJ): res/resource.rc | $(OBJDIR)
	$(LLVM_RC) $(RCFLAGS) /FO $@ $<

$(VEROBJ): $(VERRC)
	$(LLVM_RC) $(RCFLAGS) /FO $@ $<

# 链接 DLL（.res 作为位置参数交给 lld-link；/DEF 指定未装饰 COM 导出）
# /IMPLIB 重定向到 OBJDIR：导入库 .lib/.exp 是中间产物（DLL 经 COM 加载，运行时不需要），
# 不让它们落进 OUTDIR，保证发布目录 == 安装内容。
$(DLL): $(OBJS) $(RESOBJ) $(VEROBJ) $(DEF) | $(OUTDIR)
	$(CLANG) $(LDFLAGS) -o $@ $(OBJS) $(RESOBJ) $(VEROBJ) \
		-Xlinker /DEF:$(DEF) -Xlinker /IMPLIB:$(OBJDIR)/$(TARGET).lib $(LIBS)

$(OBJDIR) $(OUTDIR):
	mkdir -p $@

clean:
	rm -rf build/obj build_dev/obj $(OBJDIR) \
	       build/wind_tsf*.dll build_dev/wind_tsf*.dll
