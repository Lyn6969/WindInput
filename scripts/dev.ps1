# WindInput 开发菜单 (Windows 原生构建 / MSVC)
#
# 用法:
#   .\scripts\dev.ps1            # 交互式菜单 (对齐 dev.sh)
#   .\scripts\dev.ps1 <命令>     # 非交互直调, 如 .\scripts\dev.ps1 release
#   (dev.bat 已转发 %*, 故 dev.bat release / dev.bat m2 等价)
#
# 本机 (Windows) 原生构建:
#   - Rust(wind_input/portable): cargo build --release (host = x86_64-pc-windows-msvc)
#   - C++ TSF: CMake + "Visual Studio 17 2022" 生成器 (x64 + Win32, 自动定位 MSVC)
#   - 词库数据: 下载 rime-frost/pinyin-data/OpenCC + 生成 unigram/pinyin_map + 编 octrie
#   - 全构建产物落【产品根】build/(release) 或 build_debug/(debug); 内容 == 安装内容
#
# 命令 (菜单与命令行直调同一套; 前缀 d=debug, p=push/部署, m=单模块):
#   1            Release 全构建: wind_input + tsf(x64/x86) + portable + 词库数据 → build/
#   d1           Debug 全构建 → build_debug/
#   m1 / dm1     仅 tsf (x64+x86)            release / debug
#   m2 / dm2     仅 wind_input (核心 exe)     release / debug
#   p1 / pd1     系统安装全部 (release / debug): 复制 + 注册 TSF + 开机自启 + 启动服务
#   pm1 / pm2    系统安装单模块 (tsf / 核心, release)
#   pdm1 / pdm2  系统安装单模块 (debug)
#   k=check  l=clippy  t=test  f=fmt  fmt-check  ci(=fmt+clippy+test)  clean
#   gd=gen-data  r=repl
#
# 部署目标 (Go 非便携式系统安装; 默认在 Program Files 下, 部署自动 UAC 提权;
# 在 scripts\deploy.local.ps1 覆盖, PowerShell 赋值格式):
#   DeployDirRelease = C:\Program Files\WindInput      # p1 / pm* 目标
#   DeployDirDebug   = C:\Program Files\WindInputDev   # pd1 / pdm* 目标
#
# 数据目录说明:
#   data/                源文件(入库): 配置、五笔词库、主题等手工维护文件
#   .cache/              外部下载/生成(gitignore): rime-frost、opencc、unigram 等
#   build/ build_debug/  全构建产物(gitignore); 内容即部署到目标目录的内容

param(
    [Parameter(Position = 0)] [string]$Command = "",
    [Parameter(Position = 1)] [string]$Arg = ""
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
$Version       = (Get-Content "$ProductRoot\docs\VERSION" -Raw).Trim()
$BuildDir      = "$ProductRoot\build"
$BuildDebugDir = "$ProductRoot\build_debug"
$CacheDir      = "$ProductRoot\.cache"        # 外部下载/生成 (不入库)

# ---------- 部署目标 (Go 便携式: 复制到指定本地目录) ----------
$DeployDirRelease = "C:\Program Files\WindInput"
$DeployDirDebug   = "C:\Program Files\WindInputDev"
# 可在 scripts\deploy.local.ps1 覆盖上述变量 (PowerShell 赋值语法; 该文件 gitignore)。
$deployCfg = "$ScriptDir\deploy.local.ps1"
if (Test-Path $deployCfg) { . $deployCfg }

# ---------- 输出辅助 ----------
function Say  ([string]$m) { Write-Host $m -ForegroundColor Green }
function Warn ([string]$m) { Write-Host $m -ForegroundColor Yellow }
function ErrMsg ([string]$m) { Write-Host $m -ForegroundColor Red }
function Gray ([string]$m) { Write-Host $m -ForegroundColor DarkGray }

# release → BUILD_DIR; debug → BUILD_DEBUG_DIR
function Out-For ([string]$profile) { if ($profile -eq "debug") { $BuildDebugDir } else { $BuildDir } }

# ---------- 构建: 核心 exe ----------
# debug 变体 = release profile + debug_variant 特性 (非 dev profile):
#   ① debug_assertions 关闭 → windows_subsystem="windows" 生效, 无控制台窗口;
#   ② 优化构建, 输入法手感正常; ③ 仍是独立 _debug 身份 (管道/目录隔离)。
function Build-Core ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    $suffix = ""; $feats = @()
    if ($profile -eq "debug") { $suffix = "_debug"; $feats = @("--features", "debug_variant") }
    Say "`n[core] 构建 wind_input ($profile, release profile$(if($feats){' +debug_variant'}))..."
    Push-Location $ProjectRoot
    try {
        cargo build --release -p wind_service @feats
        if ($LASTEXITCODE -ne 0) { ErrMsg "wind_input 构建失败!"; return $false }
    } finally { Pop-Location }
    $src = "$ProjectRoot\target\release\wind_input.exe"
    if (-not (Test-Path $src)) { ErrMsg "未找到产物: $src"; return $false }
    Copy-Item $src "$outdir\wind_input$suffix.exe" -Force
    $sz = [math]::Round((Get-Item "$outdir\wind_input$suffix.exe").Length / 1MB, 1)
    Gray "已构建: wind_input$suffix.exe (${sz}MB)"
    # CLI 包装器 (wind_input config ...; 运行时自辨 debug/release exe, 两变体共用一份)
    $cli = "$ProjectRoot\scripts\wind_cli.bat"
    if (Test-Path $cli) { Copy-Item $cli "$outdir\wind_cli.bat" -Force; Gray "已复制: wind_cli.bat" }
    return $true
}

# ---------- 构建: C++ TSF DLL (x64 + x86; CMake/MSVC) ----------
# CMakeLists 把 DLL 写死输出到 ..\build[_debug], x86/x64 同名 wind_tsf.dll。
# 故先编 x86 → 改名 _x86, 再编 x64 (保留无后缀名), 避免互相覆盖。
function Build-TsfAll ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
        Warn "未找到 cmake; 跳过 TSF (安装 CMake + VS2022 C++ 工具后可构建)。"; return $true
    }
    $suffix = ""; $dvFlag = "OFF"
    if ($profile -eq "debug") { $suffix = "_debug"; $dvFlag = "ON" }
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
            "-DWIND_DEBUG_VARIANT=$dvFlag" `
            "-DAPP_VERSION_STR=$Version" `
            "-DAPP_VERSION_MAJOR=$vMaj" "-DAPP_VERSION_MINOR=$vMin" "-DAPP_VERSION_PATCH=$vPat" `
            | Out-Null
        if ($LASTEXITCODE -ne 0) { ErrMsg "TSF $($a.A) CMake 配置失败!"; return $false }
        cmake --build $bin --config Release | Out-Null
        if ($LASTEXITCODE -ne 0) { ErrMsg "TSF $($a.A) 构建失败!"; return $false }
        # CMakeLists 输出到 $outdir\wind_tsf$suffix.dll; x86 需改名加 _x86
        $produced = "$outdir\wind_tsf$suffix.dll"
        $final    = "$outdir\wind_tsf$suffix$($a.Sfx).dll"
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

# 纯 Rust 单一二进制, 运行时自辨 debug/release 变体; release/debug 产出同一份 exe。
function Build-Portable ([string]$profile = "release", [string]$outdir = $null) {
    if (-not $outdir) { $outdir = Out-For $profile }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    try {
        cargo build --release
    } finally { Pop-Location }
    if (-not (Test-Path $exe)) { ErrMsg "未找到产物: $exe"; return $false }
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
    foreach ($d in @($rimeFrostCn, $rimeFrostEn, $opencc, $pinyinData)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }

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
    Get-Dict "$pinyinBase/kXHC1983.txt"       "$pinyinData\kXHC1983.txt"       "新华字典多音字" | Out-Null
    Get-Dict "$pinyinBase/kTGHZ2013.txt"      "$pinyinData\kTGHZ2013.txt"      "通用规范汉字"   | Out-Null
    Get-Dict "$pinyinBase/kMandarin_8105.txt" "$pinyinData\kMandarin_8105.txt" "8105 标准首音"  | Out-Null
    Get-Dict "$pinyinBase/overwrite.txt"      "$pinyinData\overwrite.txt"      "手工纠正"       | Out-Null

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
function Assemble-Data ([string]$outdir = $BuildDebugDir) {
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

    $cnt = (Get-ChildItem $data -Recurse -File).Count
    Gray "data/ 组装完成 ($cnt 文件)"
    return $true
}

# 下载外部词库 + 生成 unigram/pinyin + 组装 data/
function Do-GenData ([string]$outdir = $BuildDebugDir) {
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
    if (Test-Path "$CacheDir\pinyin-data\kMandarin_8105.txt") {
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
        @{ Path = "pinyin_map.txt";                         Min = 10000 }
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
        ErrMsg "请排查 gen-data 的下载/生成 (词库源、网络、gen_unigram/gen_opencc)。"
        return $false
    }
    Say "发布数据校验通过 ✓"; return $true
}

# ---------- 全构建 (1 / d1) ----------
# 全部模块 + 数据落到【产品根】build/(release) 或 build_debug/(debug)。
# 先清空输出目录, 确保内容 == 部署到目标目录的内容, 无任何中间产物。
function Do-Full ([string]$profile = "release") {
    $outdir = Out-For $profile
    Say "`n========== 全构建 ($profile) → $outdir =========="
    if (Test-Path $outdir) { Remove-Item -Recurse -Force $outdir }
    New-Item -ItemType Directory -Path $outdir -Force | Out-Null
    if (-not (Build-Core     $profile $outdir)) { return $false }   # wind_input[_debug].exe
    if (-not (Build-TsfAll   $profile $outdir)) { return $false }   # wind_tsf[_x86][_debug].dll
    if (-not (Do-GenData     $outdir))          { return $false }   # data/
    if (-not (Verify-DistData $outdir))         { return $false }   # 硬门禁
    Say "`n========== 全构建完成 ($profile) → $outdir =========="
    Gray "内容即部署到目标目录的内容 (无中间产物)"
    return $true
}

# ---------- 部署 (Go 非便携式 / 系统安装) ----------
# 与便携式不同: 复制到安装目录后, regsvr32 注册 TSF COM (DllRegisterServer 自带
# AddLanguageProfile + RegisterCategories, 输入法直接进系统列表), 授权 AppContainer
# 宿主读取 DLL, 安装字根字体, 写开机自启, 直接启动 wind_input[_debug].exe (不靠
function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# 部署命令 → 目标安装目录; 非部署命令返回 $null (兼作"是否部署命令"判断)。
function Deploy-TargetForCmd ([string]$cmd) {
    switch ($cmd) {
        { $_ -in "p1", "pm1", "pm2" }    { $DeployDirRelease; break }
        { $_ -in "pd1", "pdm1", "pdm2" } { $DeployDirDebug;   break }
        default { $null }
    }
}

# 系统安装(注册 COM/icacls/字体)始终需管理员。非管理员执行部署命令时自动 UAC 提权。
# 返回三态: "skip" = 非部署命令/已是管理员 (调用方本地执行);
#           "done" = 已成功拉起管理员窗口 (调用方退出, 不再本地执行);
#           "fail" = 提权被取消/失败 (调用方报错并以非零码退出)。
function Invoke-Elevated ([string]$cmd, [string]$arg) {
    if (-not (Deploy-TargetForCmd $cmd)) { return "skip" }   # 非部署命令
    if (Test-Admin) { return "skip" }
    Warn "系统安装需要管理员权限, 正在请求 UAC 提升..."
    $host_exe = (Get-Process -Id $PID).Path   # pwsh.exe 或 powershell.exe
    if (-not $host_exe) { $host_exe = "powershell.exe" }
    # 提权窗口内重新执行同一命令, 完成后停留以便查看输出/错误。
    $inner = "& '$PSCommandPath' $cmd $arg; Write-Host ''; Read-Host '操作结束, 按回车关闭'"
    try {
        # -ErrorAction Stop 确保 UAC 被取消 (用户点否) 时抛出可捕获的终止错误。
        Start-Process -FilePath $host_exe -Verb RunAs -ErrorAction Stop `
            -ArgumentList "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", $inner | Out-Null
        Say "已在管理员窗口执行 '$cmd'; 请在新窗口查看结果。"
        return "done"
    } catch {
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
    $x86 = Join-Path $dir "wind_tsf${suffix}_x86.dll"
    if (Test-Path $x64) { & regsvr32 /u /s $x64 2>$null }
    if (Test-Path $x86) { & (Get-Regsvr32X86) /u /s $x86 2>$null }
}

# 注册 TSF COM (x64 必须成功; x86 失败仅告警, 不阻断 64 位使用)。
function Register-Tsf ([string]$dir, [string]$suffix) {
    $x64 = Join-Path $dir "wind_tsf$suffix.dll"
    $x86 = Join-Path $dir "wind_tsf${suffix}_x86.dll"
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

# 授权 ALL APPLICATION PACKAGES 读取执行 TSF DLL (开始菜单/搜索等 AppContainer 宿主需要)。
function Grant-TsfAcl ([string]$dir, [string]$suffix) {
    $sid = "*S-1-15-2-1"
    foreach ($n in @("wind_tsf$suffix.dll", "wind_tsf${suffix}_x86.dll")) {
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

# 复制单个文件, 处理被占用的 DLL/EXE (改名 .old_ 让路再覆盖)。
function Copy-Replace ([string]$targetDir, [string]$fileName, [string]$srcPath) {
    $dst = Join-Path $targetDir $fileName
    if (-not (Test-Path $dst)) { Copy-Item $srcPath $dst -Force; Gray "  - $fileName"; return }
    try { Copy-Item $srcPath $dst -Force -ErrorAction Stop; Gray "  - $fileName"; return } catch { }
    $old = "$fileName.old_$(Get-Random -Maximum 99999999)"
    try {
        Rename-Item $dst $old -Force -ErrorAction Stop
        Copy-Item $srcPath $dst -Force
        Gray "  - $fileName (旧文件已改名 $old)"
    } catch { ErrMsg "  [错误] 无法替换 ${fileName}: $_" }
}

function Stop-WindService ([string]$suffix) {
        Get-Process -Name $p -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Milliseconds 600
}

# 系统安装: 全部 build[_debug]/ → 安装目录, 注册 TSF + 开机自启 + 启动服务 (p1 / pd1)。
function Deploy-Full ([string]$profile = "release") {
    $outdir = Out-For $profile
    $targetDir = if ($profile -eq "debug") { $DeployDirDebug } else { $DeployDirRelease }
    $suffix = if ($profile -eq "debug") { "_debug" } else { "" }
    if (-not (Require-Admin)) { return $false }
    if (-not (Test-Path "$outdir\wind_input$suffix.exe")) {
        ErrMsg "无 $outdir 产物; 请先 '$(if($profile -eq 'debug'){'d1'}else{'1'})' 全构建。"; return $false
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
    Say "[6/7] 配置开机自启..."; Set-AutoStart $targetDir $suffix
    Get-ChildItem "$targetDir\*.old_*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    Say "[7/7] 启动输入法服务..."
    $exe = Join-Path $targetDir "wind_input$suffix.exe"
    Start-Process -FilePath $exe; Gray "  - 已启动 wind_input$suffix.exe"
    Say "`n系统安装完成 ($profile) → $targetDir"
    Say "提示: 按 Win+Space 切换到清风输入法$(if($suffix){' (Debug)'})。"
    return $true
}

# 系统安装单模块 (不重编, 用现有产物): pm1=tsf pm2=core (pd 前缀=debug)。
#   tsf : 停服务 → 反注册旧 COM → 复制 → icacls → 重注册 → 重启服务
#   core: 停服务 → 复制 (含 wind_cli.bat) → 重启服务
function Deploy-Module ([string]$profile, [string]$mod) {
    $outdir = Out-For $profile
    $targetDir = if ($profile -eq "debug") { $DeployDirDebug } else { $DeployDirRelease }
    $suffix = if ($profile -eq "debug") { "_debug" } else { "" }
    $files = @()
    switch ($mod) {
        "tsf"  { $files = @("wind_tsf$suffix.dll", "wind_tsf${suffix}_x86.dll") }
        "core" { $files = @("wind_input$suffix.exe") }
        default { ErrMsg "未知模块: $mod (tsf|core)"; return $false }
    }
    if (-not (Require-Admin)) { return $false }
    if (-not (Test-Path $targetDir)) {
        ErrMsg "安装目录不存在: $targetDir; 请先 '$(if($profile -eq 'debug'){'pd1'}else{'p1'})' 完整安装。"; return $false
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
    }
    Get-ChildItem "$targetDir\*.old_*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    Say "[4/4] 启动输入法服务..."
    $exe = Join-Path $targetDir "wind_input$suffix.exe"
    if (Test-Path $exe) { Start-Process -FilePath $exe; Gray "  - 已启动 wind_input$suffix.exe" }
    Say "`n模块部署完成 ($profile/$mod)"
    return $true
}

# ---------- 候选 REPL (本机) ----------
function Do-Repl ([string]$data = "") {
    if (-not $data) {
        if (Test-Path "$BuildDebugDir\data\schemas\pinyin\unigram.txt") { $data = "$BuildDebugDir\data" }
        else { Warn "未找到词库数据; 请先运行 gen-data"; $data = "$BuildDebugDir\data" }
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
    Write-Host "    1    Release 全构建: wind_input + tsf(x64/x86) + portable + 词库"
    Write-Host "    d1   Debug 全构建 (→ build_debug/)"
    Write-Host "`n  单模块构建 (前缀 d = debug):" -ForegroundColor Yellow
    Write-Host "    m1   仅 tsf (x64+x86)        dm1"
    Write-Host "    m2   仅 wind_input (核心)     dm2"
    Write-Host "`n  系统安装 (复制 + 注册 TSF + 开机自启, 自动提权):" -ForegroundColor Yellow
    Write-Host "    p1   安装全部 (release)        pd1   安装全部 (debug)"
    Write-Host "    pm1/pm2  安装模块(tsf/核心)    pdm1/pdm2 (debug)"
    Write-Host "      release → $DeployDirRelease" -ForegroundColor DarkGray
    Write-Host "      debug   → $DeployDirDebug" -ForegroundColor DarkGray
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
        { $_ -in "1", "release" }      { if (Do-Full release) { 0 } else { 1 }; break }
        { $_ -in "d1", "debug" }       { if (Do-Full debug)   { 0 } else { 1 }; break }
        "m1"   { if (Build-TsfAll   release) { 0 } else { 1 }; break }
        "dm1"  { if (Build-TsfAll   debug)   { 0 } else { 1 }; break }
        "m2"   { if (Build-Core     release) { 0 } else { 1 }; break }
        "dm2"  { if (Build-Core     debug)   { 0 } else { 1 }; break }
        "m4"   { if (Build-Portable release) { 0 } else { 1 }; break }
        "dm4"  { if (Build-Portable debug)   { 0 } else { 1 }; break }
        "p1"   { if (Deploy-Full release) { 0 } else { 1 }; break }
        "pd1"  { if (Deploy-Full debug)   { 0 } else { 1 }; break }
        "pm1"  { if (Deploy-Module release tsf)  { 0 } else { 1 }; break }
        "pm2"  { if (Deploy-Module release core) { 0 } else { 1 }; break }
        "pdm1" { if (Deploy-Module debug tsf)    { 0 } else { 1 }; break }
        "pdm2" { if (Deploy-Module debug core)   { 0 } else { 1 }; break }
        { $_ -in "k", "check" }   { Do-Check;  $LASTEXITCODE; break }
        { $_ -in "l", "clippy" }  { Do-Clippy; $LASTEXITCODE; break }
        { $_ -in "t", "test" }    { Do-Test;   $LASTEXITCODE; break }
        { $_ -in "f", "fmt" }     { Do-Fmt;    $LASTEXITCODE; break }
        "fmt-check"               { Do-FmtCheck; $LASTEXITCODE; break }
        "ci"                      { if (Do-Ci) { 0 } else { 1 }; break }
        "clean"                   { Do-Clean;  $LASTEXITCODE; break }
        { $_ -in "gd", "gen-data" } { if (Do-GenData) { 0 } else { 1 }; break }
        { $_ -in "r", "repl" }    { Do-Repl $arg; 0; break }
        default { 127 }
    }
}

function Menu-Loop {
    while ($true) {
        Show-Menu
        $choice = (Read-Host "`n请输入选项").Trim().ToLower()
        if ($choice -eq "q") { return }
        if (-not $choice) { continue }
        $el = Invoke-Elevated $choice ""               # 受保护目标 → UAC 提权
        if ($el -ne "skip") {                          # done/fail 都不本地执行
            Write-Host ""; Read-Host "按回车继续..." | Out-Null; continue
        }
        $rc = Dispatch $choice ""
        if ($rc -eq 127) { ErrMsg "无效选项: $choice"; Start-Sleep -Seconds 1 }
        else {
            if ($rc -ne 0) { ErrMsg "`n命令 '$choice' 失败 (退出码 $rc)" }
            Write-Host ""; Read-Host "按回车继续..." | Out-Null
        }
    }
}

# ---------- 入口 ----------
$cmd = $Command.Trim().ToLower()
switch ($cmd) {
    { $_ -in "", "menu" } { Menu-Loop }
    { $_ -in "-h", "--help", "help" } {
        Get-Content $PSCommandPath | Where-Object { $_ -match '^#' } | ForEach-Object { $_ -replace '^# ?', '' }
    }
    default {
        $el = Invoke-Elevated $cmd $Arg   # 受保护目标 → UAC 提权
        if ($el -eq "done") { exit 0 }
        if ($el -eq "fail") { exit 1 }
        $rc = Dispatch $cmd $Arg
        if ($rc -eq 127) {
            ErrMsg "未知命令: $Command"
            Write-Host "运行 '.\scripts\dev.ps1 --help' 查看可用命令"
            exit 1
        }
        exit $rc
    }
}
