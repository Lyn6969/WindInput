# WindInput 开发菜单
# 用法: .\dev.ps1 或 powershell -File dev.ps1

$ErrorActionPreference = "Stop"
# 目录层级: <产品仓>\wind_input\scripts\dev.ps1
#   ProjectRoot = wind_input    (Cargo workspace 根)
#   ProductRoot = WindInput (产品仓根, 含 docs\VERSION 等共享资产)
$ProjectRoot = Split-Path $PSScriptRoot -Parent
$ProductRoot = Split-Path $ProjectRoot -Parent
$Version = (Get-Content "$ProductRoot\docs\VERSION" -Raw).Trim()
$BuildDir = "$ProjectRoot\build"
$BuildDebugDir = "$ProjectRoot\build_debug"
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

    $goBuild = "$GoRepoRoot\WindInput\build"

    Write-Host "`n从 Go 仓库复制 TSF DLL..." -ForegroundColor Green

    foreach ($dll in @("wind_tsf.dll", "wind_tsf_x86.dll")) {
        $src = "$goBuild\$dll"
        $dst = "$OutDir\$dll"
        if (Test-Path $src) {
            New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
            Copy-Item $src $dst -Force
            Write-Host "已复制: $dll" -ForegroundColor Gray
        } else {
            Write-Host "未找到: $src" -ForegroundColor Yellow
        }
    }
}

function Copy-Data {
    param([string]$OutDir = $BuildDir)

    # 注意：必须用 Go 仓库的 build_debug\data（构建产物，含已下载的 rime 词典 +
    # .schema.toml），而非 WindInput\data（源目录，不含 .dict.yaml 词典文件）。
    # 否则部署后词典缺失，引擎无法构建，只能显示编码无候选。
    $goData = "$GoRepoRoot\WindInput\build_debug\data"
    if (-not (Test-Path "$goData\schemas\wubi86\wubi86_jidian.dict.yaml")) {
        # 回退：若 build_debug 未构建，退回源目录（仅 schema，无词典）
        Write-Host "警告: $goData 缺少词典，回退到源目录(无词典)" -ForegroundColor Yellow
        $goData = "$GoRepoRoot\WindInput\data"
    }

    Write-Host "`n从 Go 仓库复制 data/ ($goData)..." -ForegroundColor Green

    if (Test-Path $goData) {
        New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
        if (Test-Path "$OutDir\data") {
            Remove-Item -Recurse -Force "$OutDir\data"
        }
        Copy-Item "$goData" "$OutDir\data" -Recurse -Force
        Write-Host "已复制: data/" -ForegroundColor Gray
    } else {
        Write-Host "未找到: $goData" -ForegroundColor Yellow
    }
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

# 主循环
while ($true) {
    Show-Menu
    $choice = Read-Host "`n请输入选项"

    switch ($choice.ToLower()) {
        "1"  { Invoke-Build -Debug $false; Pause }
        "1d" { Invoke-Build -Debug $true; Pause }
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
