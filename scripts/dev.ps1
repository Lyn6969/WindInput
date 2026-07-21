# WindInput 开发菜单 (Windows 原生构建 / MSVC)
#
# 用法:
#   .\scripts\dev.ps1            # 交互式菜单 (对齐 dev.sh)
#   .\scripts\dev.ps1 <命令>     # 非交互直调, 如 .\scripts\dev.ps1 release
#   (dev.bat 已转发 %*, 故 dev.bat release / dev.bat m2 等价)
#
# 本机 (Windows) 原生构建:
#   - Rust(wind_input): cargo build --release (host = x86_64-pc-windows-msvc)
#   - Rust(../wind-portable): 独立仓库, 不存在则跳过便携启动器
#   - Rust(../wind-setting):  独立仓库, 不存在则跳过设置程序
#   - C++ TSF: CMake + "Visual Studio 17 2022" 生成器 (x64 + Win32, 自动定位 MSVC)
#   - 词库数据: 下载 rime-frost/pinyin-data/OpenCC + 生成 unigram/pinyin_map + 编 octrie
#   - 全构建产物落【产品根】build/(release) 或 build_dev/(dev); 内容 == 安装内容
#
# 命令 (菜单与命令行直调同一套; 前缀 d=dev, p=push/部署, m=单模块):
#   1            Release 全构建: wind_input + tsf(x64/x86) + setting + portable + 词库数据 → build/
#   d1           Dev 全构建 → build_dev/
#   m1 / dm1     仅 tsf (x64+x86)            release / dev
#   m2 / dm2     仅 wind_input (核心 exe)     release / dev
#   m3 / dm3     仅 wind_setting (../wind-setting)              release / dev (不存在则跳过)
#   m4 / dm4     仅 wind_portable (绿色版, ../wind-portable)   release / dev (不存在则跳过)
#   p1 / pd1     系统安装全部 (release / dev): 复制 + 注册 TSF + 开机自启 + 启动服务
#   u1/u / ud1/ud  系统卸载全部 (release / dev): 反注册 + 移出输入法列表 + 移除自启 + 删目录
#   pm1 / pm2    系统安装单模块 (tsf / 核心, release)
#   pdm1 / pdm2  系统安装单模块 (dev)
#   8  / d8      生成安装包 (release / dev): 全构建 + wind-installer 打包 → dist\*-Setup.exe
#   8s / d8s     生成安装包 (跳过重建, 直接打包现有 build[_dev]/)
#   k=check  l=clippy  t=test  f=fmt  fmt-check  ci(=fmt+clippy+test)  clean
#   gd=gen-data  r=repl
#
# 部署目标 (Go 非便携式系统安装; 默认在 Program Files 下, 部署自动 UAC 提权;
# 在 scripts\deploy.local.ps1 覆盖, PowerShell 赋值格式):
#   DeployDirRelease = C:\Program Files\WindInput      # p1 / pm* 目标
#   DeployDirDev   = C:\Program Files\WindInputDev   # pd1 / pdm* 目标
#
# 数据目录说明:
#   data/                源文件(入库): 配置、五笔词库、主题等手工维护文件
#   .cache/              外部下载/生成(gitignore): rime-frost、opencc、unigram 等
#   build/ build_dev/  全构建产物(gitignore); 内容即部署到目标目录的内容

param(
    # 支持连续命令: .\dev.ps1 d1 pd1 (前者失败则后者不执行)
    # repl 命令后接数据路径: .\dev.ps1 r build_dev/data
    [Parameter(Position = 0, ValueFromRemainingArguments)] [string[]]$Commands = @()
)

$ErrorActionPreference = "Stop"

# ---------- 路径 ----------
# 目录层级: <产品仓>\scripts\dev.ps1
#   ScriptDir   = <产品仓>\scripts
#   ProductRoot = <产品仓>            (含 docs\VERSION、data\、.cache\ 等)
#   ProjectRoot = <产品仓>\wind_input (Cargo workspace 根)
$ScriptDir     = $PSScriptRoot
$ProductRoot   = Split-Path $ScriptDir -Parent
$ProjectRoot   = "$ProductRoot\wind_input"
$TsfDir        = "$ProductRoot\wind_tsf"      # C++ TSF 核心层 (CMake/MSVC)
$SettingDir    = [System.IO.Path]::GetFullPath("$ProductRoot\..\wind-setting")  # 设置程序 (独立仓库)
$PortableDir   = [System.IO.Path]::GetFullPath("$ProductRoot\..\wind-portable") # 绿色版启动器 (独立仓库)
$Version       = (Get-Content "$ProductRoot\docs\VERSION" -Raw).Trim()
$BuildDir      = "$ProductRoot\build"
$BuildDevDir = "$ProductRoot\build_dev"
$CacheDir      = "$ProductRoot\.cache"        # 外部下载/生成 (不入库)
$DistDir       = "$ProductRoot\dist"          # 安装包输出目录 (gitignore)

# ---------- 部署目标 (Go 便携式: 复制到指定本地目录) ----------
$DeployDirRelease = "C:\Program Files\WindInput"
$DeployDirDev   = "C:\Program Files\WindInputDev"
# wind-installer: 通用安装器生成器 (兄弟项目, app.toml 驱动); 8/d8 打包命令调用其 pack.ps1。
$InstallerDir  = "$ProductRoot\..\wind-installer"
# 在线升级元数据里的下载地址前缀 (不含结尾斜杠); 打包后生成的 latest*.json 据此拼 exeUrl。
$CdnBase       = "https://dl.windinput.com"
# 可在 scripts\deploy.local.ps1 覆盖上述变量 (PowerShell 赋值语法; 该文件 gitignore)。
$deployCfg = "$ScriptDir\deploy.local.ps1"
if (Test-Path $deployCfg) { . $deployCfg }

# ---------- 输出辅助 ----------
function Say  ([string]$m) { Write-Host $m -ForegroundColor Green }
function Warn ([string]$m) { Write-Host $m -ForegroundColor Yellow }
function ErrMsg ([string]$m) { Write-Host $m -ForegroundColor Red }
function Gray ([string]$m) { Write-Host $m -ForegroundColor DarkGray }

# release → BUILD_DIR; dev → BUILD_DEV_DIR
function Out-For ([string]$profile) { if ($profile -eq "dev") { $BuildDevDir } else { $BuildDir } }

# ---------- 构建: 核心 exe ----------
# dev 变体 = dev-variant profile（继承 dev + 关断言）:
#   ① debug_assertions 关闭 → windows_subsystem="windows" 生效, 无控制台窗口;
#   ② 优化构建, 输入法手感正常; ③ 仍是独立 _dev 身份 (管道/目录隔离)。
function Build-Core ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    $suffix = ""; $prof = "release"
    if ($profile -eq "dev") { $suffix = "_dev"; $prof = "dev-variant" }
    Say "`n[core] 构建 wind_input ($prof)..."
    Push-Location $ProjectRoot
    try {
        cargo build --profile $prof -p wind_service
        if ($LASTEXITCODE -ne 0) { ErrMsg "wind_input 构建失败!"; return $false }
    } finally { Pop-Location }
    $src = "$ProjectRoot\target\$prof\wind_input.exe"
    if (-not (Test-Path $src)) { ErrMsg "未找到产物: $src"; return $false }
    Copy-Item $src "$outdir\wind_input$suffix.exe" -Force
    $sz = [math]::Round((Get-Item "$outdir\wind_input$suffix.exe").Length / 1MB, 1)
    Gray "已构建: wind_input$suffix.exe (${sz}MB)"
    # CLI 包装器 (wind_input config ...; 运行时自辨 dev/release exe, 两变体共用一份)
    $cli = "$ProjectRoot\scripts\wind_cli.bat"
    if (Test-Path $cli) { Copy-Item $cli "$outdir\wind_cli.bat" -Force; Gray "已复制: wind_cli.bat" }
    return $true
}

# ---------- 构建: C++ TSF DLL (x64 + x86; CMake/MSVC) ----------
# CMakeLists 把 DLL 写死输出到 ..\build[_dev], x86/x64 同名 wind_tsf.dll。
# 故先编 x86 → 改名 _x86, 再编 x64 (保留无后缀名), 避免互相覆盖。
function Build-TsfAll ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
        Warn "未找到 cmake; 跳过 TSF (安装 CMake + VS2022 C++ 工具后可构建)。"; return $true
    }
    $suffix = ""; $dvFlag = "OFF"
    if ($profile -eq "dev") { $suffix = "_dev"; $dvFlag = "ON" }
    # 解析版本号 (写入版本资源)
    $vp = ($Version -split '[.\-]')
    $vMaj = if ($vp.Count -ge 1) { $vp[0] } else { "0" }
    $vMin = if ($vp.Count -ge 2) { $vp[1] } else { "0" }
    $vPat = if ($vp.Count -ge 3) { $vp[2] } else { "0" }
    Say "`n[tsf] CMake 交叉构建 x64 + x86 ($profile, VS2022/MSVC)..."
    # arch: cmake -A 平台名 / 产物后缀
    $arches = @(
        @{ A = "Win32"; Sfx = "_x86" },   # 先 x86 → 改名
        @{ A = "x64";   Sfx = "" }        # 后 x64 → 保留无后缀
    )
    foreach ($a in $arches) {
        $bin = "$CacheDir\tsf-cmake\$($a.A)$suffix"
        New-Item -ItemType Directory -Path $bin -Force | Out-Null
        cmake -S $TsfDir -B $bin -G "Visual Studio 17 2022" -A $a.A `
            "-DWIND_DEV_VARIANT=$dvFlag" `
            "-DAPP_VERSION_STR=$Version" `
            "-DAPP_VERSION_MAJOR=$vMaj" "-DAPP_VERSION_MINOR=$vMin" "-DAPP_VERSION_PATCH=$vPat" `
            | Out-Null
        if ($LASTEXITCODE -ne 0) { ErrMsg "TSF $($a.A) CMake 配置失败!"; return $false }
        # MSBuild 的编译警告走 stdout, 整条 | Out-Null 会连警告一起吞掉 (C++ 侧等于零编译期
        # 信号)。故只丢进度噪音, 保留 warning/error 行原样打出。Select-String 不改 $LASTEXITCODE
        # (它由最后一个原生命令 cmake 设定), 下面的失败判定照常成立。
        cmake --build $bin --config Release |
            Select-String -Pattern 'warning|error|警告|错误' |
            ForEach-Object { Warn "  $($_.Line.Trim())" }
        if ($LASTEXITCODE -ne 0) { ErrMsg "TSF $($a.A) 构建失败!"; return $false }
        # CMakeLists 输出到 $outdir\wind_tsf$suffix.dll; x86 需改名加 _x86
        $produced = "$outdir\wind_tsf$suffix.dll"
        # 末尾化: 架构后缀在前, 变体后缀在后 → wind_tsf_x86_dev.dll
        $final    = "$outdir\wind_tsf$($a.Sfx)$suffix.dll"
        if ((Test-Path $produced) -and ($produced -ne $final)) {
            Move-Item $produced $final -Force
        }
    }
    # 清理 CMake 顺带产出的导入库/导出表, 保持 outdir == 安装内容
    Get-ChildItem -Path $outdir -Include "*.lib", "*.exp" -File -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    $dlls = (Get-ChildItem -Path $outdir -Filter "wind_tsf*.dll" -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join " "
    Gray "已构建: $dlls"
    return $true
}

# ---------- 构建: wind-setting (设置程序) ----------
# 独立仓库; 不存在时跳过。dev 变体产物重命名为 wind_setting_dev.exe。
function Build-Setting ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    if (-not (Test-Path $SettingDir)) { Warn "../wind-setting 仓库不存在, 跳过设置程序。"; return $true }
    $suffix = ""; $targetDir = "release"
    if ($profile -eq "dev") { $suffix = "_dev"; $targetDir = "debug" }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    Say "`n[setting] 构建 wind_setting ($profile)..."
    $env:WIND_APP_VERSION = $Version   # 版本注入: docs/VERSION → wind-setting (与主仓统一)
    Push-Location $SettingDir
    try {
        if ($profile -eq "dev") { cargo build } else { cargo build --release }
        if ($LASTEXITCODE -ne 0) { ErrMsg "wind_setting 构建失败!"; return $false }
    } finally { Pop-Location }
    $exe = "$SettingDir\target\$targetDir\wind_setting.exe"
    if (-not (Test-Path $exe)) { ErrMsg "未找到产物: $exe"; return $false }
    Copy-Item $exe "$outdir\wind_setting$suffix.exe" -Force
    $sz = [math]::Round((Get-Item "$outdir\wind_setting$suffix.exe").Length / 1MB, 1)
    Gray "已构建: wind_setting$suffix.exe (${sz}MB)"
    return $true
}

# ---------- 构建: wind-portable (绿色版便携启动器) ----------
# 独立仓库; 不存在时跳过。dev/release 产出同一份 exe。
function Build-Portable ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    if (-not (Test-Path $PortableDir)) { Warn "../wind-portable 仓库不存在, 跳过便携启动器。"; return $true }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    Say "`n[portable] 构建 wind_portable ($profile → 单一二进制)..."
    $env:WIND_APP_VERSION = $Version   # 版本注入: docs/VERSION → wind-portable (与主仓统一)
    Push-Location $PortableDir
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { ErrMsg "wind_portable 构建失败!"; return $false }
    } finally { Pop-Location }
    $exe = "$PortableDir\target\release\wind_portable.exe"
    if (-not (Test-Path $exe)) { ErrMsg "未找到产物: $exe"; return $false }
    Copy-Item $exe "$outdir\wind_portable.exe" -Force
    $sz = [math]::Round((Get-Item "$outdir\wind_portable.exe").Length / 1MB, 1)
    Gray "已构建: wind_portable.exe (${sz}MB)"
    return $true
}

# ---------- 代码质量 ----------
function Do-Check  { Say "`n正在运行 cargo check (全工作区)...";  Push-Location $ProjectRoot; try { cargo check --workspace }  finally { Pop-Location } }
function Do-Clippy { Say "`n正在运行 cargo clippy (全工作区)..."; Push-Location $ProjectRoot; try { cargo clippy --workspace } finally { Pop-Location } }
function Do-Test   { Say "`n正在运行 cargo test (全工作区)...";   Push-Location $ProjectRoot; try { cargo test --workspace }   finally { Pop-Location } }
function Do-Fmt    { Say "`n正在运行 cargo fmt...";                Push-Location $ProjectRoot; try { cargo fmt }                finally { Pop-Location } }
function Do-FmtCheck { Say "`n正在运行 cargo fmt --check...";      Push-Location $ProjectRoot; try { cargo fmt --all -- --check } finally { Pop-Location } }
function Do-Clean  { Say "`n正在运行 cargo clean...";              Push-Location $ProjectRoot; try { cargo clean }              finally { Pop-Location } }

function Do-Ci {
    Push-Location $ProjectRoot
    try {
        Do-FmtCheck; if ($LASTEXITCODE -ne 0) { ErrMsg "fmt 检查失败!"; return $false }
        Do-Clippy;   if ($LASTEXITCODE -ne 0) { ErrMsg "clippy 失败!"; return $false }
        Do-Test;     if ($LASTEXITCODE -ne 0) { ErrMsg "test 失败!";   return $false }
    } finally { Pop-Location }
    Say "`nCI 全部通过 ✓"; return $true
}

# ---------- 词库下载 ----------
function Get-Dict ([string]$url, [string]$dst, [string]$desc = "") {
    if (Test-Path $dst) { Gray "[skip] $(Split-Path $dst -Leaf) 已存在"; return $true }
    Gray "[get ] $(Split-Path $dst -Leaf) $desc"
    # 用 PowerShell 原生下载 (Invoke-WebRequest), 静默进度条以提速; 最多重试 3 次。
    $old = $ProgressPreference; $ProgressPreference = "SilentlyContinue"
    try {
        for ($i = 1; $i -le 3; $i++) {
            try {
                Invoke-WebRequest -Uri $url -OutFile $dst -UseBasicParsing -TimeoutSec 120
                return $true
            } catch {
                if (Test-Path $dst) { Remove-Item $dst -Force -ErrorAction SilentlyContinue }  # 清理半截文件
                if ($i -eq 3) { ErrMsg "下载失败 ($i/3): $url`n  $($_.Exception.Message)"; return $false }
                Warn "下载重试 ($i/3): $(Split-Path $dst -Leaf)"
                Start-Sleep -Seconds 2
            }
        }
    } finally { $ProgressPreference = $old }
    return $false
}

function Download-Dicts {
    Say "`n下载外部词库 → $CacheDir"
    $rimeFrost   = "$CacheDir\rime-frost"
    $rimeFrostCn = "$rimeFrost\cn_dicts"
    $rimeFrostEn = "$rimeFrost\en_dicts"
    $opencc      = "$CacheDir\opencc\dictionaries"
    $pinyinData  = "$CacheDir\pinyin-data"
    $rimeWubi    = "$CacheDir\rime-wubi"
    foreach ($d in @($rimeFrostCn, $rimeFrostEn, $opencc, $pinyinData, $rimeWubi)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }

    $frostBase = "https://raw.githubusercontent.com/gaboolic/rime-frost/master"
    Gray "rime-frost (拼音):"
    Get-Dict "$frostBase/rime_frost.dict.yaml"           "$rimeFrost\rime_frost.dict.yaml"      "词库入口"     | Out-Null
    Get-Dict "$frostBase/cn_dicts/8105.dict.yaml"        "$rimeFrostCn\8105.dict.yaml"          "单字词库"     | Out-Null
    Get-Dict "$frostBase/cn_dicts/41448.dict.yaml"       "$rimeFrostCn\41448.dict.yaml"         "扩展字表"     | Out-Null
    Get-Dict "$frostBase/cn_dicts/base.dict.yaml"        "$rimeFrostCn\base.dict.yaml"          "基础词库"     | Out-Null
    Get-Dict "$frostBase/cn_dicts/ext.dict.yaml"         "$rimeFrostCn\ext.dict.yaml"           "扩展词库"     | Out-Null
    Get-Dict "$frostBase/cn_dicts/others.dict.yaml"      "$rimeFrostCn\others.dict.yaml"        "容错词"       | Out-Null
    Get-Dict "$frostBase/cn_dicts/corrections.dict.yaml" "$rimeFrostCn\corrections.dict.yaml"   "错音词"       | Out-Null
    Get-Dict "$frostBase/cn_dicts/tencent.dict.yaml"     "$rimeFrostCn\tencent.dict.yaml"       "腾讯词频"     | Out-Null

    Gray "rime-frost (英文):"
    Get-Dict "$frostBase/en_dicts/en.dict.yaml"     "$rimeFrostEn\en.dict.yaml"     "主词库" | Out-Null
    Get-Dict "$frostBase/en_dicts/en_ext.dict.yaml" "$rimeFrostEn\en_ext.dict.yaml" "扩展"   | Out-Null

    $pinyinBase = "https://raw.githubusercontent.com/mozillazg/pinyin-data/master"
    Gray "pinyin-data (汉字拼音反查):"
    Get-Dict "$pinyinBase/pinyin.txt"         "$pinyinData\pinyin.txt"         "全量底表(官方合成)" | Out-Null
    Get-Dict "$pinyinBase/kXHC1983.txt"       "$pinyinData\kXHC1983.txt"       "新华字典多音字" | Out-Null
    Get-Dict "$pinyinBase/kTGHZ2013.txt"      "$pinyinData\kTGHZ2013.txt"      "通用规范汉字"   | Out-Null
    Get-Dict "$pinyinBase/kMandarin_8105.txt" "$pinyinData\kMandarin_8105.txt" "8105 标准首音"  | Out-Null
    Get-Dict "$pinyinBase/overwrite.txt"      "$pinyinData\overwrite.txt"      "手工纠正"       | Out-Null

    # 五笔词库: 下载上游原始档, 主库与 extra 由 gen_dict 重排/拆分后写入 build 目录;
    # district 不经 gen_dict, 原样复制 (见 Assemble-Data)
    $wubiBase = "https://raw.githubusercontent.com/KyleBing/rime-wubi86-jidian/master"
    Gray "rime-wubi86-jidian (五笔):"
    Get-Dict "$wubiBase/wubi86_jidian.dict.yaml"                "$rimeWubi\wubi86_jidian.dict.yaml"                "主词库"     | Out-Null
    Get-Dict "$wubiBase/wubi86_jidian_extra.dict.yaml"          "$rimeWubi\wubi86_jidian_extra.dict.yaml"          "扩展词库"   | Out-Null
    Get-Dict "$wubiBase/wubi86_jidian_extra_district.dict.yaml" "$rimeWubi\wubi86_jidian_extra_district.dict.yaml" "行政区域"   | Out-Null

    $openccBase = "https://raw.githubusercontent.com/BYVoid/OpenCC/master/data/dictionary"
    Gray "OpenCC 简繁词典:"
    Get-Dict "$openccBase/STCharacters.txt" "$opencc\STCharacters.txt" "简->繁 字级" | Out-Null
    Get-Dict "$openccBase/STPhrases.txt"    "$opencc\STPhrases.txt"    "简->繁 词级" | Out-Null
    Get-Dict "$openccBase/TWVariants.txt"   "$opencc\TWVariants.txt"   "台湾字形"   | Out-Null
    Get-Dict "$openccBase/TWPhrases.txt"    "$opencc\TWPhrases.txt"    "台湾词汇"   | Out-Null
    Get-Dict "$openccBase/HKVariants.txt"   "$opencc\HKVariants.txt"   "香港字形"   | Out-Null
    return $true
}

# 从 data/(源) + .cache/(下载/生成) 组装完整运行时数据到 $outdir\data\
function Assemble-Data ([string]$outdir = $BuildDevDir) {
    $data      = "$outdir\data"
    $schemas   = "$data\schemas"
    $pinyin    = "$schemas\pinyin"
    $pinyinCn  = "$pinyin\cn_dicts"
    $english   = "$schemas\english"
    $rimeFrost = "$CacheDir\rime-frost"

    Say "`n组装 data/ → $data"
    if (Test-Path $data) { Remove-Item -Recurse -Force $data }

    # 1. 复制 data/ 源文件 (configs、五笔词库、主题等)
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    Copy-Item "$ProductRoot\data" -Destination $outdir -Recurse -Force

    # 1b. 合并 wind_input\data\settings\ (manifest.toml 等 RPC 元数据)
    if (Test-Path "$ProjectRoot\data\settings") {
        New-Item -ItemType Directory -Path "$data\settings" -Force | Out-Null
        Copy-Item "$ProjectRoot\data\settings\*" -Destination "$data\settings" -Recurse -Force
    }

    # 2. rime-frost 拼音词库
    New-Item -ItemType Directory -Path $pinyinCn -Force | Out-Null
    if (Test-Path "$rimeFrost\rime_frost.dict.yaml") {
        Copy-Item "$rimeFrost\rime_frost.dict.yaml" $pinyin -Force
        foreach ($f in @("8105.dict.yaml", "41448.dict.yaml", "base.dict.yaml", "ext.dict.yaml", "others.dict.yaml", "corrections.dict.yaml")) {
            if (Test-Path "$rimeFrost\cn_dicts\$f") { Copy-Item "$rimeFrost\cn_dicts\$f" $pinyinCn -Force }
        }
    } else { Warn "缺 .cache\rime-frost\, 拼音词库不可用 (运行 gen-data 下载)" }

    # 3. 英文词库
    New-Item -ItemType Directory -Path $english -Force | Out-Null
    foreach ($f in @("en.dict.yaml", "en_ext.dict.yaml")) {
        if (Test-Path "$rimeFrost\en_dicts\$f") { Copy-Item "$rimeFrost\en_dicts\$f" $english -Force }
    }

    # 4. Unigram 语言模型
    $unigram = "$CacheDir\pinyin-frost\unigram.txt"
    if (Test-Path $unigram) { Copy-Item $unigram "$pinyin\unigram.txt" -Force }
    else { Warn "缺 unigram.txt (运行 gen-data 生成)" }

    # 4b. 汉字拼音反查表
    $pinyinMap = "$CacheDir\pinyin-data\pinyin_map.txt"
    if (Test-Path $pinyinMap) { Copy-Item $pinyinMap "$data\pinyin_map.txt" -Force }
    else { Warn "缺 pinyin_map.txt (运行 gen-data 生成)" }

    # 5. OpenCC 编译 .octrie (Rust 工具 gen_opencc)
    New-Item -ItemType Directory -Path "$data\opencc" -Force | Out-Null
    if ((Test-Path "$CacheDir\opencc\dictionaries") -and (Get-ChildItem "$CacheDir\opencc\dictionaries\*.txt" -ErrorAction SilentlyContinue)) {
        Gray "编译 OpenCC → .octrie ..."
        Push-Location $ProjectRoot
        try {
            cargo run -q -p wind-tools --bin gen_opencc -- --src "$CacheDir\opencc\dictionaries" --out "$data\opencc"
            if ($LASTEXITCODE -ne 0) { Warn "OpenCC 编译失败 (简繁转换不可用)" }
        } finally { Pop-Location }
    } else { Warn "缺 .cache\opencc\, OpenCC 不可用 (运行 gen-data 下载)" }

    # 6. 五笔词库 (Rust 工具 gen_dict): 主库按词频重排 + extra 拆成 4 库
    #    产物直接写进 build 目录, 不入版本库 —— 源码树 data\schemas\wubi86\ 只保留
    #    wubi86.schema.toml 与字体等真正的源文件, 避免再把生成物误当源文件手工编辑
    $wubiOut  = "$schemas\wubi86"
    $rimeWubi = "$CacheDir\rime-wubi"
    if (Test-Path "$rimeWubi\wubi86_jidian.dict.yaml") {
        Gray "生成五笔词库 (gen_dict) ..."
        New-Item -ItemType Directory -Path $wubiOut -Force | Out-Null
        Push-Location $ProjectRoot
        try {
            # district 由 gen_dict 的 passthrough 一并处理 (原样透传 + 清洗头部)
            cargo run -q -p wind-tools --bin gen_dict -- --cache $CacheDir --out $wubiOut --report $rimeWubi
            if ($LASTEXITCODE -ne 0) { Warn "五笔词库生成失败 (五笔方案不可用)" }
        } finally { Pop-Location }
    } else { Warn "缺 .cache\rime-wubi\, 五笔词库不可用 (运行 gen-data 下载)" }

    $cnt = (Get-ChildItem $data -Recurse -File).Count
    Gray "data/ 组装完成 ($cnt 文件)"
    return $true
}

# 下载外部词库 + 生成 unigram/pinyin + 组装 data/
function Do-GenData ([string]$outdir = $BuildDevDir) {
    if (-not (Download-Dicts)) { return $false }

    # 生成 Unigram 语言模型 (Rust 工具 gen_unigram)
    $unigram = "$CacheDir\pinyin-frost\unigram.txt"
    New-Item -ItemType Directory -Path (Split-Path $unigram -Parent) -Force | Out-Null
    if (-not (Test-Path $unigram)) {
        Say "生成 Unigram 语言模型..."
        Push-Location $ProjectRoot
        try {
            cargo run -q -p wind-tools --bin gen_unigram -- --rime "$CacheDir\rime-frost\cn_dicts" --out $unigram
            if ($LASTEXITCODE -ne 0) { Warn "Unigram 生成失败 (智能组句不可用)" }
        } finally { Pop-Location }
    } else { Gray "Unigram 已缓存" }

    # 生成汉字拼音反查表 (Rust 工具 gen_pinyin)
    $pinyinMap = "$CacheDir\pinyin-data\pinyin_map.txt"
    if (Test-Path "$CacheDir\pinyin-data\pinyin.txt") {
        Say "生成汉字拼音反查表..."
        Push-Location $ProjectRoot
        try {
            cargo run -q -p wind-tools --bin gen_pinyin -- --src "$CacheDir\pinyin-data" --out $pinyinMap
            if ($LASTEXITCODE -ne 0) { Warn "拼音反查表生成失败 (候选拼音提示不可用)" }
        } finally { Pop-Location }
    } else { Warn "缺 .cache\pinyin-data\, 拼音反查表不可用" }

    Assemble-Data $outdir | Out-Null
    Say "gen-data 完成 → $outdir\data"
    return $true
}

# 发布前硬门禁: 校验关键运行时数据完整 (缺失/过小即失败)
function Verify-DistData ([string]$outdir = $BuildDir) {
    $data = "$outdir\data"
    $ok = $true
    $checks = @(
        @{ Path = "schemas\pinyin\unigram.txt";            Min = 1000000 },
        @{ Path = "schemas\pinyin\cn_dicts\base.dict.yaml"; Min = 1000000 },
        @{ Path = "schemas\pinyin\cn_dicts\8105.dict.yaml"; Min = 10000 },
        @{ Path = "schemas\english\en.dict.yaml";           Min = 1000 },
        @{ Path = "pinyin_map.txt";                         Min = 10000 },
        # 五笔词库为 gen_dict 生成物, 不入版本库 —— 忘跑 gen-data 时必须在此拦下,
        # 否则打出来的包五笔方案整个不可用
        @{ Path = "schemas\wubi86\wubi86_jidian.dict.yaml";       Min = 1000000 },
        @{ Path = "schemas\wubi86\wubi86_jidian_extra.dict.yaml"; Min = 10000 },
        @{ Path = "schemas\wubi86\wubi86_jidian_emoji.dict.yaml"; Min = 1000 },
        @{ Path = "schemas\wubi86\wubi86_jidian_extra_district.dict.yaml"; Min = 10000 }
    )
    Say "`n校验发布数据完整性 → $data"
    foreach ($c in $checks) {
        $p = "$data\$($c.Path)"
        if (-not (Test-Path $p)) { ErrMsg "  ✗ 缺失: $($c.Path)"; $ok = $false; continue }
        $sz = (Get-Item $p).Length
        if ($sz -lt $c.Min) { ErrMsg "  ✗ 过小 (${sz}B < 期望 $($c.Min)B): $($c.Path)"; $ok = $false }
        else { Gray "  ✓ $($c.Path) ($([math]::Round($sz/1KB))KB)" }
    }
    $octrie = @(Get-ChildItem "$data\opencc\*.octrie" -ErrorAction SilentlyContinue | Where-Object { $_.Length -gt 0 })
    if ($octrie.Count -lt 1) { ErrMsg "  ✗ 缺失: opencc\*.octrie (简繁转换编译失败)"; $ok = $false }
    else { Gray "  ✓ opencc\*.octrie ($($octrie.Count) 个)" }

    if (-not $ok) {
        ErrMsg "`n发布数据校验失败! 上述文件缺失或异常会导致功能残缺。"
        ErrMsg "请排查 gen-data 的下载/生成 (词库源、网络、gen_unigram/gen_opencc/gen_dict)。"
        return $false
    }
    Say "发布数据校验通过 ✓"; return $true
}

# ---------- 全构建 (1 / d1) ----------
# 全部模块 + 数据落到【产品根】build/(release) 或 build_dev/(dev)。
# 先清空输出目录, 确保内容 == 部署到目标目录的内容, 无任何中间产物。
# ---------- 版本变化侦测: 版本号变更时强制重建关键产物 (确定性保险) ----------
# 产品版本唯一真源是 docs/VERSION。cargo 的 rerun-if-env-changed 与 CMake -D 已能在
# 版本变化时自动重建; 此处再加一道保险: 记录上次构建版本, 一旦变化即清理最终产物
# (Rust 最终二进制包 + TSF 的 CMake 缓存目录), 强制重新写入版本资源。仅版本真变时付代价。
function Sync-VersionStamp {
    $stampFile = "$CacheDir\.last_build_version"
    $lastVer = if (Test-Path $stampFile) { (Get-Content $stampFile -Raw).Trim() } else { "" }
    if ($lastVer -eq $Version) { return }   # 版本未变 → 走增量, 不清理

    if ($lastVer) { Say "`n[version] 版本变化 $lastVer -> $Version, 清理关键产物强制刷新版本号..." }
    else          { Say "`n[version] 首次记录版本 $Version, 清理关键产物确保版本号写入..." }

    # 1. Rust: 仅清最终二进制包 (依赖库保留, 秒级); build.rs 随之重跑注入新版本资源。
    # 注意: $ProjectRoot 已是 wind_input 目录 (见路径定义), 勿再拼 \wind_input。
    Push-Location $ProjectRoot
    try { cargo clean -p wind_service 2>&1 | Out-Null } catch {} finally { Pop-Location }
    if (Test-Path $SettingDir) {
        Push-Location $SettingDir
        try { cargo clean -p wind_setting 2>&1 | Out-Null } catch {} finally { Pop-Location }
    }
    if (Test-Path $PortableDir) {
        Push-Location $PortableDir
        try { cargo clean -p wind_portable 2>&1 | Out-Null } catch {} finally { Pop-Location }
    }

    # 2. TSF: 删 CMake 缓存目录, 强制 configure_file 重新生成 version.rc。
    $tsfCache = "$CacheDir\tsf-cmake"
    if (Test-Path $tsfCache) { Remove-Item -Recurse -Force $tsfCache -ErrorAction SilentlyContinue }

    # 记录当前版本, 避免下次重复清理。
    New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
    Set-Content -Path $stampFile -Value $Version -NoNewline
}

function Do-Full ([string]$profile = "release") {
    $outdir = Out-For $profile
    Sync-VersionStamp   # 版本号变化则强制重建关键产物 (确定性保险)
    Say "`n========== 全构建 ($profile) → $outdir =========="
    if (Test-Path $outdir) { Remove-Item -Recurse -Force $outdir }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    if (-not (Build-Core     $profile $outdir)) { return $false }   # wind_input[_dev].exe
    if (-not (Build-TsfAll   $profile $outdir)) { return $false }   # wind_tsf[_x86][_dev].dll
    if (-not (Build-Setting  $profile $outdir)) { return $false }   # wind_setting[_dev].exe (可选)
    if (-not (Build-Portable $profile $outdir)) { return $false }   # wind_portable.exe (可选)
    if (-not (Do-GenData     $outdir))          { return $false }   # data/
    if (-not (Verify-DistData $outdir))         { return $false }   # 硬门禁
    Say "`n========== 全构建完成 ($profile) → $outdir =========="
    Gray "内容即部署到目标目录的内容 (无中间产物)"
    return $true
}

# ---------- 部署 (Go 非便携式 / 系统安装) ----------
# 与便携式不同: 复制到安装目录后, regsvr32 注册 TSF COM (DllRegisterServer 自带
# AddLanguageProfile + RegisterCategories, 输入法直接进系统列表), 授权 AppContainer
# 宿主读取 DLL, 安装字根字体, 写开机自启, 直接启动 wind_input[_dev].exe (不靠
function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# 部署命令 → 目标安装目录; 非部署命令返回 $null (兼作"是否部署命令"判断)。
function Deploy-TargetForCmd ([string]$cmd) {
    if (@("p1","pm1","pm2","u1","u") -contains $cmd)           { return $DeployDirRelease }
    if (@("pd1","pdm1","pdm2","ud1","ud") -contains $cmd)      { return $DeployDirDev }
    return $null
}

# 系统安装(注册 COM/icacls/字体)始终需管理员。非管理员执行部署命令时自动 UAC 提权。
# 返回三态: "skip" = 非部署命令/已是管理员 (调用方本地执行);
#           "done" = 提权进程已执行完毕, 输出已在当前窗口显示 (调用方直接继续);
#           "fail" = 提权被取消/失败 (调用方报错并以非零码退出)。
function Invoke-Elevated ([string]$cmd, [string]$arg) {
    if (-not (Deploy-TargetForCmd $cmd)) { return "skip" }   # 非部署命令
    if (Test-Admin) { return "skip" }
    Warn "系统安装需要管理员权限, 正在请求 UAC 提升..."
    $host_exe = (Get-Process -Id $PID).Path   # pwsh.exe 或 powershell.exe
    if (-not $host_exe) { $host_exe = "pwsh.exe" }
    # 临时日志文件捕获提权子进程的全部输出流 (*>), 执行后读回在本窗口显示。
    $TmpLog = Join-Path $env:TEMP "wind_deploy_$(Get-Random -Maximum 99999999).log"
    $argPart = if ($arg) { " `"$arg`"" } else { "" }
    $innerCmd = "& `"$PSCommandPath`" `"$cmd`"$argPart *> `"$TmpLog`""
    $encodedCmd = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($innerCmd))
    try {
        # -PassThru 取得进程对象; 用 WaitForExit() 替代 -Wait 以确保子进程真正退出后再读日志。
        # (-Verb RunAs + -Wait 在部分 PS5.1 版本下存在不可靠的竞争问题)
        $proc = Start-Process -FilePath $host_exe -Verb RunAs -PassThru -ErrorAction Stop `
            -ArgumentList "-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-EncodedCommand", $encodedCmd
        $proc.WaitForExit()
        if (Test-Path $TmpLog) {
            Get-Content $TmpLog | ForEach-Object { Write-Host $_ }
            Remove-Item $TmpLog -ErrorAction SilentlyContinue
        }
        if ($proc.ExitCode -ne 0) { return "fail" }
        return "done"
    } catch {
        if (Test-Path $TmpLog) { Remove-Item $TmpLog -ErrorAction SilentlyContinue }
        ErrMsg "提权失败或被取消: $($_.Exception.Message)"
        ErrMsg "可【以管理员身份】重开 PowerShell 再运行本脚本。"
        return "fail"
    }
}

# 部署安全网: 非管理员 (如被直接调用未经提权) → 明确报错。
function Require-Admin {
    if (-not (Test-Admin)) {
        ErrMsg "系统安装需要管理员权限 (注册 TSF COM / 设置权限 / 安装字体)。"
        ErrMsg "请以【管理员身份】打开 PowerShell 后重试。"
        return $false
    }
    return $true
}

# 32 位 regsvr32 (注册 x86 TSF DLL, 写 WOW6432Node 供 32 位应用加载)。
function Get-Regsvr32X86 { Join-Path $env:SystemRoot "SysWOW64\regsvr32.exe" }

# 反注册安装目录中的旧 TSF COM (x64 + x86)。
function Unregister-Tsf ([string]$dir, [string]$suffix) {
    $x64 = Join-Path $dir "wind_tsf$suffix.dll"
    $x86 = Join-Path $dir "wind_tsf_x86${suffix}.dll"
    if (Test-Path $x64) { & regsvr32 /u /s $x64 2>$null }
    if (Test-Path $x86) { & (Get-Regsvr32X86) /u /s $x86 2>$null }
}

# 注册 TSF COM (x64 必须成功; x86 失败仅告警, 不阻断 64 位使用)。
function Register-Tsf ([string]$dir, [string]$suffix) {
    $x64 = Join-Path $dir "wind_tsf$suffix.dll"
    $x86 = Join-Path $dir "wind_tsf_x86${suffix}.dll"
    & regsvr32 /s $x64
    if ($LASTEXITCODE -ne 0) { ErrMsg "  - x64 COM 注册失败: $x64"; return $false }
    Gray "  - x64 COM 已注册"
    if (Test-Path $x86) {
        & (Get-Regsvr32X86) /s $x86
        if ($LASTEXITCODE -ne 0) { Warn "  - x86 COM 注册失败 (32 位应用可能无法使用输入法)" }
        else { Gray "  - x86 COM 已注册" }
    }
    return $true
}

# 将本变体 TSF 输入法加入【当前用户】中文(zh-CN)输入法列表 → 默认启用, 免去手动"添加键盘"。
# 背景: regsvr32/DllRegisterServer 只把 IME 注册为系统级"可用"; 对已配置好的语言, Windows
#       不会自动把新 TIP 追加进用户启用列表 (RegisterProfile 的 bEnabledByDefault 仅在该语言
#       【首次添加】时生效)。故此处显式追加。
# 注1: CLSID/Profile GUID 必须与 wind_tsf\src\Globals.cpp 一致 (dev=DEB0/DEB1, release=EE30/EE31)。
# 注2: 仅"添加"本变体, 绝不删除其它输入法 → 与系统已装的标准版清风/微软拼音等共存。
# 注3: 部署在管理员令牌下运行; 同账户 UAC 提升时 HKCU 仍指向本人, 故对当前用户生效。
function Enable-TsfForUser ([string]$profile) {
    if ($profile -eq "dev") {
        $tip = "0804:{99C2DEB0-5C57-45A2-9C63-FB54B34FD90A}{99C2DEB1-5C57-45A2-9C63-FB54B34FD90A}"
    } else {
        $tip = "0804:{99C2EE30-5C57-45A2-9C63-FB54B34FD90A}{99C2EE31-5C57-45A2-9C63-FB54B34FD90A}"
    }
    try {
        $list = Get-WinUserLanguageList
        $zh = $list | Where-Object { $_.LanguageTag -like "zh-Hans*" -or $_.LanguageTag -like "zh-CN*" } | Select-Object -First 1
        if (-not $zh) {
            $list.Add("zh-Hans-CN")
            $zh = $list | Where-Object { $_.LanguageTag -like "zh-Hans*" } | Select-Object -First 1
        }
        if ($zh -and ($zh.InputMethodTips -notcontains $tip)) {
            $zh.InputMethodTips.Add($tip)
            Set-WinUserLanguageList -LanguageList $list -Force
            Gray "  - 已加入当前用户输入法列表 (默认启用, 与标准版共存)"
        } else {
            Gray "  - 输入法已在用户列表, 跳过"
        }
    } catch {
        Warn "  - 自动启用输入法失败 (可在 设置>时间和语言>语言>中文>选项>键盘 手动添加): $($_.Exception.Message)"
    }
}

# 授权 ALL APPLICATION PACKAGES 读取执行 TSF DLL (开始菜单/搜索等 AppContainer 宿主需要)。
function Grant-TsfAcl ([string]$dir, [string]$suffix) {
    $sid = "*S-1-15-2-1"
    foreach ($n in @("wind_tsf$suffix.dll", "wind_tsf_x86${suffix}.dll")) {
        $p = Join-Path $dir $n
        if (Test-Path $p) { & icacls $p /grant "${sid}:(RX)" /c | Out-Null }
    }
}

# 安装 PUA 字根字体到系统 (供 DirectWrite fallback; 已存在且一致则跳过)。best-effort。
function Install-WubiFont ([string]$dir) {
    $src = Join-Path $dir "data\schemas\wubi86\HeiTiZiGen.ttf"
    if (-not (Test-Path $src)) { return }
    $dest = Join-Path $env:SystemRoot "Fonts\HeiTiZiGen.ttf"
    try {
        $need = $true
        if (Test-Path $dest) {
            try { if ((Get-FileHash $src -Algorithm SHA1).Hash -eq (Get-FileHash $dest -Algorithm SHA1).Hash) { $need = $false } } catch { $need = $true }
        }
        if ($need) { Copy-Item $src $dest -Force }
        Set-ItemProperty -Path "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts" -Name "黑体字根 (TrueType)" -Value "HeiTiZiGen.ttf" -Force
        Gray "  - 字体: 黑体字根 $(if($need){'已安装'}else{'已存在,跳过'})"
    } catch { Warn "  - 安装字体失败: $($_.Exception.Message)" }
}

# 写开机自启 (HKCU Run; 免管理员)。
function Set-AutoStart ([string]$dir, [string]$suffix) {
    $name = if ($suffix) { "WindInputDev" } else { "WindInput" }
    $exe  = Join-Path $dir "wind_input$suffix.exe"
    try {
        Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name $name -Value "`"$exe`"" -Force
        Gray "  - 已配置开机自启 ($name)"
    } catch { Warn "  - 配置开机自启失败" }
}

# 终止占用目标 exe 的进程 (按镜像名), 等其退出让出文件锁; 仅对 .exe 生效。
# 背景: Stop-WindService 只杀核心服务 wind_input; 独立打开的设置程序 wind_setting[_dev].exe /
#       便携版 wind_portable.exe 不随之退出, 覆盖前需先按名杀掉 (对齐 ../wind-setting Do-Copy 的处理)。
# DLL 由宿主进程加载, 没有独立进程可杀 → 跳过, 仍靠 Copy-Replace 的改名让路兜底。
function Stop-ProcessForFile ([string]$fileName) {
    if ($fileName -notmatch '\.exe$') { return }
    $procName = [System.IO.Path]::GetFileNameWithoutExtension($fileName)
    $procs = @(Get-Process -Name $procName -ErrorAction SilentlyContinue)
    if ($procs.Count -gt 0) {
        Gray "  - 终止运行中的 $fileName ($($procs.Count) 个进程)..."
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    }
}

# 复制单个文件, 处理被占用的 DLL/EXE。
# 顺序: ① 先杀占用该 exe 的进程 (如独立开着的设置程序) 并等待让出文件锁
#       ② 尝试覆盖 (= 删旧写新) ③ 仍被锁 (如已加载的 TSF DLL) 则改名让路再写。
function Copy-Replace ([string]$targetDir, [string]$fileName, [string]$srcPath) {
    $dst = Join-Path $targetDir $fileName
    if (-not (Test-Path $dst)) { Copy-Item $srcPath $dst -Force; Gray "  - $fileName"; return }
    Stop-ProcessForFile $fileName   # 先判断并杀进程等待, 再尝试覆盖; 覆盖失败才改名让路
    try { Copy-Item $srcPath $dst -Force -ErrorAction Stop; Gray "  - $fileName"; return } catch { }
    # 让路后缀必须每次唯一: NTFS 允许改名在用文件, 但不允许改名去【覆盖】一个在用文件。
    # 曾用固定 .old 槽复用, 结果上轮 .old 仍被宿主进程 map 着时 Move -Force 直接失败, 部署中断
    # (TSF DLL in-proc 常驻, 宿主不重启就一直锁旧代, 双代同锁是常态)。唯一后缀则目标必不存在,
    # 改名恒成功。垃圾累积由 Remove-OrRename 侧「不重复改名已让路文件」+ 各处 *.old* 清理消化。
    $old = "$dst.old_$(Get-Random -Maximum 99999999)"
    try {
        Move-Item $dst $old -Force -ErrorAction Stop
        Copy-Item $srcPath $dst -Force
        Gray "  - $fileName (旧文件已改名 $(Split-Path $old -Leaf))"
    } catch { ErrMsg "  [错误] 无法替换 ${fileName}: 旧文件被锁定且改名让路失败, 请重启后重试" }
}

function Stop-WindService ([string]$suffix) {
    Get-Process -Name "wind_input$suffix" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 600
}

# 系统安装: 全部 build[_dev]/ → 安装目录, 注册 TSF + 开机自启 + 启动服务 (p1 / pd1)。
function Deploy-Full ([string]$profile = "release") {
    $outdir = Out-For $profile
    $targetDir = if ($profile -eq "dev") { $DeployDirDev } else { $DeployDirRelease }
    $suffix = if ($profile -eq "dev") { "_dev" } else { "" }
    if (-not (Require-Admin)) { return $false }
    if (-not (Test-Path "$outdir\wind_input$suffix.exe")) {
        ErrMsg "无 $outdir 产物; 请先 '$(if($profile -eq 'dev'){'d1'}else{'1'})' 全构建。"; return $false
    }
    Say "`n========== 系统安装 ($profile) → $targetDir =========="
    Say "[1/7] 停止旧进程..."; Stop-WindService $suffix
    Say "[2/7] 反注册旧 TSF COM..."; Unregister-Tsf $targetDir $suffix
    Say "[3/7] 准备目录..."; New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
    Say "[4/7] 复制文件..."
    Copy-Replace $targetDir "wind_input$suffix.exe" "$outdir\wind_input$suffix.exe"
    if (Test-Path "$outdir\wind_cli.bat")      { Copy-Replace $targetDir "wind_cli.bat"      "$outdir\wind_cli.bat" }
    foreach ($dll in (Get-ChildItem "$outdir\wind_tsf*.dll" -ErrorAction SilentlyContinue)) {
        Copy-Replace $targetDir $dll.Name $dll.FullName
    }
    if (Test-Path "$outdir\wind_setting$suffix.exe") { Copy-Replace $targetDir "wind_setting$suffix.exe" "$outdir\wind_setting$suffix.exe" }
    if (Test-Path "$outdir\wind_portable.exe")       { Copy-Replace $targetDir "wind_portable.exe"       "$outdir\wind_portable.exe" }
    if (Test-Path "$outdir\data") {
        $td = Join-Path $targetDir "data"
        if (Test-Path $td) { Remove-Item $td -Recurse -Force -ErrorAction SilentlyContinue }
        Copy-Item "$outdir\data" -Destination $targetDir -Recurse -Force
        Gray "  - data\ (词库、方案、主题)"
    }
    Say "[5/7] 设置权限 + 注册 TSF COM..."
    Grant-TsfAcl $targetDir $suffix
    if (-not (Register-Tsf $targetDir $suffix)) { return $false }
    Install-WubiFont $targetDir
    Say "[6/7] 配置开机自启 + 默认启用输入法..."
    Set-AutoStart $targetDir $suffix
    Enable-TsfForUser $profile
    Get-ChildItem "$targetDir\*.old*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    Say "[7/7] 启动输入法服务..."
    $exe = Join-Path $targetDir "wind_input$suffix.exe"
    Start-Process -FilePath $exe; Gray "  - 已启动 wind_input$suffix.exe"
    Say "`n系统安装完成 ($profile) → $targetDir"
    Say "提示: 按 Win+Space 切换到清风输入法$(if($suffix){' (Dev)'})。"
    return $true
}

# 系统安装单模块 (不重编, 用现有产物): pm1=tsf pm2=core (pd 前缀=dev)。
#   tsf : 停服务 → 反注册旧 COM → 复制 → icacls → 重注册 → 重启服务
#   core: 停服务 → 复制 (含 wind_cli.bat) → 重启服务
function Deploy-Module ([string]$profile, [string]$mod) {
    $outdir = Out-For $profile
    $targetDir = if ($profile -eq "dev") { $DeployDirDev } else { $DeployDirRelease }
    $suffix = if ($profile -eq "dev") { "_dev" } else { "" }
    $files = @()
    switch ($mod) {
        "tsf"  { $files = @("wind_tsf$suffix.dll", "wind_tsf_x86${suffix}.dll") }
        "core" { $files = @("wind_input$suffix.exe") }
        default { ErrMsg "未知模块: $mod (tsf|core)"; return $false }
    }
    if (-not (Require-Admin)) { return $false }
    if (-not (Test-Path $targetDir)) {
        ErrMsg "安装目录不存在: $targetDir; 请先 '$(if($profile -eq 'dev'){'pd1'}else{'p1'})' 完整安装。"; return $false
    }
    foreach ($f in $files) { if (-not (Test-Path "$outdir\$f")) { ErrMsg "本地无 $outdir\$f (先构建对应模块)"; return $false } }
    Say "`n========== 系统安装模块 ($profile/$mod) → $targetDir =========="
    Say "[1/4] 停止旧进程..."; Stop-WindService $suffix
    if ($mod -eq "tsf") { Say "[2/4] 反注册旧 TSF COM..."; Unregister-Tsf $targetDir $suffix }
    else                { Say "[2/4] (core 无需反注册 COM)" }
    Say "[3/4] 复制模块文件..."
    foreach ($f in $files) { Copy-Replace $targetDir $f "$outdir\$f" }
    if ($mod -eq "core" -and (Test-Path "$outdir\wind_cli.bat")) { Copy-Replace $targetDir "wind_cli.bat" "$outdir\wind_cli.bat" }
    if ($mod -eq "tsf") {
        Grant-TsfAcl $targetDir $suffix
        if (-not (Register-Tsf $targetDir $suffix)) { return $false }
        Enable-TsfForUser $profile
    }
    Get-ChildItem "$targetDir\*.old*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    Say "[4/4] 启动输入法服务..."
    $exe = Join-Path $targetDir "wind_input$suffix.exe"
    if (Test-Path $exe) { Start-Process -FilePath $exe; Gray "  - 已启动 wind_input$suffix.exe" }
    Say "`n模块部署完成 ($profile/$mod)"
    return $true
}

# ---------- 卸载 (系统卸载 = 安装的逆操作) ----------
# 从当前用户中文输入法列表移除本变体 TIP (Enable-TsfForUser 的逆操作)。
function Disable-TsfForUser ([string]$profile) {
    if ($profile -eq "dev") {
        $tip = "0804:{99C2DEB0-5C57-45A2-9C63-FB54B34FD90A}{99C2DEB1-5C57-45A2-9C63-FB54B34FD90A}"
    } else {
        $tip = "0804:{99C2EE30-5C57-45A2-9C63-FB54B34FD90A}{99C2EE31-5C57-45A2-9C63-FB54B34FD90A}"
    }
    try {
        $list = Get-WinUserLanguageList
        $changed = $false
        foreach ($l in $list) {
            if ($l.InputMethodTips -contains $tip) { [void]$l.InputMethodTips.Remove($tip); $changed = $true }
        }
        if ($changed) { Set-WinUserLanguageList -LanguageList $list -Force; Gray "  - 已从用户输入法列表移除" }
        else { Gray "  - 用户列表无此输入法, 跳过" }
    } catch { Warn "  - 移除用户输入法失败: $($_.Exception.Message)" }
}

# 移除开机自启 (HKCU Run; Set-AutoStart 的逆操作)。
function Remove-AutoStart ([string]$suffix) {
    $name = if ($suffix) { "WindInputDev" } else { "WindInput" }
    Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name $name -ErrorAction SilentlyContinue
    Gray "  - 已移除开机自启 ($name)"
}

# 删除单个文件; 被占用(已加载的 DLL)时改名让路 — NTFS 允许改名在用文件, 仅不可删。
# 返回 $true=已真正删除; $false=删不掉(已改名让路或失败)。与 Copy-Replace 同一唯一后缀让路策略。
function Remove-OrRename ([string]$path) {
    if (-not (Test-Path $path)) { return $true }
    $leaf = Split-Path $path -Leaf
    try { Remove-Item $path -Force -ErrorAction Stop; Gray "  - 删除 $leaf"; return $true }
    catch {
        # 已带让路标记的文件不再重复改名: 它已经让过路了, 改成 .old_a.old_b 既无意义,
        # 又是垃圾累积的真正来源 (卸载遍历目录下所有文件, 每轮把删不掉的存量整体翻新一遍)。
        # 只标记未让路的原文件, 则每个原文件至多留一个残留, 待宿主释放后被 *.old* 清理带走。
        if ($leaf -match '\.old(_\d+)?$') {
            Warn "  - $leaf 仍被占用 (历史让路文件, 不再改名); 重启后可清除"
            return $false
        }
        $old = "$path.old_$(Get-Random -Maximum 99999999)"
        try {
            Move-Item $path $old -Force -ErrorAction Stop
            Warn "  - $leaf 被占用, 已改名让路 ($(Split-Path $old -Leaf)); 重启后可清除"
        } catch {
            ErrMsg "  - $leaf 删除/改名均失败: $($_.Exception.Message)"
        }
        return $false
    }
}

# 系统卸载: 完整撤销 Deploy-Full 的副作用 (u1 / ud1)。
#   停进程 → 移出用户输入法列表 → 反注册 TSF COM(x64+x86) → 移除开机自启 → 删安装目录。
# 共存安全: 仅动本变体 (CLSID/目录/自启名均带本变体后缀), 不影响另一变体或系统其它输入法。
# 字体(黑体字根)为两变体共享, 故【不】卸载, 以免影响仍在用的另一变体。
# 个人数据(词库/配置/统计)默认保留; 仅打印路径供手动清除。
function Uninstall-Full ([string]$profile = "release") {
    $targetDir = if ($profile -eq "dev") { $DeployDirDev } else { $DeployDirRelease }
    $suffix = if ($profile -eq "dev") { "_dev" } else { "" }
    if (-not (Require-Admin)) { return $false }
    Say "`n========== 系统卸载 ($profile) → $targetDir =========="
    Say "[1/5] 停止进程..."; Stop-WindService $suffix
    Say "[2/5] 移出用户输入法列表..."; Disable-TsfForUser $profile
    Say "[3/5] 反注册 TSF COM..."
    if (Test-Path $targetDir) { Unregister-Tsf $targetDir $suffix; Gray "  - 已反注册 (x64 + x86)" }
    else { Warn "  - 安装目录不存在, 跳过反注册 (可能已卸载)" }
    Say "[4/5] 移除开机自启..."; Remove-AutoStart $suffix
    Say "[5/5] 删除安装文件 (锁定的 DLL 改名让路)..."
    if (Test-Path $targetDir) {
        # 先清掉历史改名残留 (上次卸载留下、此刻或已可删)
        Get-ChildItem "$targetDir\*.old*" -Recurse -File -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
        # 逐文件删除; 占用的(TSF DLL 等)改名让路, 不再因单个锁定文件整体失败
        $allGone = $true
        Get-ChildItem $targetDir -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object {
            if (-not (Remove-OrRename $_.FullName)) { $allGone = $false }
        }
        if ($allGone) {
            try { Remove-Item $targetDir -Recurse -Force -ErrorAction Stop; Gray "  - 已删除安装目录 $targetDir" }
            catch { Warn "  - 文件已清空, 但目录未能删除 (重启后可删): $targetDir" }
        } else {
            Warn "  - 部分文件被占用已改名让路; 重启系统后重跑本命令或手动删除残留目录:"
            Warn "    $targetDir"
        }
    } else { Gray "  - 目录不存在, 跳过" }
    Say "`n系统卸载完成 ($profile)。"
    $appName = if ($suffix) { "WindInputDev" } else { "WindInput" }
    Warn "提示: 个人数据已保留, 如需彻底清除请手动删除:"
    Warn "  漫游配置/词库: $env:APPDATA\$appName"
    Warn "  本机缓存/日志: $env:LOCALAPPDATA\$appName"
    return $true
}

# ---------- 安装包打包 (调用兄弟项目 wind-installer, app.toml 驱动) ----------
# wind-installer 是「通用安装器生成器」: 同一预编译 stub 配不同 app.toml 即生成不同安装包。
# 安装目录由 app.toml 的 [app] id 派生 (ProgramFiles\<id>), 故 dev=WindInputDev、release=WindInput
# 自然落到与 pd1/p1 一致的目录; IME 注册 GUID/文件名/字体亦全部由清单描述, 无需改安装器源码。
#
# 生成变体 app.toml: 全用绝对路径 + 正斜杠 (TOML 与 Windows 均接受正斜杠, 免去反斜杠转义;
# 且 pack.ps1 用 "([^"]+)" 正则解析 source_dir, 双引号字符串才能匹配)。落到 dist\ (在 source 之外,
# 不会被 packer 递归打进包)。GUID 必须与 wind_tsf\src\Globals.cpp 一致 (dev=DEB0/DEB1, release=EE30/EE31)。
function New-InstallerConfig ([string]$profile, [string]$outdir, [string]$cfgPath, [string]$assetsDir) {
    if ($profile -eq "dev") {
        $id = "WindInputDev"; $disp = "清风输入法 (开发版)"; $mainExe = "wind_input_dev.exe"
        $menu = "清风输入法 (开发版)"; $title = "清风输入法 (开发版) 安装向导"; $proto = "windinputdev"
        $settingExe = "wind_setting_dev.exe"
        $procs = '["wind_setting_dev", "wind_portable", "wind_input_dev"]'
        $acl   = '["wind_tsf_dev.dll", "wind_tsf_x86_dev.dll"]'
        $clsid = "{99C2DEB0-5C57-45A2-9C63-FB54B34FD90A}"; $prof = "{99C2DEB1-5C57-45A2-9C63-FB54B34FD90A}"
        $dllX64 = "wind_tsf_dev.dll"; $dllX86 = "wind_tsf_x86_dev.dll"; $outName = "WindInputDev-Setup"
    } else {
        $id = "WindInput"; $disp = "清风输入法"; $mainExe = "wind_input.exe"
        $menu = "清风输入法"; $title = "清风输入法 安装向导"; $proto = "windinput"
        $settingExe = "wind_setting.exe"
        $procs = '["wind_setting", "wind_portable", "wind_input"]'
        $acl   = '["wind_tsf.dll", "wind_tsf_x86.dll"]'
        $clsid = "{99C2EE30-5C57-45A2-9C63-FB54B34FD90A}"; $prof = "{99C2EE31-5C57-45A2-9C63-FB54B34FD90A}"
        $dllX64 = "wind_tsf.dll"; $dllX86 = "wind_tsf_x86.dll"; $outName = "WindInput-Setup"
    }
    # 设置程序为可选模块: ../wind-setting 不存在时 Build-Setting 会跳过, build/ 里就没有产物。
    # 此时必须置空 setting_exe, 否则安装器会为不存在的文件建开始菜单快捷方式。
    if (-not (Test-Path (Join-Path $outdir $settingExe))) {
        Warn "未找到 $outdir\$settingExe, 本次打包不含设置程序 (setting_exe 置空)"
        $settingExe = ""
    }
    $srcFwd  = $outdir.Replace('\', '/')
    $distFwd = $DistDir.Replace('\', '/')
    $logoFwd = (Join-Path $assetsDir "logo.png").Replace('\', '/')
    $iconFwd = (Join-Path $assetsDir "installer.ico").Replace('\', '/')

    # 单一真相: 读 config\app.toml, 仅把 [app]/[ime]/[package] 替换为变体/机器相关值;
    # [[font]]/[autostart]/[[shortcut]]/[startup]/[datadir]/[strings]/[ui] 等能力与文案段原样继承。
    # 快捷方式用 {setting_exe}/{main_exe}/{display_name} 占位符, 安装器运行期按 [app] 字段替换,
    # 故一份 config 对 dev/release 通用; {setting_exe} 为空 (无设置程序) 时安装器自动跳过该快捷方式。
    # 这样 wind-installer 新增能力段时只需改 config\app.toml 一处, 无需同步本脚本 (消除双真相漂移)。
    $baseCfg = Join-Path $ProductRoot "config\app.toml"
    if (-not (Test-Path $baseCfg)) { ErrMsg "未找到清单基底: $baseCfg"; throw "缺少 config\app.toml" }
    $base = Get-Content $baseCfg -Raw

    $appSec = @"
[app]
id                = "$id"
display_name      = "$disp"
version           = "$Version"
publisher         = "清风输入法 项目"
description       = "轻量开源输入法"
main_exe          = "$mainExe"
setting_exe       = "$settingExe"
start_menu_folder = "$menu"
window_title      = "$title"
url_protocol      = "$proto"
portable_marker   = "portable_mode"
process_names     = $procs
acl_dlls          = $acl
"@
    $imeSec = @"
[ime]
clsid        = "$clsid"
profile_guid = "$prof"
lang_id      = "0804"
dll_x64      = "$dllX64"
dll_x86      = "$dllX86"
"@
    $pkgSec = @"
[package]
compression = "zstd"
source_dir  = "$srcFwd"
output_name = "$outName"
output_dir  = "$distFwd"
logo        = "$logoFwd"
icon        = "$iconFwd"
"@
    # 砍掉 config 的 [package] 及之后 (打包参数按机器生成), 再替换 [app]/[ime] 段。
    # 用 MatchEvaluator 回调返回字面串, 避免 -replace 把替换文本里的 $ 当分组引用。
    $head = ($base -split '(?m)^\[package\]', 2)[0]
    $head = [regex]::Replace($head, '(?ms)^\[app\]\r?\n.*?(?=^\[)', { param($x) $appSec + "`r`n`r`n" })
    $head = [regex]::Replace($head, '(?ms)^\[ime\]\r?\n.*?(?=^\[)',  { param($x) $imeSec + "`r`n`r`n" })
    $ai = $head.IndexOf("[app]"); if ($ai -gt 0) { $head = $head.Substring($ai) }
    $gen = "# 本文件由 dev.ps1 自动生成 —— $profile 变体; [app]/[ime]/[package] 为变体/机器值, 其余段继承 config\app.toml。请勿手工编辑。`r`n"
    $toml = $gen + $head.TrimEnd() + "`r`n`r`n" + $pkgSec + "`r`n"

    # 无 BOM UTF-8 写出 (Rust toml 解析器对前置 BOM 会报错; PS5.1 的 Set-Content -Encoding UTF8 带 BOM)。
    [System.IO.File]::WriteAllText($cfgPath, $toml, (New-Object System.Text.UTF8Encoding($false)))
}

# ---------- 在线升级元数据 (latest.json / latest-dev.json) ----------
# 供 wind-setting 的在线升级检查读取, 与安装包一并上传 CDN。
# 字段契约见 wind-setting\docs\online-update-plan.md §3.2。要点:
#   · sha256/size 为必填 —— 客户端在缺失或不匹配时拒绝升级, 不退化为"不校验就装"
#     (旧 Go 版官网渠道 size 恒为 0, 导致 %TEMP% 里一个被截断的同名文件会被当成完整包安装)。
#   · channel 与客户端自身变体交叉校验, 防 CDN 缓存串档把 dev 包发给正式版用户。
# 两个变体各写各的文件, 互不干扰; 上传时务必**先传 exe 再传 json** —— json 是开关,
# 反过来会让客户端看到新版本却下载到 404。
function New-UpdateManifest ([string]$profile, [string]$setupPath) {
    $isDev    = ($profile -eq "dev")
    $channel  = if ($isDev) { "dev" } else { "stable" }
    $base     = if ($isDev) { "WindInputDev" } else { "WindInput" }
    $jsonName = if ($isDev) { "latest-dev.json" } else { "latest.json" }

    $item = Get-Item $setupPath
    $sha  = (Get-FileHash -Path $setupPath -Algorithm SHA256).Hash.ToLower()

    # sha256 sidecar (标准 sha256sum 格式), 便于手工核对与 CDN 侧校验
    $shaFile = "$setupPath.sha256"
    [System.IO.File]::WriteAllText($shaFile, "$sha  $($item.Name)`n",
        (New-Object System.Text.UTF8Encoding($false)))

    $manifest = [ordered]@{
        version         = $Version
        tag             = "v$Version"
        channel         = $channel
        exeUrl          = "$CdnBase/$($item.Name)"
        sha256          = $sha
        size            = $item.Length
        releaseNotesUrl = "$CdnBase/$base-$Version-Release.md"
        publishedAt     = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    }

    $out = Join-Path $DistDir $jsonName
    # 无 BOM UTF-8: 客户端按 UTF-8 文本解析, 前置 BOM 会让 serde_json 报错。
    [System.IO.File]::WriteAllText($out, ($manifest | ConvertTo-Json -Depth 3),
        (New-Object System.Text.UTF8Encoding($false)))

    Say "升级元数据: $out"
    Gray "  channel=$channel  version=$Version  size=$($item.Length)"
    Gray "  sha256=$sha"
    Gray "  上传顺序: 先 $($item.Name), 确认可访问后再 $jsonName"
}

# 生成安装包: (除非 skip) 全构建当前变体 → 生成 app.toml → 调 wind-installer\scripts\pack.ps1。
#   pack.ps1 负责: 原生编译 stub/uninstaller/packer → 注入 uninstall.exe 到 source → wind-packer build。
# 打包是纯文件 IO + cargo 构建, 不需管理员 (故未纳入 UAC 提权命令)。
function Do-Installer ([string]$profile = "release", [bool]$skipBuild = $false) {
    # 1. 定位 wind-installer 兄弟项目
    $instDir = $InstallerDir
    if (Test-Path $InstallerDir) { $instDir = (Resolve-Path $InstallerDir).Path }
    if (-not (Test-Path $instDir)) {
        ErrMsg "未找到 wind-installer 项目: $instDir"
        ErrMsg "请将 wind-installer 与 WindInput 放在同级目录, 或在 scripts\deploy.local.ps1 设置 `$InstallerDir。"
        return $false
    }
    $packPs1 = Join-Path $instDir "scripts\pack.ps1"
    if (-not (Test-Path $packPs1)) { ErrMsg "缺少打包脚本: $packPs1"; return $false }
    $assetsDir = Join-Path $instDir "assets"

    # 2. 构建产物 (除非 skip)
    $outdir = Out-For $profile
    $suffix = if ($profile -eq "dev") { "_dev" } else { "" }
    if (-not $skipBuild) {
        if (-not (Do-Full $profile)) { return $false }
    } elseif (-not (Test-Path "$outdir\wind_input$suffix.exe")) {
        ErrMsg "无 $outdir 产物; 去掉 skip 先全构建, 或运行 '$(if($profile -eq 'dev'){'d1'}else{'1'})'。"; return $false
    }

    # 3. 生成变体 app.toml → dist\ (在 source 之外)
    New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
    $cfgName = if ($profile -eq "dev") { "WindInputDev.app.toml" } else { "WindInput.app.toml" }
    $cfg = Join-Path $DistDir $cfgName
    New-InstallerConfig $profile $outdir $cfg $assetsDir

    Say "`n========== 生成安装包 ($profile) =========="
    Gray "  安装器: $instDir"
    Gray "  产物:   $outdir"
    Gray "  配置:   $cfg"
    Gray "  输出:   $DistDir"

    # 4. 调 pack.ps1 (编译 stub + 注入卸载器 + packer build)。
    #    skip 模式且 installer 二进制已在 → 透传 -SkipBuild 跳过 stub 重编 (加速反复打包)。
    $stub   = Join-Path $instDir "target\release\wind-installer.exe"
    $packer = Join-Path $instDir "target\release\wind-packer.exe"
    $unins  = Join-Path $instDir "target\release\wind-uninstaller.exe"
    $instBuilt = (Test-Path $stub) -and (Test-Path $packer) -and (Test-Path $unins)
    # 哈希表 splat 才能按名绑定 (数组 splat 会把 -Config 当成位置参数的值)。
    $packArgs = @{ Config = $cfg }
    if ($skipBuild -and $instBuilt) { $packArgs['SkipBuild'] = $true }
    & $packPs1 @packArgs
    if ($LASTEXITCODE -ne 0) { ErrMsg "打包失败 (见上方 wind-packer 输出)"; return $false }

    $setup = Join-Path $DistDir "$(if($profile -eq 'dev'){'WindInputDev-Setup'}else{'WindInput-Setup'})-$Version.exe"
    if (Test-Path $setup) {
        $sz = [math]::Round((Get-Item $setup).Length / 1MB, 1)
        Say "`n安装包已生成: $setup (${sz}MB)"
        # 5. 生成在线升级元数据 + sha256 sidecar (供 wind-setting 检查更新)
        New-UpdateManifest $profile $setup
    } else {
        Warn "打包脚本已结束, 但未找到预期输出: $setup"
        Warn "请检查上方 wind-packer 实际输出名 (dist\ 下)。"
    }
    return $true
}

# ---------- 候选 REPL (本机) ----------
function Do-Repl ([string]$data = "") {
    if (-not $data) {
        if (Test-Path "$BuildDevDir\data\schemas\pinyin\unigram.txt") { $data = "$BuildDevDir\data" }
        else { Warn "未找到词库数据; 请先运行 gen-data"; $data = "$BuildDevDir\data" }
    }
    Say "`n启动候选 REPL (data=$data)..."
    Push-Location $ProjectRoot
    try { $env:WIND_DATA = $data; cargo run --release -p wind-repl -- $data } finally { Pop-Location }
}

# ---------- 菜单 ----------
function Show-Menu {
    Clear-Host
    Write-Host "============================================" -ForegroundColor Cyan
    Write-Host "  WindInput 开发菜单  v$Version  (Windows/MSVC)" -ForegroundColor Cyan
    Write-Host "============================================`n" -ForegroundColor Cyan
    Write-Host "  全构建 (→ build/, 内容 == 部署内容):" -ForegroundColor Yellow
    Write-Host "    1    Release 全构建: wind_input + tsf(x64/x86) + setting + portable + 词库"
    Write-Host "    d1   Dev 全构建 (→ build_dev/)"
    Write-Host "`n  单模块构建 (前缀 d = dev):" -ForegroundColor Yellow
    Write-Host "    m1   仅 tsf (x64+x86)                dm1"
    Write-Host "    m2   仅 wind_input (核心)             dm2"
    Write-Host "    m3   仅 wind_setting (../wind-setting)  dm3"
    Write-Host "    m4   仅 wind_portable (../wind-portable) dm4"
    Write-Host "`n  系统安装 / 卸载 (注册 TSF + 开机自启 + 默认启用, 自动提权):" -ForegroundColor Yellow
    Write-Host "    p1   安装全部 (release)        pd1   安装全部 (dev)"
    Write-Host "    pm1/pm2  安装模块(tsf/核心)    pdm1/pdm2 (dev)"
    Write-Host "    u1/u  卸载全部 (release)        ud1/ud  卸载全部 (dev)"
    Write-Host "      release → $DeployDirRelease" -ForegroundColor DarkGray
    Write-Host "      dev     → $DeployDirDev" -ForegroundColor DarkGray
    Write-Host "`n  安装包 (调用兄弟项目 wind-installer 打包):" -ForegroundColor Yellow
    Write-Host "    8    生成安装包 (release)       d8    生成安装包 (dev)"
    Write-Host "    8s   跳过重建直接打包 (release)  d8s   跳过重建直接打包 (dev)"
    Write-Host "      输出 → $DistDir" -ForegroundColor DarkGray
    Write-Host "`n  代码质量:" -ForegroundColor Yellow
    Write-Host "    k=check  l=clippy  t=test  f=fmt  ci=fmt+clippy+test"
    Write-Host "`n  数据 / 实测:" -ForegroundColor Yellow
    Write-Host "    gd=gen-data  r=repl(本机)"
    Write-Host "`n  杂项:" -ForegroundColor Yellow
    Write-Host "    clean  q=退出"
    Write-Host "============================================" -ForegroundColor Cyan
}

# ---------- 统一分发 (菜单与命令行直调共用; 命令已转小写) ----------
# 返回 127 = 未知命令 (区别于命令执行失败)。
function Dispatch ([string]$cmd, [string]$arg) {
    switch ($cmd) {
        { $_ -in @("1", "release") }        { if (Do-Full release) { 0 } else { 1 }; break }
        { $_ -in @("d1", "dev") }           { if (Do-Full dev)   { 0 } else { 1 }; break }
        "m1"   { if (Build-TsfAll   release) { 0 } else { 1 }; break }
        "dm1"  { if (Build-TsfAll   dev)   { 0 } else { 1 }; break }
        "m2"   { if (Build-Core     release) { 0 } else { 1 }; break }
        "dm2"  { if (Build-Core     dev)   { 0 } else { 1 }; break }
        "m3"   { if (Build-Setting  release) { 0 } else { 1 }; break }
        "dm3"  { if (Build-Setting  dev)   { 0 } else { 1 }; break }
        "m4"   { if (Build-Portable release) { 0 } else { 1 }; break }
        "dm4"  { if (Build-Portable dev)   { 0 } else { 1 }; break }
        "p1"   { if (Deploy-Full release) { 0 } else { 1 }; break }
        "pd1"  { if (Deploy-Full dev)   { 0 } else { 1 }; break }
        "pm1"  { if (Deploy-Module release tsf)  { 0 } else { 1 }; break }
        "pm2"  { if (Deploy-Module release core) { 0 } else { 1 }; break }
        "pdm1" { if (Deploy-Module dev tsf)    { 0 } else { 1 }; break }
        "pdm2" { if (Deploy-Module dev core)   { 0 } else { 1 }; break }
        "u"    { if (Uninstall-Full release) { 0 } else { 1 }; break }
        "u1"   { if (Uninstall-Full release) { 0 } else { 1 }; break }
        "ud"   { if (Uninstall-Full dev)   { 0 } else { 1 }; break }
        "ud1"  { if (Uninstall-Full dev)   { 0 } else { 1 }; break }
        { $_ -in @("8", "installer") }       { if (Do-Installer release $false) { 0 } else { 1 }; break }
        "8s"                                 { if (Do-Installer release $true)  { 0 } else { 1 }; break }
        { $_ -in @("d8", "installer-dev") }  { if (Do-Installer dev $false)   { 0 } else { 1 }; break }
        "d8s"                                { if (Do-Installer dev $true)     { 0 } else { 1 }; break }
        { $_ -in @("k", "check") }   { Do-Check;  $LASTEXITCODE; break }
        { $_ -in @("l", "clippy") }  { Do-Clippy; $LASTEXITCODE; break }
        { $_ -in @("t", "test") }    { Do-Test;   $LASTEXITCODE; break }
        { $_ -in @("f", "fmt") }     { Do-Fmt;    $LASTEXITCODE; break }
        "fmt-check"                  { Do-FmtCheck; $LASTEXITCODE; break }
        "ci"                         { if (Do-Ci) { 0 } else { 1 }; break }
        "clean"                      { Do-Clean;  $LASTEXITCODE; break }
        { $_ -in @("gd", "gen-data") }  { if (Do-GenData) { 0 } else { 1 }; break }
        { $_ -in @("r", "repl") }       { Do-Repl $arg; 0; break }
        default { 127 }
    }
}

function Menu-Loop {
    while ($true) {
        Show-Menu
        $raw = (Read-Host "`n请输入选项").Trim()
        if (-not $raw) { continue }
        if ($raw.ToLower() -eq "q") { return }

        # 支持空格分隔的连续命令: "d1 pd1" → 依次执行, 前者失败则停止
        # @() 强制包装: 单 token 时 Where-Object 返回标量字符串, 索引会取字符而非词
        $tokens = @($raw.ToLower() -split '\s+' | Where-Object { $_ -ne "" })
        $i = 0
        $anyFailed = $false
        $needPause = $false   # UAC 成功时输出已内联显示, 无需额外暂停
        while ($i -lt $tokens.Count -and -not $anyFailed) {
            $choice = $tokens[$i]
            $choiceArg = ""
            # repl 命令后一个 token 为数据路径 (非命令)
            if ($choice -eq "r" -or $choice -eq "repl") {
                $i++
                if ($i -lt $tokens.Count) { $choiceArg = $tokens[$i] }
            }
            $el = Invoke-Elevated $choice $choiceArg
            if ($el -eq "skip") {
                $needPause = $true   # 普通命令在当前窗口产生输出, 需暂停让用户阅读
                $rc = Dispatch $choice $choiceArg
                if ($rc -eq 127) { ErrMsg "无效选项: $choice"; $anyFailed = $true }
                elseif ($rc -ne 0) { ErrMsg "`n命令 '$choice' 失败 (退出码 $rc)"; $anyFailed = $true }
            } elseif ($el -eq "done") {
                $needPause = $true   # UAC 子进程输出已内联显示, 暂停让用户阅读
            } elseif ($el -eq "fail") {
                $needPause = $true   # 提权失败/被取消, 需暂停让用户看到错误
                $anyFailed = $true
            }
            $i++
        }
        if ($needPause) { Write-Host ""; Write-Host "按回车继续..." -NoNewline; Read-Host | Out-Null }
    }
}

# ---------- 入口 ----------
$allCmds = @($Commands | Where-Object { $_ -ne "" })

# 无参数 → 交互菜单
if ($allCmds.Count -eq 0) { Menu-Loop; return }

$firstCmd = $allCmds[0].Trim().ToLower()

# help
if ($firstCmd -eq "-h" -or $firstCmd -eq "--help" -or $firstCmd -eq "help") {
    Get-Content $PSCommandPath | Where-Object { $_ -match '^#' } | ForEach-Object { $_ -replace '^# ?', '' }
    return
}

# menu (显式)
if ($firstCmd -eq "menu") { Menu-Loop; return }

# 按序执行所有命令; repl 后一个参数为数据路径
$i = 0
while ($i -lt $allCmds.Count) {
    $cmd = $allCmds[$i].Trim().ToLower()
    $arg = ""
    if ($cmd -eq "r" -or $cmd -eq "repl") {
        $i++
        if ($i -lt $allCmds.Count) { $arg = $allCmds[$i] }
    }

    $el = Invoke-Elevated $cmd $arg
    if ($el -eq "done") { $i++; continue }
    if ($el -eq "fail") { exit 1 }

    $rc = Dispatch $cmd $arg
    if ($rc -eq 127) {
        ErrMsg "未知命令: $cmd"
        Write-Host "运行 '.\scripts\dev.ps1 --help' 查看可用命令"
        exit 1
    }
    if ($rc -ne 0) { exit $rc }
    $i++
}
exit 0
