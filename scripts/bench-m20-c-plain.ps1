# bench-m20-c-plain.ps1 - M20/B200100：C（CAS）挂载视图上 plain `git worktree add`
#（不带 --no-checkout）实测 + 模拟强制全量物化。
#
# 背景（M19 的第三组对比）：M18 未实现 plain add 的拦截/自动注册（shim 与
# daemon watcher 留后续）。本脚本量化两条腿：
#
#   leg A（真实路径）：plain `git worktree add --detach <挂载根>/wt-plain-N`
#     ——挂载根 mkdir 命中 VfsRegistry::auto_register（B1 overlay 兜底路径；
#     C 的 store 无模板 → full-copy fallback），C 的 CAS 层不参与。driver
#     实测后端路由、add/status/diff/commit 耗时、upper 磁盘、remove 首试
#     （无 daemon → initializing 根 rmdir 被拒）与 mark_ready 后重试（模拟
#     daemon watcher）。结果如实记录，不冒充 CAS 流程。
#   leg B（模拟路径）：`worktree add --no-checkout` 建元数据（挂载外真实
#     目录）+ seed + 注册 CAS 视图 + 清位 → 删 index + `git reset --hard`
#     强制经 CAS 视图全量物化（= plain add 的 checkout 阶段会做的事）。
#     **明确是模拟，不是真实 plain add**。记录 checkout/upper/cache/status/
#     diff/commit/首读/remove。
#
# 驱动：scripts/m20-driver（独立 Rust crate，进程内挂载 + 库 API，与 M18
# cas_mount_bench 同款机制；报告落 stderr，标记行 `## M20 C-PLAIN` + KV 块）。
#
# 每轮 fresh 本地 clone 夹具（git/git @HEAD，4,874 文件），与 M19 同口径。
#
# 前置：本仓库 crates/（release 依赖树）可构建；LIBCLANG_PATH 缺省
# %LOCALAPPDATA%\wtfs\tools。driver 首次构建约 30-60 s（复用仓库 target 的
# 第三方产物）。
# 本文件必须保存为 UTF-8 with BOM（PS5.1 无 BOM 会按 ANSI 误读中文）。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\bench-m20-c-plain.ps1
#       [-FixtureRepo <path>] [-CRoot <path>] [-Worktrees 3] [-Rounds 2]
#       [-FirstReadFiles 200] [-DriverTimeoutSec 1500] [-OutFile <path>]
#       [-Build] [-KeepRounds]
#   注意：-Worktrees 同时作用于 leg A 与 leg B（默认 3：leg A 为 full-copy
#   逐文件 fsync，单 wt 可达 30-60 s）。默认跑完清理 %TEMP%\wtfs-m20-r<N>
#   轮次目录，-KeepRounds 保留（内含 driver-stderr.log / driver-stdout.log）。

param(
    [string]$FixtureRepo = "C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo",
    [string]$CRoot = "",
    [int]$Worktrees = 3,
    [int]$Rounds = 2,
    [int]$FirstReadFiles = 200,
    [int]$DriverTimeoutSec = 1500,
    [string]$OutFile = "",
    [string]$RoundDirPrefix = "wtfs-m20",
    [switch]$Build,
    [switch]$KeepRounds
)

$ErrorActionPreference = "Continue"
$ProgressPreference = "SilentlyContinue"
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }

if (-not $CRoot) { $CRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path }
if (-not $OutFile) { $OutFile = Join-Path $env:TEMP "wtfs-m20-results.md" }
$DriverManifest = Join-Path $CRoot "scripts\m20-driver\Cargo.toml"
$DriverExe = Join-Path $CRoot "target\release\wtfs-m20-driver.exe"

if (-not (Test-Path (Join-Path $FixtureRepo ".git"))) { throw "fixture repo not found: $FixtureRepo" }
if (-not (Test-Path $DriverManifest)) { throw "driver manifest not found: $DriverManifest（-CRoot 须为含 scripts/m20-driver 的仓库根）" }

if (-not $env:LIBCLANG_PATH -and (Test-Path "$env:LOCALAPPDATA\wtfs\tools\llvm-23.1.5\bin")) {
    $env:LIBCLANG_PATH = "$env:LOCALAPPDATA\wtfs\tools\llvm-23.1.5\bin"
}
# driver 运行时动态加载 winfsp-x64.dll（WinFSP 安装目录不在会话 PATH 时
# 启动即 0xC0000135 STATUS_DLL_NOT_FOUND——实测踩过）。
$winfspBin = "${env:ProgramFiles(x86)}\WinFsp\bin"
if (-not (Test-Path $winfspBin)) { $winfspBin = "C:\Program Files (x86)\WinFsp\bin" }
if ((Test-Path $winfspBin) -and (($env:PATH -split ";") -notcontains $winfspBin)) {
    $env:PATH = "$winfspBin;$env:PATH"
}
# driver 构建复用仓库 target（第三方依赖产物不重编）。
$env:CARGO_TARGET_DIR = "$CRoot\target"

function Invoke-Step([string]$Title, [string]$Command, [string]$WorkDir, [int]$TimeoutSec,
    [string]$StdoutLog, [string]$StderrLog, [string]$ExitFile) {
    # 经 cmd /c 原生重定向直写文件（OS 级句柄，非 PS 管道）：避免 PS5.1 把
    # stderr 包成 ErrorRecord、避免管道缓冲塞死子进程（M19 同款）。
    # cmd /c 对含特殊字符的命令行会剥离首尾引号（老规则）：所有路径必须
    # 无空格，含空格直接报错而不是悄悄跑错。
    foreach ($q in @($WorkDir, $StdoutLog, $StderrLog, $ExitFile)) {
        if ($q -match "\s") { throw "bench 路径含空格，cmd 重定向无法安全处理: $q" }
    }
    $cmdLine = '/v:on /c {0} > {1} 2> {2} & echo !errorlevel! > {3}' -f `
        $Command, $StdoutLog, $StderrLog, $ExitFile
    $proc = Start-Process -FilePath $env:ComSpec -ArgumentList $cmdLine -NoNewWindow -PassThru `
        -WorkingDirectory $WorkDir
    $exited = $proc.WaitForExit($TimeoutSec * 1000)
    if (-not $exited) {
        & taskkill.exe /T /F /PID $proc.Id *> $null
        try { $proc.Kill() } catch { }
        throw "{0} 未在 {1} s 内退出（已 kill 进程树 pid {2}）；log: {3}" -f $Title, $TimeoutSec, $proc.Id, $StderrLog
    }
    $exitLines = @(Get-Content -LiteralPath $ExitFile -ErrorAction SilentlyContinue)
    $exitText = ""
    if ($exitLines.Count -gt 0) { $exitText = [string]$exitLines[0] }
    $code = $null
    if ($exitText -match "(-?\d+)") { $code = [int]$Matches[1] }
    if ($null -eq $code) { throw "{0} 退出码不可读（文件缺失/为空: '{1}'）；log: {2}" -f $Title, $exitText, $StderrLog }
    return $code
}

if ($Build -or -not (Test-Path $DriverExe)) {
    Write-Host "building driver ($DriverManifest)..."
    $code = Invoke-Step "driver build" `
        ('"{0}" build --release --manifest-path {1}' -f (Get-Command cargo).Source, $DriverManifest) `
        $CRoot $DriverTimeoutSec "$env:TEMP\wtfs-m20-build-out.log" "$env:TEMP\wtfs-m20-build-err.log" `
        "$env:TEMP\wtfs-m20-build-exit.txt"
    if ($code -ne 0) { throw "driver build failed (exit $code); log: $env:TEMP\wtfs-m20-build-err.log" }
    Write-Host "driver: $DriverExe"
}

$results = New-Object System.Collections.Generic.List[string]
$results.Add("# M20 C-mount plain worktree add 基准原始结果（生成于 $(Get-Date -Format 'yyyy-MM-dd HH:mm')）")
$results.Add("")
$results.Add("- fixture: $FixtureRepo (fresh local clone per round, git/git HEAD)")
$results.Add("- C 挂载: $CRoot scripts/m20-driver（进程内 VfsRegistry，同 M18 cas_mount_bench 机制）")
$results.Add("- leg A: plain `git worktree add`（挂载根 mkdir → auto_register 实测路由；无 daemon）")
$results.Add("- leg B: 模拟 = add --no-checkout + seed + 注册 + 清位 + 删 index + reset --hard 强制经 CAS 视图全量物化")
$results.Add("- rounds: $Rounds; worktrees per leg: $Worktrees; first-read files: $FirstReadFiles")
$results.Add("")

$summary = New-Object System.Collections.Generic.List[string]
$summary.Add("## 轮次汇总（每腿 ${Worktrees} wt 中位，ms）")
$summary.Add("")
$summary.Add("| 轮 | A add | A upper add后 KB | A status 冷 | A status D条 | A remove 首试 | A remove 兜底 | A remove daemon | B add 元数据 | B seed | B 强制checkout | B upper checkout后 KB | B status 冷 | B 首读 ms | B 首读 miss | B 缓存 KB |")
$summary.Add("| -- | ----- | ---------------- | ----------- | ------------ | ------------- | ------------- | --------------- | ------------ | ------ | -------------- | --------------------- | ----------- | --------- | ----------- | --------- |")

for ($r = 1; $r -le $Rounds; $r++) {
    Write-Host ""
    Write-Host "===== round $r / $Rounds ====="
    $roundDir = Join-Path $env:TEMP "$RoundDirPrefix-r$r"
    if (Test-Path $roundDir) {
        try { Remove-Item -LiteralPath $roundDir -Recurse -Force -ErrorAction Stop }
        catch { Write-Host "warn: 旧轮次目录清理失败（残留挂载 junction？）: $($_.Exception.Message)；换用新目录" }
    }
    if (-not (Test-Path $roundDir)) { New-Item -ItemType Directory $roundDir | Out-Null }
    $repo = Join-Path $roundDir "repo"

    Write-Host "cloning fixture (local)..."
    & git clone -q $FixtureRepo $repo
    if ($LASTEXITCODE -ne 0) { throw "clone failed" }

    Write-Host "running driver (leg A + leg B, $Worktrees wt)..."
    $stdoutLog = Join-Path $roundDir "driver-stdout.log"
    $stderrLog = Join-Path $roundDir "driver-stderr.log"
    $exitFile = Join-Path $roundDir "driver-exitcode.txt"
    $code = Invoke-Step "m20 driver" `
        ('{0} --repo {1} --work {2} --worktrees {3} --first-read {4}' -f `
            $DriverExe, $repo, $roundDir, $Worktrees, $FirstReadFiles) `
        $CRoot $DriverTimeoutSec $stdoutLog $stderrLog $exitFile
    if ($code -ne 0) {
        $tail = @(Get-Content -LiteralPath $stderrLog -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -Last 15) -join "`n"
        throw "m20 driver failed (exit $code); log tail:`n$tail"
    }

    # 解析 KV + markdown 报告块（stderr）。
    $kv = @{}
    $block = New-Object System.Collections.Generic.List[string]
    $inBlock = $false
    foreach ($line in (Get-Content -LiteralPath $stderrLog -Encoding UTF8)) {
        if ($line -match "^([A-Za-z0-9_]+)=(-?[\d.]+)$") { $kv[$Matches[1]] = [double]$Matches[2]; continue }
        if ($line.StartsWith("### KV")) { $inBlock = $false; continue }
        if ($line.StartsWith("## M20 C-PLAIN")) { $inBlock = $true }
        if ($inBlock -and $line.Trim()) { $block.Add($line) }
    }
    if ($block.Count -lt 4 -or $kv.Count -eq 0) {
        throw "driver 报告解析失败（标记行/KV 缺失）；log: $stderrLog"
    }
    foreach ($k in @("A_add_median_ms","A_remove_fallback_ms","A_remove_daemon_ms","A_status_dirty_lines","B_checkout_ms","B_upper_after_checkout_median_kb","A_backend_overlay_fallback")) {
        if (-not $kv.ContainsKey($k)) { throw "KV 缺少 $k；log: $stderrLog" }
    }

    $results.Add("## Round $r — leg A（真实 plain add，${Worktrees} wt）+ leg B（模拟强制物化，${Worktrees} wt）")
    $results.Add("")
    $results.AddRange($block.ToArray())
    $results.Add("")

    $summary.Add(("| $r | {0:N1} | {1:N1} | {2:N1} | {3} | {4:N1} | {5:N1} | {6:N1} | {7:N1} | {8:N1} | {9:N1} | {10:N1} | {11:N1} | {12:N1} | {13} | {14:N1} |" -f `
        $kv["A_add_median_ms"], $kv["A_upper_after_add_median_kb"], $kv["A_status_cold_ms"],
        [int]$kv["A_status_dirty_lines"], $kv["A_remove_try1_ms"], $kv["A_remove_fallback_ms"], $kv["A_remove_daemon_ms"],
        $kv["B_add_ms"], $kv["B_seed_ms"], $kv["B_checkout_ms"], $kv["B_upper_after_checkout_median_kb"],
        $kv["B_status_cold_ms"], $kv["B_first_read_ms"], [int]$kv["B_first_read_misses"], $kv["B_cache_kb"]))
}

$summary.Add("")
$results.AddRange($summary.ToArray())
$resultsText = $results -join "`r`n"
Set-Content -LiteralPath $OutFile -Value $resultsText -Encoding UTF8
Write-Host ""
Write-Host $resultsText
Write-Host ""
Write-Host "written: $OutFile"

if (-not $KeepRounds) {
    foreach ($rn in (1..$Rounds)) {
        Remove-Item -LiteralPath (Join-Path $env:TEMP "$RoundDirPrefix-r$rn") -Recurse -Force -ErrorAction SilentlyContinue
    }
    Write-Host "round dirs cleaned (use -KeepRounds to retain driver-stderr.log / driver-stdout.log)"
}
