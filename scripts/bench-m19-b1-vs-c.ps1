# bench-m19-b1-vs-c.ps1 - B1 (overlay + template) vs C (CAS + manifest) 对比基准。
#
# 同一夹具（git/git @923ecfe，4,874 文件）交替跑两条实现至少 3 轮，每轮
# fresh 本地 clone，避免轮间互相污染：
#
#   B1（commit 0922092，真实 release CLI 流程）：
#     wtfs template create（一次性）→ wtfs mount → 逐 wt：plain
#     `git worktree add`（挂载点内，auto_register）→ ready 轮询 →
#     status 冷/热 → diff --stat → commit -am → remove --force → unmount。
#   C（ff4216f，M18 harness cas_mount_bench，库 API 驱动显式流程）：
#     worktree add --no-checkout + seed + register + 清位（C 的 add 无 CLI
#     接线，M18 边界）；status/diff/commit/remove/首读/缓存口径由 harness
#     打印 markdown 表。
#
# 两条实现的驱动方式不同（B1 = 外部 CLI 二进制 + wtfsd 守护进程；C = 进程内
# harness），FS 路径与 git 操作口径一致；对比时注意这一差异。
#
# 指标：add（B1 含 ready；C 含 seed/注册/清位）、status 冷/热、diff、commit、
# remove、磁盘（B1 template+upper；C upper+标记+hydration 缓存）、首次读
# 200 个 .c/.h、hydration 缓存大小；驱动进程峰值工作集（500ms 采样）。
#
# 前置（两套 checkout 均需先行构建 release，-Build 可代劳）：
#   B1 checkout：`git worktree add --detach %TEMP%\wtfs-b19-b1 0922092`
#                且 `cargo build --release`（产出 wtfs.exe + wtfsd.exe）
#   C  checkout：本仓库（ff4216f）
#                `cargo test --release -p wtfs-vfs --test cas_mount_bench --no-run`
#   LIBCLANG_PATH 指向 winfsp-sys 所需 clang（缺省 %LOCALAPPDATA%\wtfs\tools）。
#   本文件必须保存为 UTF-8 with BOM（PS5.1 无 BOM 会按 ANSI 误读中文）。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\bench-m19-b1-vs-c.ps1
#       [-FixtureRepo <path>] [-CRoot <path>] [-B1Root <path>] [-B1Commit <sha>]
#       [-Worktrees 10] [-Rounds 3] [-FirstReadFiles 200] [-OutFile <path>] [-Build]

param(
    [string]$FixtureRepo = "C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo",
    [string]$CRoot = "",
    [string]$B1Root = "",
    [string]$B1Commit = "0922092",
    [int]$Worktrees = 10,
    [int]$Rounds = 3,
    [int]$FirstReadFiles = 200,
    [string]$OutFile = "",
    [switch]$Build
)

$ErrorActionPreference = "Continue" # PS5.1: native stderr 不作终止错误；以 $LASTEXITCODE 判定
$ProgressPreference = "SilentlyContinue"
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { } # 原生进程 UTF-8 输出按 UTF-8 解码

if (-not $CRoot) { $CRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path }
if (-not $B1Root) { $B1Root = Join-Path $env:TEMP "wtfs-b19-b1" }
if (-not $OutFile) { $OutFile = Join-Path $env:TEMP "wtfs-b19-results.md" }
$B1Exe = Join-Path $B1Root "target\release\wtfs.exe"

if (-not (Test-Path (Join-Path $FixtureRepo ".git"))) { throw "fixture repo not found: $FixtureRepo" }
if (-not (Test-Path $B1Exe)) { throw "B1 wtfs.exe not found: $B1Exe (先在 $B1Root 构建 release)" }

if (-not $env:LIBCLANG_PATH -and (Test-Path "$env:LOCALAPPDATA\wtfs\tools\llvm-23.1.5\bin")) {
    $env:LIBCLANG_PATH = "$env:LOCALAPPDATA\wtfs\tools\llvm-23.1.5\bin"
}

function Invoke-Git([string]$Repo, [string[]]$GitArgs) {
    # 带基准同款身份；输出为 stdout+stderr 合并文本。
    # 注意：不能命名为 Git——PS 函数名大小写不敏感，会遮蔽 git 可执行文件。
    $out = & git -c "user.name=wtfs-bench" -c "user.email=wtfs-bench@example.com" -C $Repo @GitArgs 2>&1
    return @{ Ok = ($LASTEXITCODE -eq 0); Text = ($out | ForEach-Object { $_.ToString() }) -join "`n" }
}

function Assert-Git([string]$Repo, [string[]]$GitArgs, [string]$What) {
    $r = Invoke-Git $Repo $GitArgs
    if (-not $r.Ok) { throw "$What failed: git $($GitArgs -join ' ')`n$($r.Text)" }
    return $r.Text
}

function Timed-Git([string]$Repo, [string[]]$GitArgs, [string]$What) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $text = Assert-Git $Repo $GitArgs $What
    $sw.Stop()
    return @{ Ms = $sw.Elapsed.TotalMilliseconds; Text = $text }
}

function Median([double[]]$Values) {
    $sorted = $Values | Sort-Object
    $n = $sorted.Count
    if ($n -eq 0) { return 0.0 }
    if ($n % 2 -eq 1) { return $sorted[[int](($n - 1) / 2)] }
    return ($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2.0
}

function Dir-Bytes([string]$Dir) {
    # (bytes, files)：目录不存在按零计。
    if (-not (Test-Path $Dir)) { return @{ Bytes = [int64]0; Files = 0 } }
    $m = Get-ChildItem -LiteralPath $Dir -Recurse -File -Force -ErrorAction SilentlyContinue |
        Measure-Object -Property Length -Sum
    $sum = [int64]0
    if ($m.Sum) { $sum = [int64]$m.Sum }
    return @{ Bytes = $sum; Files = $m.Count }
}

function Start-MemSampler([string]$Tag, [string]$PathPrefix) {
    $stopFile = Join-Path $env:TEMP "wtfs-b19-stop-$Tag"
    Remove-Item $stopFile -ErrorAction SilentlyContinue
    return Start-Job -ArgumentList $PathPrefix, $stopFile -ScriptBlock {
        param($pfx, $stopPath)
        $peak = [int64]0; $who = ""
        while (-not (Test-Path $stopPath)) {
            Get-Process -ErrorAction SilentlyContinue | Where-Object {
                try { return $_.Path -and $_.Path.StartsWith($pfx, [StringComparison]::OrdinalIgnoreCase) } catch { return $false }
            } | ForEach-Object {
                if ($_.WorkingSet64 -gt $peak) { $peak = $_.WorkingSet64; $who = "$($_.ProcessName) pid $($_.Id)" }
            }
            Start-Sleep -Milliseconds 500
        }
        [pscustomobject]@{ PeakMB = [math]::Round($peak / 1MB, 1); Who = $who }
    }
}

function Stop-MemSampler($Job, [string]$Tag) {
    Set-Content -LiteralPath (Join-Path $env:TEMP "wtfs-b19-stop-$Tag") -Value "stop" -ErrorAction SilentlyContinue
    $null = Wait-Job $Job -Timeout 15
    $r = @(Receive-Job $Job -ErrorAction SilentlyContinue)
    Remove-Job $Job -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $env:TEMP "wtfs-b19-stop-$Tag") -ErrorAction SilentlyContinue
    if ($r.Count -gt 0) { return $r[-1] }
    return [pscustomobject]@{ PeakMB = 0.0; Who = "(未捕获)" }
}

function Wait-B1Ready([string]$Pipe, [string]$Name, [int]$TimeoutSec) {
    # 轮询 `wtfs status --json`，直到该 worktree state=ready。返回等待毫秒 + worktree_id。
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        $raw = & $B1Exe --pipe $Pipe status --json 2>$null
        if ($LASTEXITCODE -eq 0 -and $raw) {
            try {
                $j = (($raw | ForEach-Object { $_.ToString() }) -join "`n") | ConvertFrom-Json
                $wt = $j.status.worktrees | Where-Object { $_.name -eq $Name } | Select-Object -First 1
                if ($wt -and $wt.state -eq "ready") { $sw.Stop(); return @{ Ms = $sw.Elapsed.TotalMilliseconds; Id = $wt.worktree_id } }
            } catch { }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "timeout waiting for worktree $Name ready (pipe $Pipe)"
}

function Cas-Val([System.Collections.Generic.List[string]]$Block, [string]$Prefix) {
    # 从 harness markdown 表提取某行的数值（取最后一列首个数字，避免吃进标签里的数字）。
    foreach ($line in $Block) {
        if ($line.StartsWith("| $Prefix")) {
            $cells = ($line.Trim() -split "\|")
            $value = $cells[$cells.Count - 2]
            if ($value -match "([\d.]+)") { return [double]$Matches[1] }
        }
    }
    return 0.0
}

if ($Build) {
    Write-Host "building B1 ($B1Root, commit $B1Commit) release..."
    & cargo build --release --manifest-path (Join-Path $B1Root "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "B1 cargo build --release failed" }
    Write-Host "building C ($CRoot) cas_mount_bench release test binary..."
    Push-Location $CRoot
    try {
        & cargo test --release -p wtfs-vfs --test cas_mount_bench --no-run
        if ($LASTEXITCODE -ne 0) { throw "C cargo test --no-run failed" }
    } finally { Pop-Location }
}

# 开局清理：残留的上一轮 wtfsd（崩溃遗留）按轮管道名 + 缺省管道尽力卸载。
foreach ($p in (1..($Rounds + 1))) {
    & $B1Exe --pipe "\\.\pipe\wtfs-b19-r$p" unmount *> $null
}
& $B1Exe unmount *> $null

$results = New-Object System.Collections.Generic.List[string]
$results.Add("# M19 B1 vs C 基准原始结果（生成于 $(Get-Date -Format 'yyyy-MM-dd HH:mm')）")
$results.Add("")
$results.Add("- fixture: $FixtureRepo (fresh local clone per round)")
$results.Add("- B1: commit $B1Commit @ $B1Root (release CLI: wtfs + wtfsd)")
$results.Add("- C: ff4216f @ $CRoot (cargo test cas_mount_bench, in-process harness)")
$results.Add("- rounds: $Rounds, worktrees/round: $Worktrees, first-read files: $FirstReadFiles")
$results.Add("")

$summary = New-Object System.Collections.Generic.List[string]
$summary.Add("## 轮次汇总（10 wt 中位数，ms）")
$summary.Add("")
$summary.Add("| 轮 | 实现 | add | ready/seed | status 冷 | status 热 | diff | commit | remove | 首读合计 | 驱动峰值 WS MB |")
$summary.Add("| -- | ---- | --- | ---------- | --------- | --------- | ---- | ------ | ------ | -------- | -------------- |")

for ($r = 1; $r -le $Rounds; $r++) {
    Write-Host ""
    Write-Host "===== round $r / $Rounds ====="
    $roundDir = Join-Path $env:TEMP "wtfs-b19-r$r"
    if (Test-Path $roundDir) {
        try { Remove-Item -LiteralPath $roundDir -Recurse -Force } catch { throw "cannot clear round dir ${roundDir}: $($_.Exception.Message)" }
    }
    New-Item -ItemType Directory $roundDir | Out-Null
    $repo = Join-Path $roundDir "repo"

    Write-Host "cloning fixture (local)..."
    & git clone -q $FixtureRepo $repo
    if ($LASTEXITCODE -ne 0) { throw "clone failed" }
    $head = (Assert-Git $repo @("rev-parse", "HEAD") "rev-parse").Trim()
    $fileCount = ((Assert-Git $repo @("ls-files") "ls-files") -split "`n" | Where-Object { $_ }).Count
    Write-Host "fixture: $repo ($fileCount files @ $($head.Substring(0,8)))"

    # ================= B1 leg =================
    Write-Host "--- B1 leg (CLI flow @ $B1Commit) ---"
    $storeB1 = Join-Path $roundDir "store-b1"
    $pipe = "\\.\pipe\wtfs-b19-r$r"
    $mountPoint = Join-Path $repo ".tower\worktrees"
    New-Item -ItemType Directory -Force $mountPoint | Out-Null

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $tplOut = & $B1Exe template create --repo $repo --base master --store-root $storeB1 2>&1
    if ($LASTEXITCODE -ne 0) { throw "template create failed: $($tplOut | ForEach-Object { $_.ToString() })" }
    $templateMs = $sw.Elapsed.TotalMilliseconds
    Write-Host ("template create: {0:N1} ms" -f $templateMs)

    $sw.Restart()
    $mountOut = & $B1Exe --pipe $pipe mount --repo $repo --base master --store-root $storeB1 2>&1
    if ($LASTEXITCODE -ne 0) { throw "mount failed: $($mountOut | ForEach-Object { $_.ToString() })" }
    $mountMs = $sw.Elapsed.TotalMilliseconds
    Write-Host ("mount: {0:N1} ms" -f $mountMs)

    $b1Job = Start-MemSampler "b1-r$r" (Join-Path $B1Root "target\release")

    $add = @(); $ready = @(); $cold = @(); $hot = @(); $diff = @(); $commit = @(); $remove = @()
    $upperBytes = @(); $upperFiles = @(); $firstReadMs = 0.0; $firstReadKB = 0.0
    for ($i = 1; $i -le $Worktrees; $i++) {
        $name = "wt-b1-$i"
        $view = Join-Path $mountPoint $name
        $t = Timed-Git $repo @("worktree", "add", "--detach", $view, "HEAD") "b1 add $name"
        $add += $t.Ms
        $rd = Wait-B1Ready $pipe $name 60
        $ready += $rd.Ms
        $wid = $rd.Id

        $t = Timed-Git $view @("status", "--porcelain") "b1 status cold $name"
        if ($t.Text.Trim()) { throw "b1 $name cold status not clean: $($t.Text)" }
        $cold += $t.Ms
        $t = Timed-Git $view @("status", "--porcelain") "b1 status hot $name"
        if ($t.Text.Trim()) { throw "b1 $name hot status not clean: $($t.Text)" }
        $hot += $t.Ms

        $t = Timed-Git $view @("diff", "--stat") "b1 diff $name"
        if ($t.Text.Trim()) { throw "b1 $name diff not empty: $($t.Text)" }
        $diff += $t.Ms

        [IO.File]::WriteAllText((Join-Path $view "README.md"), "# bench $i`n", (New-Object Text.UTF8Encoding($false)))
        $t = Timed-Git $view @("commit", "-am", "bench edit", "-q") "b1 commit $name"
        $commit += $t.Ms
        $null = Assert-Git $view @("status", "--porcelain") "b1 post-commit status $name" # 未计时

        $up = Join-Path $storeB1 "worktrees\wt-$wid\upper"
        $d = Dir-Bytes $up
        $upperBytes += $d.Bytes; $upperFiles += $d.Files

        if ($i -eq 1) {
            $sample = (Assert-Git $view @("ls-files", "*.c", "*.h") "ls-files sample") -split "`n" |
                Where-Object { $_ } | Select-Object -First $FirstReadFiles
            $sw.Restart()
            foreach ($p in $sample) {
                $bytes = [IO.File]::ReadAllBytes((Join-Path $view $p))
                $firstReadKB += $bytes.Length / 1024.0
            }
            $sw.Stop()
            $firstReadMs = $sw.Elapsed.TotalMilliseconds
        }
    }

    $tplParent = Join-Path $storeB1 "templates"
    $tplD = Dir-Bytes $tplParent
    $storeTotals = Dir-Bytes $storeB1

    $tRemoveFirst = 0.0
    for ($i = 1; $i -le $Worktrees; $i++) {
        $view = Join-Path $mountPoint "wt-b1-$i"
        $t = Timed-Git $repo @("worktree", "remove", "--force", $view) "b1 remove wt-b1-$i"
        if ($i -eq 1) { $tRemoveFirst = $t.Ms }
        $remove += $t.Ms
    }

    & $B1Exe --pipe $pipe unmount *> $null
    if ($LASTEXITCODE -ne 0) { Write-Host "warn: unmount exit $LASTEXITCODE" }
    $b1Mem = Stop-MemSampler $b1Job "b1-r$r"
    & git -C $repo worktree prune *> $null

    $results.Add("## Round $r — B1（CLI @ $B1Commit）")
    $results.Add("")
    $results.Add(("- template create（一次性）: {0:N1} ms；mount: {1:N1} ms" -f $templateMs, $mountMs))
    $results.Add(("- add（plain worktree add，10 wt 中位）: {0:N1} ms；各 wt: {1}" -f (Median $add), (($add | ForEach-Object { [math]::Round($_, 1) }) -join ", ")))
    $results.Add(("- ready（轮询至 ready，10 wt 中位）: {0:N1} ms；add+ready 中位: {1:N1} ms" -f (Median $ready), ((Median $add) + (Median $ready))))
    $results.Add(("- status 冷/热（中位）: {0:N1} / {1:N1} ms" -f (Median $cold), (Median $hot)))
    $results.Add(("- diff --stat（中位）: {0:N1} ms" -f (Median $diff)))
    $results.Add(("- commit -am 单文件（中位）: {0:N1} ms" -f (Median $commit)))
    $results.Add(("- remove --force（中位）: {0:N1} ms（首个 {1:N1} ms）" -f (Median $remove), $tRemoveFirst))
    $results.Add(("- 首读 {0} 个 .c/.h: {1:N1} ms 合计（{2:N3} ms/文件，{3:N0} KB）" -f $FirstReadFiles, $firstReadMs, ($firstReadMs / $FirstReadFiles), $firstReadKB))
    $results.Add(("- upper 磁盘（10 wt 合计）: {0} 文件 / {1:N1} KB；templates: {2} 文件 / {3:N1} MB；store 总计: {4:N1} MB" -f `
        (($upperFiles | Measure-Object -Sum).Sum), (($upperBytes | Measure-Object -Sum).Sum / 1024.0), $tplD.Files, ($tplD.Bytes / 1MB), ($storeTotals.Bytes / 1MB)))
    $results.Add(("- 驱动进程峰值 WS: {0} MB（{1}）" -f $b1Mem.PeakMB, $b1Mem.Who))
    $results.Add("")

    $summary.Add(("| $r | B1 | {0:N1} | {1:N1} | {2:N1} | {3:N1} | {4:N1} | {5:N1} | {6:N1} | {7:N1} | {8} |" -f `
        (Median $add), (Median $ready), (Median $cold), (Median $hot), (Median $diff), (Median $commit), (Median $remove), $firstReadMs, $b1Mem.PeakMB))

    # ================= C leg =================
    Write-Host "--- C leg (harness cas_mount_bench @ ff4216f) ---"
    $env:WTFS_BENCH_REPO = $repo
    $casLog = Join-Path $roundDir "cas-stderr.log"
    $casOutLog = Join-Path $roundDir "cas-stdout.log"
    $memJob = Start-MemSampler "cas-r$r" (Join-Path $CRoot "target\release\deps")
    # 经 Start-Process 直写文件，避免 PS5.1 把 stderr 包成 ErrorRecord（其
    # ToString 会注入 "System.Management.Automation.RemoteException" 垃圾行，
    # 破坏 harness 报告表）。harness 报告走 eprintln → stderr。
    $cargo = Get-Command cargo
    $p = Start-Process -FilePath $cargo.Source -NoNewWindow -Wait -PassThru `
        -WorkingDirectory $CRoot `
        -RedirectStandardOutput $casOutLog -RedirectStandardError $casLog `
        -ArgumentList "test", "--release", "-p", "wtfs-vfs", "--test", "cas_mount_bench", "--", "--ignored", "--nocapture"
    Remove-Item Env:WTFS_BENCH_REPO -ErrorAction SilentlyContinue
    if ($p.ExitCode -ne 0) { throw "cas_mount_bench failed (exit $($p.ExitCode)); log: $casLog" }
    $casMem = Stop-MemSampler $memJob "cas-r$r"
    & git -C $repo worktree prune *> $null

    # stderr（报告 + cargo 构建信息）在前，stdout（cargo test 头尾）在后。
    $casText = @(@(Get-Content -LiteralPath $casLog -Encoding UTF8) + @(Get-Content -LiteralPath $casOutLog -Encoding UTF8))
    $begin = -1
    for ($li = 0; $li -lt $casText.Count; $li++) {
        if ($casText[$li] -and $casText[$li].StartsWith("## M18 CAS")) { $begin = $li; break }
    }
    if ($begin -lt 0) { throw "CAS report marker not found in round $r log" }
    # 容忍标记与表格之间的任意杂质行：直接收集标记之后的全部表格行。
    $block = New-Object System.Collections.Generic.List[string]
    $block.Add($casText[$begin])
    foreach ($line in ($casText | Select-Object -Skip ($begin + 1))) {
        if ($line -and $line.StartsWith("|")) { $block.Add($line) }
    }
    if ($block.Count -lt 4) { throw "CAS report table rows missing in round $r log" }

    $results.Add("## Round $r — C（harness @ ff4216f）")
    $results.Add("")
    $results.AddRange($block.ToArray())
    $results.Add(("- 驱动进程峰值 WS: {0} MB（{1}；进程内 harness，含 manifest/registry 内存，非纯 FS 层）" -f $casMem.PeakMB, $casMem.Who))
    $results.Add("")

    $summary.Add(("| $r | C | {0:N1} | {1:N1} | {2:N1} | {3:N1} | {4:N1} | {5:N1} | {6:N1} | {7:N1} | {8} |" -f `
        (Cas-Val $block "add（"), (Cas-Val $block "种子（"), (Cas-Val $block "status 冷"), (Cas-Val $block "status 热"), (Cas-Val $block "diff"),
        (Cas-Val $block "commit"), (Cas-Val $block "remove"), (Cas-Val $block "首次读"), $casMem.PeakMB))
}

$summary.Add("")
$results.AddRange($summary.ToArray())
$resultsText = $results -join "`r`n"
Set-Content -LiteralPath $OutFile -Value $resultsText -Encoding UTF8
Write-Host ""
Write-Host $resultsText
Write-Host ""
Write-Host "written: $OutFile"
