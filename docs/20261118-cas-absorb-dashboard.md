# CAS checkout 吸收器与 dashboard 10×10 基准

日期：2026-11-18
基线：`master` @ `16810e4` + 本次未提交的吸收器改动

## 1. 这次实现了什么

在 C（CAS 视图）的 checkout 窗口加了一个“内存吸收器”：

- `CasBackend` 新增 `absorbing` 标志（`crates/wtfs-vfs/src/backend.rs`）；
- `absorbing = true` 时 `ViewBackend::is_initializing()` 返回 true，VFS 的
  `ensure_write_state` 走已有的 **Staged 内存缓冲**；
- close 时 `finish_staged` 按 git blob sha1 回查 manifest oid：
  - 一致 → 丢弃 buffer，零 upper、零 fsync，读回落 manifest；
  - 不一致（CRLF/filter/真实修改）→ 写 upper，恰好一次 fsync；
- `CasView::upper_abs` 统一把 `/` 换成 `\`，并支持 `\\?\` 长路径
  （dashboard 的 `specs/...` 路径超过 MAX_PATH）；
- CAS 暂存句柄的 `projected_staged_stat` 不再报错：manifest 同尺寸报固定
  纪元，否则报当前时间，保证 Git 在 checkout 中能 stat。

### 门控

已实现：**路径 = 新 worktree + 状态 = initializing**。
未实现：**进程 = git**——WinFSP 回调拿不到 PID（只有用户 SID），ProjFS 才能
拿到 `TriggeringProcessId` / `TriggeringProcessImageFileName`（已用最小
provider 实测）。当前 benchmark 在注册后手动 `set_absorbing(true)`，checkout
结束后 `set_absorbing(false)`；自动按路径/进程激活还未接线。

## 2. 基准口径

- 夹具：`dashboard` 的本地 clone，去掉 2 个 gitlink（submodule）后
  **791 tracked files**，HEAD `260557e`；去掉 gitlink 是为了避免 lazy seed
  的 ` T` 类型变化污染 status（与之前 git/git 基准去掉 submodule 同口径）。
- native / B1：`m0_benchmark`，`WTFS_BENCH_ITERS=10`，每次 10 worktree。
- C lazy：`cas_mount_bench`，`--no-checkout` + seed + 注册 + 清位，10 worktree。
- C absorb：同一 harness，`--no-checkout` + stat config + 注册 +
  `set_absorbing(true)` + `git reset --hard` + `set_absorbing(false)`，10 worktree。
- 每种变体跑 **10 轮**，表中为轮中位数的中位（native/B1 的 add 是 10 个
  worktree 的中位，再对 10 轮取中位）。

## 3. 结果（中位）

| 指标 | native | B1 | C lazy | C absorb |
|---|---:|---:|---:|---:|
| worktree add（全程） | 952 ms | 996 ms | **263 ms**（add 53 + setup 210） | 1,597 ms（add 46 + setup 50 + checkout 1,501） |
| status 冷 | 194 ms | **74 ms** | 137 ms | 137 ms |
| status 热 | 156 ms | **73 ms** | 134 ms | 116 ms |
| diff（干净树） | — | — | 230 ms | 222 ms |
| commit -am 单文件 | — | — | 407 ms | 294 ms |
| remove | 445 ms | 823 ms | **45 ms** | 55 ms |
| 首次读 200 文件 | — | — | 982 ms | 859 ms |
| upper（10 wt 合计） | 425.6 MB（整树） | 0.1 KB + 42.5 MB 共享模板 | **1 KB** | 17.2 MB |
| hydration 缓存 | — | — | 6.5 MB | 6.5 MB |
| 一次性 template + mount | — | 1.4 s + 50 ms | — | — |

## 4. 解读

- **absorb 不是 add 最快的**：dashboard 上 1.6 s，慢于 native 0.95 s 和 B1
  1.0 s。原因是 Git 仍然 checkout 791 个文件，absorb 只省了“写 upper +
  fsync”，没有省 WinFSP 回调和 Git 自身 checkout。
- **absorb 赢在磁盘**：10 个 worktree 只有 17.2 MB upper（300 个差异文件），
  而 native 是 425.6 MB（10 × 42.5 MB），B1 是 42.5 MB 共享模板 + 0.1 KB
  upper；absorb 没有 template 一次性成本。
- **C lazy 仍然最快、最省**：0.26 s add、1 KB upper、6.5 MB 缓存；代价是
  必须 `--no-checkout` + seed，plain `git worktree add` 不会自动走这条路。
- **status**：B1 74/73 ms 最快；C 137/116 ms，absorb 热态比 lazy 略快。
- **remove**：B1 823 ms，native 445 ms，C 45–55 ms；C 的真实目录只有
  `.git` + 稀疏 upper。
- **commit**：absorb 294 ms vs lazy 407 ms，因为 absorb 已经把差异文件
  物化到 upper，commit 时少了一次 hydrate/回收。

## 5. 限制与后续

1. **进程门控没实现**：WinFSP 没有 PID；要用 `git.exe` 作为条件，需要
   ProjFS（`TriggeringProcessId`）或 ETW/minifilter。当前只有路径 +
   initializing。
2. **自动激活没实现**：benchmark 是显式 `set_absorbing`；plain
   `git worktree add` 的 auto-register 仍会建 B1 overlay（M20 leg A）。
   要做“无 shim 的 plain add”，需要在 registry 里加 lazy CAS backend：
   新 worktree 首次访问时读 HEAD、建 manifest、注册 CAS，并在 checkout
   窗口内 set_absorbing。
3. **absorb 只对 checkout 窗口安全**：窗口外用户正常写文件必须走磁盘
   upper；否则崩溃会丢未提交改动。
4. **dashboard 夹具去掉了 2 个 gitlink**：submodule 语义还未在 CAS 视图
   建模；真实 dashboard 带 gitlink 时 lazy status 会报 ` T`。
5. **没有专门单测**：absorb 路径由 ignored 的 `cas_mount_bench` 覆盖；
   正式立卡时应补一个真实挂载的回归测试。

## 6. 复跑

```powershell
# native + B1
$env:WTFS_BENCH_REPO="C:/Users/notxx/AppData/Local/Temp/wtfs-dashboard-bench/repo"
$env:WTFS_BENCH_ITERS="10"
cargo test --release -p wtfs-daemon --test m0_benchmark -- --ignored --nocapture

# C lazy
$env:WTFS_BENCH_SKIP_HYDRATE_ASSERT="1"
cargo test --release -p wtfs-vfs --test cas_mount_bench -- --ignored --nocapture

# C absorb
$env:WTFS_CAS_ABSORB="1"
cargo test --release -p wtfs-vfs --test cas_mount_bench -- --ignored --nocapture
```

原始 10 轮日志：`%TEMP%\wtfs-dashboard-bench\results\`。

## 7. 三态缓存复测（2026-11-18）

在 `CasView` 里加了每文件三态索引：

- `FileState = Lower | Upper | Deleted`；
- 启动时从 manifest 文件 + upper 递归扫描 + markers.log replay 构建；
- `resolve`/`stat` 一次 `HashMap` 查询，替代 upper 探测 + whiteout + manifest 三次推导；
- 写/删/rename 增量更新；VFS 直写通过 `note_upper_mutation(rel)` 修正；
- `upper_neg` 只对 manifest 文件缓存负结果，目录/未知路径不缓存，避免外部重建目录 stale。

dashboard 791 文件 / 10 wt × 10 轮（中位）：

| 指标 | C lazy 旧 | C lazy + state | C absorb 旧 | C absorb + state |
|---|---:|---:|---:|---:|
| add 全程 | 263 ms | 275 ms | 1,597 ms | **1,108 ms** |
| setup | 210 ms | 214 ms | 50 ms | 46 ms |
| register（manifest 已加载，state build） | — | **0.5 ms** | — | **0.4 ms** |
| checkout | — | — | 1,501 ms | **1,019 ms** |
| status 冷 | 137 ms | 138 ms | 137 ms | 139 ms |
| status 热 | 134 ms | 144 ms | 116 ms | 115 ms |
| diff | 230 ms | **137 ms** | 222 ms | **122 ms** |
| commit | 407 ms | **300 ms** | 294 ms | **181 ms** |
| remove | 45 ms | 44 ms | 55 ms | 53 ms |
| 首次读 200 文件 | 982 ms | 1,003 ms | 859 ms | 834 ms |
| upper（10 wt） | 1 KB | 1 KB | 17.2 MB | 17.2 MB |

结论：

- **启动 state build 0.4–0.5 ms**，不慢；SQLite checkpoint 目前没有必要（manifest 装载 592 ms 是另一回事，state 快照省不掉它）。
- **status 没有变快**：10 轮中位仍 ~138/144 ms，和旧版持平。status 的剩余成本不在 `resolve`，而在 `list_stat`/目录枚举/WinFSP 每文件回调；状态 map 不是瓶颈。
- **diff/commit/checkout 明显变快**：diff 230→137（lazy）、222→122（absorb）；commit 407→300、294→181；absorb checkout 1,501→1,019 ms。这些路径会大量走 `resolve`/`stat`，三态 map 省掉了三次推导。
- 单轮 111 ms 是噪声，不要当结论。

下一步如果要继续压 status，应该 profile `list_stat` / `read_directory` / `get_file_info` 的回调占比，而不是继续加状态缓存。
