# ============================================================================
# wind_tsf — MinGW(Linux→Windows 交叉编译) Makefile
# ----------------------------------------------------------------------------
# 用 x86_64-w64-mingw32 工具链把 C++ TSF 核心层编译为 wind_tsf.dll，纳入本仓库
# 的 Linux 交叉编译链路（与 Rust 侧 x86_64-pc-windows-gnu 一致，无需 Windows/MSVC）。
# MSVC/Windows 原生构建仍可用 CMakeLists.txt（见 README.md）。
#
# 用法：
#   make                      # Release: build/wind_tsf.dll
#   make DEBUG_VARIANT=1      # 调试变体: build_debug/wind_tsf_debug.dll（独立 CLSID/管道/日志）
#   make OUTDIR=/path         # 指定输出目录
#   make VERSION=1.2.3        # 指定版本号（写入版本资源）
#   make clean
#
# 依赖：x86_64-w64-mingw32-g++ / -windres（mingw-w64 工具链）。
# MinGW 自带 TSF 头不完整，缺失的接口/GUID/常量由 include/mingw_tsf_compat.h 补齐
# （仅 __MINGW32__ 下生效）；详见 docs/redesign/tsf-migration.md。
# ============================================================================

# ---- 工具链 ----
TRIPLE   ?= x86_64-w64-mingw32
CXX      := $(TRIPLE)-g++
WINDRES  := $(TRIPLE)-windres

# ---- 变体 / 版本 / 输出 ----
DEBUG_VARIANT ?= 0
VERSION       ?= 0.0.0-dev

ifeq ($(DEBUG_VARIANT),1)
  TARGET   := wind_tsf_debug
  OUTDIR   ?= build_debug
  VARFLAGS := -DWIND_DEBUG_VARIANT
  DISPLAY_NAME := 清风输入法开发版
else
  TARGET   := wind_tsf
  OUTDIR   ?= build
  VARFLAGS :=
  DISPLAY_NAME := 清风输入法
endif

# 版本号拆分（X.Y.Z[-suffix] → MAJOR/MINOR/PATCH）
VER_CLEAN := $(firstword $(subst -, ,$(VERSION)))
VER_MAJOR := $(word 1,$(subst ., ,$(VER_CLEAN)))
VER_MINOR := $(word 2,$(subst ., ,$(VER_CLEAN)))
VER_PATCH := $(word 3,$(subst ., ,$(VER_CLEAN)))
VER_MAJOR := $(or $(VER_MAJOR),0)
VER_MINOR := $(or $(VER_MINOR),0)
VER_PATCH := $(or $(VER_PATCH),0)

# ---- 编译 / 链接选项 ----
OBJDIR := $(OUTDIR)/obj$(if $(filter 1,$(DEBUG_VARIANT)),_debug,)

CXXFLAGS := -std=c++17 -O2 -municode \
  -DUNICODE -D_UNICODE -D_WINDOWS -D_USRDLL $(VARFLAGS) \
  -D_WIN32_WINNT=0x0A00 -DWINVER=0x0A00 -DNTDDI_VERSION=0x0A000000 \
  -Iinclude -Isrc -include include/mingw_tsf_compat.h

# 静态链接 libgcc/libstdc++/winpthread，使 DLL 自包含（drop-in 到无 MinGW 运行时的 Windows）
LDFLAGS := -shared -municode -static -static-libgcc -static-libstdc++ \
  -Wl,--enable-stdcall-fixup
LIBS := -ladvapi32 -ldwrite -lgdi32 -lole32 -loleaut32 -lshlwapi -luser32 -luuid -lkernel32 -lshell32

WINDRESFLAGS := -c 65001 -I include -I res

SRCS := $(wildcard src/*.cpp)
OBJS := $(patsubst src/%.cpp,$(OBJDIR)/%.o,$(SRCS))

DLL     := $(OUTDIR)/$(TARGET).dll
DEF     := $(OBJDIR)/$(TARGET).def
VERRC   := $(OBJDIR)/version.rc
RESOBJ  := $(OBJDIR)/resource_rc.o
VEROBJ  := $(OBJDIR)/version_rc.o

.PHONY: all clean x86
all: $(DLL)
	@echo "✅ $(DLL) ($(VERSION))"

# x86(32 位)构建 → $(OUTDIR)/wind_tsf_x86.dll
# 32 位宿主程序(部分旧软件)需要对应位数的 TSF DLL 才能显示输入法。
# 用 i686 工具链 + 独立 objdir,命令行赋值覆盖 TARGET/OBJDIR(对 := 同样生效)。
x86:
	$(MAKE) TRIPLE=i686-w64-mingw32 TARGET=wind_tsf_x86 \
	        OBJDIR=$(OUTDIR)/obj_x86 VERSION=$(VERSION) all

# 目标文件
$(OBJDIR)/%.o: src/%.cpp | $(OBJDIR)
	$(CXX) $(CXXFLAGS) -c $< -o $@

# 模块定义文件（由 .def.in 生成，LIBRARY 名随 TARGET 变化）
$(DEF): wind_tsf.def.in | $(OBJDIR)
	sed -e 's/@TSF_TARGET_NAME@/$(TARGET)/g' $< > $@

# 版本资源（由 .rc.in 生成）
$(VERRC): res/version.rc.in | $(OBJDIR)
	sed -e 's/@APP_VERSION_MAJOR@/$(VER_MAJOR)/g' \
	    -e 's/@APP_VERSION_MINOR@/$(VER_MINOR)/g' \
	    -e 's/@APP_VERSION_PATCH@/$(VER_PATCH)/g' \
	    -e 's/@APP_VERSION_BUILD@/0/g' \
	    -e 's/@APP_VERSION_STR@/$(VERSION)/g' \
	    -e 's/@APP_DISPLAY_NAME@/$(DISPLAY_NAME)/g' $< > $@

$(RESOBJ): res/resource.rc | $(OBJDIR)
	$(WINDRES) $(WINDRESFLAGS) -i $< -o $@

$(VEROBJ): $(VERRC)
	$(WINDRES) $(WINDRESFLAGS) -i $< -o $@

# 链接 DLL
$(DLL): $(OBJS) $(RESOBJ) $(VEROBJ) $(DEF) | $(OUTDIR)
	$(CXX) $(LDFLAGS) -o $@ $(OBJS) $(RESOBJ) $(VEROBJ) $(DEF) $(LIBS)

$(OBJDIR) $(OUTDIR):
	mkdir -p $@

clean:
	rm -rf build/obj build/obj_x86 build_debug/obj_debug $(OBJDIR) \
	       build/wind_tsf.dll build/wind_tsf_x86.dll build_debug/wind_tsf_debug.dll
