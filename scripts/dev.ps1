# WindInput 开发菜单
# 用法: .\dev.ps1 或 powershell -File dev.ps1

$ErrorActionPreference = "Stop"
# 目录层级: <产品仓>\scripts\dev.ps1 （产品级编排脚本，统管 wind_input\ 及未来的 tsf\macos\）
#   ProductRoot = <产品仓>          (产品仓根, 含 docs\VERSION、data\ 等共享资产)
#   ProjectRoot = <产品仓>\wind_input (Cargo workspace 根)
$ProductRoot = Split-Path $PSScriptRoot -Parent
$ProjectRoot = "$ProductRoot\wind_input"
$Version = (Get-Content "$ProductRoot\docs\VERSION" -Raw).Trim()
$BuildDir = "$ProjectRoot\build"
$BuildDebugDir = "$ProjectRoot\build_debug"
$LocalData = "$ProductRoot\data"
$DictProbe = "schemas\wubi86\wubi86_jidian.dict.yaml"
# Go 仓库与产品仓同级
$GoRepoRoot = Split-Path $ProductRoot -Parent

function Show-Menu {
    Clear-Host
    Write-Host "============================================" -ForegroundColor Cyan
    Write-Host "  WindInput 开发菜单  v$Version" -ForegroundColor Cyan
    Write-Host "============================================" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  构建:" -ForegroundColor Yellow
    Write-Host "    1  - Release 构建 + 部署" -ForegroundColor White
    Write-Host "    1d - Debug 构建 + 部署" -ForegroundColor White
    Write-Host "    a  - 一键全编译 (core; ad = debug)" -ForegroundColor White
    Write-Host "    2  - cargo check (快速编译检查)" -ForegroundColor White
    Write-Host "    3  - cargo clippy (代码检查)" -ForegroundColor White
    Write-Host "    4  - cargo test (运行测试)" -ForegroundColor White
    Write-Host ""
    Write-Host "  部署:" -ForegroundColor Yellow
    Write-Host "    5  - 完整部署 (复制 DLL + 数据)" -ForegroundColor White
    Write-Host "    6  - 从 Go 仓库复制 TSF DLL" -ForegroundColor White
    Write-Host "    7  - 从 Go 仓库复制 data/" -ForegroundColor White
    Write-Host ""
    Write-Host "  工具:" -ForegroundColor Yellow
    Write-Host "    f  - cargo fmt (代码格式化)" -ForegroundColor White
    Write-Host "    c  - cargo clean (清理构建)" -ForegroundColor White
    Write-Host "    q  - 退出" -ForegroundColor White
    Write-Host ""
    Write-Host "============================================" -ForegroundColor Cyan
}

function Invoke-Build {
    param([bool]$Debug = $false)
    $profile = if ($Debug) { "debug" } else { "release" }
    $outDir = if ($Debug) { $BuildDebugDir } else { $BuildDir }
    $suffix = if ($Debug) { "_debug" } else { "" }

    Write-Host "`n正在构建 ($profile)..." -ForegroundColor Green
    Set-Location $ProjectRoot

    if ($Debug) {
        cargo build --features debug_variant 2>&1
    } else {
        cargo build --release 2>&1
    }

    if ($LASTEXITCODE -ne 0) {
        Write-Host "构建失败!" -ForegroundColor Red
        return
    }

    # 创建输出目录
    New-Item -ItemType Directory -Path $outDir -Force | Out-Null

    # 复制二进制文件
    $binSuffix = if ($Debug) { "debug" } else { "release" }
    $srcExe = "$ProjectRoot\target\$binSuffix\wind_input.exe"
    $dstExe = "$outDir\wind_input${suffix}.exe"

    if (Test-Path $srcExe) {
        Copy-Item $srcExe $dstExe -Force
        Write-Host "已复制: wind_input${suffix}.exe" -ForegroundColor Gray
    }

    # 复制 TSF DLL
    Copy-TsfDll -OutDir $outDir -Suffix $suffix

    # 复制数据文件
    Copy-Data -OutDir $outDir

    Write-Host "构建完成!" -ForegroundColor Green
}

function Invoke-Check {
    Write-Host "`n正在运行 cargo check..." -ForegroundColor Green
    Set-Location $ProjectRoot
    cargo check 2>&1
}

function Invoke-Clippy {
    Write-Host "`n正在运行 cargo clippy..." -ForegroundColor Green
    Set-Location $ProjectRoot
    cargo clippy 2>&1
}

function Invoke-Test {
    Write-Host "`n正在运行 cargo test..." -ForegroundColor Green
    Set-Location $ProjectRoot
    cargo test 2>&1
}

function Invoke-Fmt {
    Write-Host "`n正在运行 cargo fmt..." -ForegroundColor Green
    Set-Location $ProjectRoot
    cargo fmt 2>&1
}

function Invoke-Clean {
    Write-Host "`n正在运行 cargo clean..." -ForegroundColor Green
    Set-Location $ProjectRoot
    cargo clean 2>&1
}

function Copy-TsfDll {
    param([string]$OutDir = $BuildDir, [string]$Suffix = "")

    Write-Host "`n复制 TSF DLL (暂复用 Go 仓库产物, 尚无 Rust 版)..." -ForegroundColor Green
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    $found = $false
    foreach ($dll in @("wind_tsf.dll", "wind_tsf_x86.dll")) {
        foreach ($base in @("$GoRepoRoot\WindInput\build", "$GoRepoRoot\WindInput\build_debug")) {
            $src = "$base\$dll"
            if (Test-Path $src) {
                Copy-Item $src "$OutDir\$dll" -Force
                Write-Host "已复制: $dll (来自 $base)" -ForegroundColor Gray
                $found = $true
                break
            }
        }
    }
    if (-not $found) {
        Write-Host "未找到 Go TSF DLL (Go 仓库未构建); 仅本地完整镜像需要它。" -ForegroundColor Gray
    }
}

function Copy-Data {
    param([string]$OutDir = $BuildDir)

    # data 来源优先级：① 本机真实词库 ($LocalData) ② Go 构建产物 (build_debug\data)
    # ③ Go 源目录 (仅 schema, 无 .dict.yaml → 引擎无候选)。优先真实词库，避免缺词典回退。
    $src = $null
    if (Test-Path "$LocalData\$DictProbe") {
        $src = $LocalData
        Write-Host "data 源: 本机真实词库 $LocalData" -ForegroundColor Gray
    } elseif (Test-Path "$GoRepoRoot\WindInput\build_debug\data\$DictProbe") {
        $src = "$GoRepoRoot\WindInput\build_debug\data"
        Write-Host "data 源: Go 构建产物 $src" -ForegroundColor Gray
    } elseif (Test-Path "$GoRepoRoot\WindInput\data") {
        $src = "$GoRepoRoot\WindInput\data"
        Write-Host "data 源: Go 源目录 (仅 schema, 无词典)" -ForegroundColor Yellow
    } else {
        Write-Host "找不到任何 data 源; 跳过 data 复制" -ForegroundColor Yellow
        return
    }

    Write-Host "`n复制 data/ ($src)..." -ForegroundColor Green
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    if (Test-Path "$OutDir\data") {
        Remove-Item -Recurse -Force "$OutDir\data"
    }
    Copy-Item "$src" "$OutDir\data" -Recurse -Force
    Write-Host "已复制: data/" -ForegroundColor Gray
}

function Deploy-All {
    param([bool]$Debug = $false)
    $outDir = if ($Debug) { $BuildDebugDir } else { $BuildDir }
    $suffix = if ($Debug) { "_debug" } else { "" }

    New-Item -ItemType Directory -Path $outDir -Force | Out-Null
    Copy-TsfDll -OutDir $outDir -Suffix $suffix
    Copy-Data -OutDir $outDir
    Write-Host "部署完成!" -ForegroundColor Green
}

# 一键全编译（类 Go）：core(exe+TSF+data) 进 build/。
function Build-All {
    param([bool]$Debug = $false)
    Invoke-Build -Debug $Debug
    $outDir = if ($Debug) { $BuildDebugDir } else { $BuildDir }
    Write-Host "`n一键全编译完成 -> $outDir" -ForegroundColor Green
}

# 主循环
while ($true) {
    Show-Menu
    $choice = Read-Host "`n请输入选项"

    switch ($choice.ToLower()) {
        "1"  { Invoke-Build -Debug $false; Pause }
        "1d" { Invoke-Build -Debug $true; Pause }
        "a"  { Build-All -Debug $false; Pause }
        "ad" { Build-All -Debug $true; Pause }
        "2"  { Invoke-Check; Pause }
        "3"  { Invoke-Clippy; Pause }
        "4"  { Invoke-Test; Pause }
        "5"  { Deploy-All; Pause }
        "6"  { Copy-TsfDll; Pause }
        "7"  { Copy-Data; Pause }
        "f"  { Invoke-Fmt; Pause }
        "c"  { Invoke-Clean; Pause }
        "q"  { exit }
        default { Write-Host "无效选项" -ForegroundColor Red; Start-Sleep -Seconds 1 }
    }
}
