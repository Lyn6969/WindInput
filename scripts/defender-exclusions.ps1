# Windows Defender 开发排除项配置
#
# 背景:
#   Rust 编译对实时反病毒是最坏负载 —— 不是"几个大文件", 而是数万个中小文件的
#   高频创建/写入/重命名/删除。Defender 的 minifilter 在每次 CreateFile/CloseFile
#   上同步拦截扫描, 链接阶段要打开数千个 .rlib/.o, 扫描开销直接串进关键路径。
#   本仓一个 target 目录量级为 GB / 数万文件, 用 git worktree 并行开发时还要乘以
#   worktree 个数(各自独立 target)。
#
# 策略 —— 进程排除为主, 路径排除为辅:
#   进程排除的语义是"该进程发起的文件 I/O 不扫描"(而非"该进程本身不被扫描"),
#   它与路径无关, 因此【新建 worktree 自动受益, 无需重跑本脚本】。这是路径排除
#   做不到的 —— 路径排除只能覆盖运行脚本那一刻已存在的目录。
#   路径排除挡的是【非编译进程】的扫描: 编辑器索引、搜索、备份软件。
#
#   注意 link.exe / mspdbsrv.exe 等 MSVC 进程在名单里主要是为 Rust 服务:
#   x86_64-pc-windows-msvc 目标的链接阶段调用的正是 MSVC link.exe。
#
# 用法:
#   # 预览将要做的改动 (免管理员)
#   pwsh -File .\scripts\defender-exclusions.ps1 -WhatIfOnly
#
#   # 应用 (需管理员 PowerShell)
#   pwsh -File .\scripts\defender-exclusions.ps1
#
#   # 连同兄弟仓库一起排除 (wind-setting / wind-portable / wind-dict ...)
#   pwsh -File .\scripts\defender-exclusions.ps1 -Scope Workspace
#
#   # 撤销本脚本添加的排除项
#   pwsh -File .\scripts\defender-exclusions.ps1 -Remove
#
# 幂等: 已存在的排除项会跳过, 可重复运行。

[CmdletBinding()]
param(
    # Repo      = 仅本仓库 (默认)
    # Workspace = 本仓库的父目录, 覆盖并列的兄弟仓库
    [ValidateSet('Repo', 'Workspace')]
    [string]$Scope = 'Repo',

    # 额外调整全局扫描策略 (仅空闲时计划扫描 + 降低扫描 CPU 上限)。
    # 默认【不】开启: 这会改动设备级策略, 超出"为本项目加排除项"的范畴。
    [switch]$TuneScanSchedule,

    # 移除本脚本添加的排除项
    [switch]$Remove,

    # 只打印将要做什么, 不实际修改 (免管理员)
    [switch]$WhatIfOnly
)

$ErrorActionPreference = 'Stop'

# ── 路径推导: 一律从脚本自身位置算起, 不硬编码任何机器路径 ──────
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$ScanRoot = if ($Scope -eq 'Workspace') {
    (Resolve-Path (Join-Path $RepoRoot '..')).Path
} else {
    $RepoRoot
}

# ── 管理员校验 ────────────────────────────────────────────────
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin -and -not $WhatIfOnly) {
    Write-Host "需要管理员权限。请在管理员 PowerShell 中运行:" -ForegroundColor Red
    Write-Host "  pwsh -File `"$PSCommandPath`"" -ForegroundColor Yellow
    Write-Host "(加 -WhatIfOnly 可免管理员预览将要做的改动)" -ForegroundColor Yellow
    exit 1
}

# ── 1. 进程排除 (主力: 与路径无关, worktree 自动覆盖) ──────────
$processes = @(
    # Rust 工具链
    'rustc.exe'
    'cargo.exe'
    'rustdoc.exe'
    'rustup.exe'
    'rust-analyzer.exe'
    'cargo-clippy.exe'
    'clippy-driver.exe'
    'rust-lld.exe'
    'sccache.exe'
    # MSVC 工具链 —— Rust 的链接阶段亦经由此, 非仅 C++ 构建所需
    'link.exe'
    'lld-link.exe'
    'mspdbsrv.exe'   # PDB 生成服务, 链接期高频写盘
    'mspdbcmf.exe'
    'cl.exe'
    'lib.exe'
    # C++ (CMake / Visual Studio) 构建
    'MSBuild.exe'
    'tracker.exe'
    'rc.exe'
    'midl.exe'
)

# ── 2. 路径排除 (补充: 包仓库 + 已发现的 target) ───────────────
$paths = [System.Collections.Generic.List[string]]::new()

# Cargo 包仓库与 rustup 工具链: 文件数极多, 且内容由工具链自己校验
$cargoHome  = if ($env:CARGO_HOME)  { $env:CARGO_HOME }  else { Join-Path $env:USERPROFILE '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
$paths.Add((Join-Path $cargoHome 'registry'))
$paths.Add((Join-Path $cargoHome 'git'))
$paths.Add($rustupHome)

# sccache 缓存目录 (若启用): 优先取 SCCACHE_DIR, 否则用其默认位置
$sccacheDir = if ($env:SCCACHE_DIR) {
    $env:SCCACHE_DIR
} else {
    Join-Path $env:LOCALAPPDATA 'Mozilla\sccache'
}
$paths.Add($sccacheDir)

# 扫描范围内所有真实的 cargo 输出目录 (含 worktree 下的)。
# 判据是"同级有 Cargo.toml", 避免把恰好叫 target 的普通目录排除掉。
if (Test-Path $ScanRoot) {
    Get-ChildItem -Path $ScanRoot -Directory -Recurse -Filter 'target' `
                  -Depth 6 -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.Parent.FullName 'Cargo.toml') } |
        ForEach-Object { $paths.Add($_.FullName) }
}

# 通配路径: 让【以后】新建的 worktree / 新增的 crate 也命中路径排除。
# Defender 的 * 只匹配单层名称, 故按深度逐条写出, 不假定具体子目录名。
$paths.Add((Join-Path $ScanRoot '*\target'))
$paths.Add((Join-Path $ScanRoot '*\*\target'))
$paths.Add((Join-Path $RepoRoot '.claude\worktrees\*\target'))
$paths.Add((Join-Path $RepoRoot '.claude\worktrees\*\*\target'))

$paths = @($paths | Sort-Object -Unique)

# ── 应用 ──────────────────────────────────────────────────────
$verb = if ($Remove) { '移除' } else { '添加' }
Write-Host ""
Write-Host "仓库根 : $RepoRoot"
Write-Host "扫描根 : $ScanRoot  (-Scope $Scope)"
Write-Host "操作   : $verb 排除项$(if ($WhatIfOnly) { ' [预览, 不实际修改]' })"

# 非管理员时 Get-MpPreference 不返回真实列表(返回一句提示串), 无从判断哪些已存在,
# 故按"全部待处理"呈现 —— 预览里的 [添加] 是上界而非实数, 明确告知以免误读。
$pref = Get-MpPreference
$existingProc = if ($isAdmin) { @($pref.ExclusionProcess) } else { @() }
$existingPath = if ($isAdmin) { @($pref.ExclusionPath) }    else { @() }
if (-not $isAdmin) {
    Write-Host "注意   : 非管理员无法读取现有排除项, 以下一律列为待处理;" -ForegroundColor DarkYellow
    Write-Host "         实际执行时已存在的会自动跳过。" -ForegroundColor DarkYellow
}

function Invoke-ExclusionChange {
    param(
        [string]$Kind,        # 'Process' | 'Path'
        [string[]]$Items,
        [string[]]$Existing
    )
    Write-Host ""
    Write-Host "=== $(if ($Kind -eq 'Process') { '进程' } else { '路径' })排除 ===" -ForegroundColor Cyan
    foreach ($item in $Items) {
        $present = $Existing -contains $item
        if ($Remove) {
            if (-not $present -and $isAdmin) {
                Write-Host "  [不存在] $item" -ForegroundColor DarkGray
                continue
            }
            Write-Host "  [移除] $item" -ForegroundColor Yellow
            if (-not $WhatIfOnly) {
                if ($Kind -eq 'Process') { Remove-MpPreference -ExclusionProcess $item }
                else                     { Remove-MpPreference -ExclusionPath    $item }
            }
        } else {
            if ($present) {
                Write-Host "  [已有] $item" -ForegroundColor DarkGray
                continue
            }
            Write-Host "  [添加] $item" -ForegroundColor Green
            if (-not $WhatIfOnly) {
                if ($Kind -eq 'Process') { Add-MpPreference -ExclusionProcess $item }
                else                     { Add-MpPreference -ExclusionPath    $item }
            }
        }
    }
}

Invoke-ExclusionChange -Kind 'Process' -Items $processes -Existing $existingProc
Invoke-ExclusionChange -Kind 'Path'    -Items $paths     -Existing $existingPath

# ── 3. 全局扫描策略 (可选, 默认不动) ──────────────────────────
if ($TuneScanSchedule) {
    Write-Host ""
    Write-Host "=== 全局扫描策略 ===" -ForegroundColor Cyan
    if ($Remove) {
        Write-Host "  跳过: 本脚本不恢复扫描策略, 原值未知。" -ForegroundColor DarkGray
        Write-Host "  如需手动还原默认: Set-MpPreference -ScanAvgCPULoadFactor 50 -ScanOnlyIfIdleEnabled `$true" -ForegroundColor DarkGray
    } elseif ($WhatIfOnly) {
        Write-Host "  [预览] ScanOnlyIfIdleEnabled = True" -ForegroundColor Green
        Write-Host "  [预览] ScanAvgCPULoadFactor  = 30" -ForegroundColor Green
    } else {
        Set-MpPreference -ScanOnlyIfIdleEnabled $true
        Set-MpPreference -ScanAvgCPULoadFactor 30
        Write-Host "  ScanOnlyIfIdleEnabled = True" -ForegroundColor Green
        Write-Host "  ScanAvgCPULoadFactor  = 30" -ForegroundColor Green
    }
}

Write-Host ""
Write-Host "完成。验证: Get-MpPreference | Select-Object -ExpandProperty ExclusionProcess" -ForegroundColor Cyan

# 安全取舍
# ────────
# 本脚本放宽的是【开发工具链自身产生的文件】的扫描, 实时保护仍全局开启:
#   - 从浏览器/邮件/移动介质进入的文件照常扫描;
#   - 编译产物【运行时】不在排除范围内 —— 部署到 build_dev/ 后被宿主进程加载那一刻
#     仍受实时保护覆盖。这是刻意保留的, 那才是真正的执行面。
# 残余风险: 若某依赖的 build.rs 是恶意的, 它经 rustc.exe 写出的文件不会被扫描。
# 对应的缓解手段是依赖审计 (cargo-audit / cargo-vet), 而非指望 Defender 拦编译产物。
