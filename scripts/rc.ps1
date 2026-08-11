# rc = remote cargo —— 把任意 cargo 子命令丢到编译机跑, 本机不占 CPU。
#
# 用法 (在仓库任意位置):
#   .\scripts\rc.ps1 test -p wind-coordinator
#   .\scripts\rc.ps1 test --test input_flow
#   .\scripts\rc.ps1 check -p wind-engine
#   .\scripts\rc.ps1 clippy --workspace
#
# 与 dev.ps1 的分工: dev.ps1 管「完整构建 + 产物回传 + 部署」, rc.ps1 管「细粒度 cargo」。
# 后者不产出 build\ 内容, 故只同步源码 + 远程执行 + 透传输出, 不回传。
#
# 未配置 scripts\build.local.ps1 时不报错, 而是回落到本机跑同一条 cargo 命令 (逻辑在
# remote-build.ps1)。想临时强制本机: $env:WIND_NO_REMOTE = "1"。
# worktree 会自动占用独立的远程槽位, 见 AGENTS.md「远程编译机」。
#
# ⚠️ 远程跑测试前, 编译机上必须有 build_dev\data\ —— 否则依赖词库的测试会静默跳过且
#    计数照绿。remote-build.ps1 会在开跑前拦截这种情况, 不必自己记着。

$ErrorActionPreference = "Stop"

if (-not $args -or $args.Count -eq 0) {
    Write-Host "用法: .\scripts\rc.ps1 <cargo 子命令及参数>" -ForegroundColor Yellow
    Write-Host "例如: .\scripts\rc.ps1 test -p wind-coordinator" -ForegroundColor DarkGray
    exit 2
}

# 含空格的参数补回引号, 否则拼成一条命令后会被远程 shell 拆错词
$parts = $args | ForEach-Object {
    $s = [string]$_
    if ($s -match '\s') { '"' + $s + '"' } else { $s }
}

& "$PSScriptRoot\remote-build.ps1" -Raw ("cargo " + ($parts -join ' '))
exit $LASTEXITCODE
