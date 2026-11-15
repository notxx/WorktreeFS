# M19/B190100：B1 与 C 两种实现对比（性能 + 优缺点 + 适用场景）

B1 = master 此前基线（commit `0922092`，M14 merge）：WinFSP per-worktree
overlay，模板 lower + upper + 标记日志。C = 新方案（`ff4216f`，M18 merge）：
CAS 虚拟视图，Git 对象库 + manifest + 按需 hydrate + upper。两者挂载点相同
（`<repo>/.tower/worktrees`）、夹具相同（git/git @923ecfe，4,874 文件），
保证可比。

## 对比口径与边界（必读）

* **驱动方式不同**：B1 走真实 release CLI（`wtfs template create` →
  `wtfs mount` → plain `git worktree add` → … → `git worktree remove
  --force` → `wtfs unmount`，wtfsd 守护进程常驻）；C 的 add/seed/注册在
  M18 **没有 CLI/daemon 接线**（显式流程，库 API 驱动），只能用 M18 的
  `cas_mount_bench` harness（进程内挂载）驱动。两者的 FS 路径与 git 操作
  口径一致，但驱动进程形态不同（外部 CLI + 守护进程 vs 进程内 harness）。
* **C 的 add 是显式流程**：`git worktree add --no-checkout` + `seed_
  stat_aligned` + `write_stat_config` + `register_cas_worktree` +
  `clear_skip_worktree`。M18 边界：Tower 的 plain `git worktree add`
  拦截/shim 自动注册不在本 mission。**下表 C 的 add 数字不得冒充 Tower
  自动流程**；B1 的 add 是 plain `git worktree add`（挂载点内
  auto_register）。
* remove 两者都用 `--force`（C 的稀疏 upper 需要；B1 同口径）。
* wt 数按腿不同：B1 腿由 `-Worktrees` 参数控制（默认 10）；C 腿是
  harness（cas_mount_bench）**固定 10 wt**，不受参数影响。
* 每轮 fresh 本地 clone 夹具，B1/C 交替，共 3 轮；轮间清理 store 与
  worktree。B1/C 各自独立 checkout + target dir 构建 release。
* 指标：add（B1 含 ready 轮询；C 含 seed，注册/清位未单列计时）、
  status 冷/热、diff --stat 干净树、commit -am 单文件、remove、磁盘
  （B1 模板+upper；C upper+标记+hydration 缓存）、首读 200 个 .c/.h、
  hydration 缓存大小；驱动进程峰值工作集（500ms 采样）。
* 时间与机器：2026-11-15，Windows，release 构建，scripts/
  bench-m19-b1-vs-c.ps1 可复跑（见文末）。

## 性能表（4,874 文件 / 10 worktree，3 轮交替，每轮 fresh clone）

主表：每 wt 中位（ms），三轮各自值 → 三轮中位。

| 指标 | B1（r1/r2/r3） | B1 中位 | C（r1/r2/r3） | C 中位 | C ÷ B1 |
| ---- | -------------- | ------- | ------------- | ------ | ------ |
| add① | 3,906 / 4,043 / 4,005 | **4,005** | 38.2 / 40.3 / 38.4 | **38.4** | 0.010 |
| ready（B1 watcher 轮询）/ seed（C read-tree+stat 对齐+配置） | 1,639 / 1,692 / 1,628 | **1,639** | 99.9 / 102.9 / 99.9 | **99.9** | — |
| add 全程② | 5,545 / 5,735 / 5,632 | **5,632** | 138.1 / 143.2 / 138.3 | **138.3** | 0.025 |
| status 冷 | 97.7 / 102.0 / 99.3 | **99.3** | 144.5 / 143.6 / 145.2 | **144.5** | 1.45 |
| status 热 | 96.3 / 98.9 / 99.2 | **98.9** | 143.5 / 140.5 / 143.9 | **143.5** | 1.45 |
| diff --stat 干净树 | 115.3 / 113.1 / 105.9 | **113.1** | 220.5 / 221.4 / 224.4 | **221.4** | 1.96 |
| commit -am 单文件 | 230.2 / 222.2 / 230.6 | **230.2** | 292.2 / 286.2 / 285.7 | **286.2** | 1.24 |
| remove --force | 3,200 / 3,185 / 3,195 | **3,195** | 34.0 / 32.9 / 34.6 | **34.0** | 0.011 |
| 首次读 200 文件（冷，合计） | 151.5 / 139.1 / 146.2（0.70-0.76 ms/文件） | **146.2** | 848.5 / 825.6 / 876.2（4.13-4.38 ms/文件） | **848.5** | 5.8 |

① B1 add = plain `git worktree add`（挂载点内，含模板 checkout 经 FS）；
  C add = 显式流程的 `worktree add --no-checkout` 一步。
② B1 全程 = add + ready；C 全程 = add + seed，**未含注册与清位**（harness
  未单列计时；M18 记录批量清 skip-worktree ~48 ms/4,874，注册为内存操作），
  即 C 全程实为 ~190 ms 量级。B1 的 ready 轮询含 ~100 ms 的 CLI 进程轮询
  粒度（上界估计）。

一次性成本（每轮重建 store 口径）：

| 一次性项 | B1 三轮（中位） | C 三轮 |
| -------- | --------------- | ------ |
| lower 构建 | template create：3,678 / 3,737 / 4,707 ms（**3,737**；物化 4,875 文件树 + index） | manifest 装载一次跨 wt 共享（M17 计 ~592 ms；本基准记录规模 4,874 文件 / 48.2 MB 树） |
| mount | 3,866 / 3,903 / 5,028 ms（**3,903**；spawn wtfsd + WinFSP 挂载） | harness 进程内挂载，未单列 |

磁盘与内存（10 wt 各提交 1 文件后）：

| 维度 | B1 | C |
| ---- | -- | -- |
| 每 wt upper（10 wt 合计） | 20 文件 / 0.9 KB | 20 文件 / 0.9 KB |
| 共享 lower | 模板树物化 4,875 文件 / **49.7 MB**（store 总计 50.5 MB） | Git 对象库零复制（复用仓库 .git）+ manifest 内存索引 |
| 读缓存 | 无独立缓存（NTFS 页缓存承担） | hydration 缓存 216 文件 / **3.6 MB**（status+首读后；读过才灌，oid 全局共享，上限≈树大小） |
| 标记日志 | 0 文件（bench 无删除） | 0 文件 |
| status 期间 hydrate | —（读直达模板） | 14 次，全部 `.gitignore` exclude；tracked 内容 0 |
| 驱动进程峰值 WS | wtfsd 37.8 / 38.0 / 38.0 MB | harness 35.3 / 34.8 / 35.1 MB（含 manifest/registry，非纯 FS 层口径） |

首读口径备注：B1 首读经 PowerShell `ReadAllBytes` 驱动（含 PS 循环开销），
C 首读为 harness 内 Rust `fs::read`（含 cat-file 子进程 + fsync）——驱动
差异不影响结论：C 冷首读被 hydrate 的 fsync 主导，比 B1 的 NTFS 直读慢
~6 倍/文件；命中缓存后两者同为 µs 级探测。

## 差距归因

* **C 的 add 是 O(1) 树复制 vs B1 的 O(files) 回调流量（4,005+1,639 ms
  vs 38+100 ms）**。B1 的 plain `git worktree add`（挂载点内）=
  auto_register（建 upper + 模板解析）+ git checkout 经 WinFSP 回调物化
  4,874 个文件（每文件 open/write/cleanup IPC）+ watcher 确认 index 后
  mark_ready——实测 add 4.0 s + ready 1.6 s，成本在"把树搬进可见卷"。
  C = `add --no-checkout`（只写 .git 指针 + 空 index，38 ms）+ seed
  （read-tree + gix index stat 改写，纯 index 重写、零 FS 文件流量，
  100 ms）+ 注册（内存表）+ 批量清 skip-worktree（~50 ms/4,874，M18 口
  径）——成本在"改写 index"，不经过 FS。**一个数量级的差距来自这一结构
  差异**，且 C 的 per-wt 增量成本里只有 seed 随文件数线性（index 重写）。
* **status 双方都不 hydrate 内容（145 vs 99 ms，同量级）；B1 靠模板
  stat、C 靠 manifest stat**。B1：模板 index 与 FS stat 对齐（M13），
  status 走 index-refresh 快路径，读 = 元数据探测。C：skip-worktree 清位
  后 git 只比 mtime/size（checkStat=minimal），manifest 固定 stat 与种子
  逐位对齐 → tracked 内容零读取（实测 10 wt × status 仅 14 次 hydrate，
  全部是 `.gitignore` exclude 分类读取，hydration 缓存按 oid 全局共享，
  其余 9 个 wt 全命中）。C 慢的 ~45 ms = 每目录 list_stat 枚举 + manifest
  查找 vs B1 模板树上的 NTFS stat。
* **首次读是 C 的结构性成本：cat-file 往返 + 恰好一次 fsync（4.2 vs
  0.73 ms/文件）**。B1 的首读是 NTFS 直读（模板树已物化）；C 首读每个
  文件 = `git cat-file` 子进程往返 + temp+fsync+rename 原子落缓存
  （fsync 主导）+ 读缓存。命中后 C 与 B1 同为 µs 级探测。这一差距只作用
  于"冷读集"，且缓存跨 worktree 全局共享——第二个 worktree 读同一 blob
  不再付费。
* **diff/commit 干净树零 hydrate（diff 1.96×、commit 1.24×）；成本在
  git 自身的工作量**。C 干净树 `diff --stat` 与 status 同路径（manifest
  stat，零对象读），慢近一倍的部分是每目录 list_stat 枚举 + git 自身
  工作量（B1 模板树由 NTFS 原生枚举）。commit 的计时区间只覆盖
  `git commit -am` 本身：README 编辑的 copy-up（cat-file 取 blob + 复制
  upper + 终裁 fsync）发生在计时前的写文件步骤，refresh_head 重载
  manifest 在计时后调用，都不计入 286 ms；计时内主要是 commit 的全树
  stat 扫描（等价一次 status 的 per-op VFS IPC，经挂载视图逐项探测）+
  对象与 index 写直落挂载外的真实 .git。与 B1（230 ms，同一 git 流程但
  stat 走 NTFS）的差距主要是 stat 单价差。
* **remove 的 IPC 地板：B1 删可见树，C 删稀疏 upper（3,195 vs 34 ms，
  ~94×）**。B1 `remove --force` 要经 FS 逐项删掉 4,874 文件的可见树
  （set_delete/cleanup/rmdir 回调 = IPC 地板；M14 从 6,427 ms 优化到
  3,306 ms 的正是这一面，本轮 3.2 s 与该口径一致）。C 的 upper 只有
  .git 指针 + 提交残留（干净改动回收后近零），remove 只删这 20 项 +
  回收挂载外的真实目录。这是 C 对"多而短命 worktree"场景的决定性优势。
  注意口径：C 必须 `--force`（稀疏 upper 使 git 干净检查必报删除），
  B1 同口径对齐。

## 优缺点对比

| 维度 | B1（overlay + 模板） | C（CAS + manifest） |
| ---- | -------------------- | ------------------- |
| 实现复杂度 | 组件多：daemon（状态机/watcher/管道）、模板 store（sqlite + 树 checkout + index 复制）、overlay（auto-register、M8/M9 staged 写事务、M12 标记日志、M14 负缓存） | 更薄：独立 `wtfs-cas` crate（manifest/hydration/seed，不依赖 wtfs-core）+ VFS 后端 enum；无状态机（注册即 ready）、无 staged 流；但引入 gix index 改写等 git 内部知识 |
| 故障模式 | 模板缺失/过期 → 回落全量 copy（正确但慢，且需 template update 维护）；标记日志重放；upper/元数据残留 | manifest 与 HEAD 漂移窗口（commit 后 refresh 前；分支切换完整语义 M18 未做）；seed 与 FS stat 失配 → status 假脏（checkStat=minimal + E2E 锁住）；hydration 缓存损毁可弃——真相在 Git 对象库，删缓存不丢数据 |
| Git 兼容性 | 模板 index 原生，git 全操作零特判；普通 remove 可用；分支切换/模板 refresh 原生支持 | 依赖 skip-worktree + checkStat=minimal + stat 种子；status/diff/commit 正常，但稀疏 upper → remove 必须 `--force`；分支切换 refresh（路径复用冲突）未实现；依赖 git 对 checkStat/trustctime 的行为 |
| 缓存/GC | 模板只读共享（天然零拷贝）；读无缓存、直达 NTFS；模板按 max-age-days gc | hydration 缓存按 oid 全局分片共享、可随时删重建；gc(max_bytes) 按 mtime 逐出（显式调用，未接 daemon）；manifest 一次装载跨 wt 共享 |
| 并发 | auto_register 名字幂等；单 daemon watcher；overlay 锁收敛写 | per-mount Mutex 收敛写回调；cat-file 并发灌缓存（temp+rename 原子）；同 oid 并发安全 |
| 磁盘 | 模板一次性 ~50 MB（整树）+ 每 wt upper 改动面 | 每 wt upper ≈ .git 指针 + 改动（同内容回收后近零）+ 缓存（读过才灌）+ 共享对象库零复制；add 后磁盘 ≈ 一个 index |
| 冷热读 | 首读 NTFS µs 级（树已物化） | 首读 ~4 ms/文件（cat-file+fsync，冷）；命中后 µs 级 |
| 可观测性 | daemon status 状态机（initializing/ready）+ M13/M14 回调级 profiling（回调计数/NTFS 计数三分） | 缓存统计 hits/misses/files/bytes（可机器断言"零 hydrate"）；status 报 `ready (cas)`；无生命周期状态可观察 |
| 运维 | CLI/daemon/shim 全接线：mount/unmount/status/template/gc 即开即用；wtfsd 常驻 | 无 daemon 依赖（库挂载即可用），但 add/seed/注册/remove 未接 CLI/shim——目前只能编程驱动；缓存 gc 未接线自动化 |

## 适用场景建议

一句话结论：**读热、长寿、要即插即用选 B1；仓库大、worktree 多而短命、
读稀疏、能改造流程选 C**。实测差距集中在生命周期操作：add 全程
5,632 → ~190 ms（~30×）、remove 3,195 → 34 ms（~94×）、10 wt 磁盘
50.5 → ~3.6 MB（~14×）；C 付出的代价集中在读路径：冷首读 ~6×/文件、
status/diff 慢 1.45×/1.96×——都是读的工作集换生命周期速度。

* **选 B1**：
  * 工作流要求"今天即可用"的完整接线（plain `git worktree add` 透明、
    普通 remove、分支切换、模板 update/gc 已在 CLI/daemon 里）。
  * 首读延迟敏感：IDE 全量索引、构建系统扫全树、测试跑套件——模板物化树
    首读零成本，C 的按需 hydrate 会把成本挪到首次访问。
  * 分支切换频繁 / 模板演进正常（B1 的 refresh 语义完整，C 的跨分支
    refresh 未做完）。
  * wt 少而长寿（模板成本被长生命周期摊薄，upper 改动面小）。
* **选 C**：
  * 大仓库 × 多 worktree / 短生命周期（CI 并发检视、临时审查、批量重建）：
    add/remove 快 1-2 个数量级、add 后磁盘 ≈ 一个 index、remove 只删
    稀疏 upper。
  * 读稀疏：只碰少部分文件的审计/检索工作流，按需 hydrate 总成本低于
    整树物化；hydration 缓存跨 wt 共享，重复读零成本。
  * 能接受/消除首读延迟（预热、预取）且能改造自动化流程（shim 接线后
    C 的适用面显著扩大）。

## C 的后续改进（按收益排序）

1. **shim 自动注册**：把显式流程（add --no-checkout + seed + 注册 + 清位）
   藏进 plain `git worktree add`（wtfs-git-shim 拦截或 daemon watcher），
   消除与 B1 的接线差距——这是 C 目前最大的可用性缺口。
2. **缓存预取**：挂载后台按 manifest 预 hydrate 热点子树（如构建依赖、
   README/顶层文件），把首读延迟从用户路径挪到挂载瞬间。
3. **整 workspace 挂载**：一次挂载呈现全部 wt（现有 VfsRegistry 已支持
   混合注册），免去逐 wt 手工注册的流程成本。
4. **独立 CAS 服务**：hydration 缓存与对象访问抽出为跨仓库服务（同一台
   机器多仓库共享 oid 缓存；进一步可对接远端 CAS）。
5. **remove 先注销再 remove**（免 `--force`）：daemon/shim 在
   `worktree remove` 前先 unregister 视图，让 git 干净检查不再看到稀疏
   upper 的"删除"，恢复普通 remove 语义。
6. **分支切换 refresh 完整语义 + upper/缓存清理接进 `wtfs gc` 计划**
   （M18 已留扩展点）。

## 复跑

```powershell
# 前置：B1 checkout（%TEMP%\wtfs-b19-b1 @ 0922092）与 C（本仓库 @ ff4216f）
# 各自 cargo build --release / cargo test --release -p wtfs-vfs
# --test cas_mount_bench --no-run；或直接 -Build 让脚本代劳。
powershell -ExecutionPolicy Bypass -File scripts\bench-m19-b1-vs-c.ps1 `
    -FixtureRepo C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo `
    -Rounds 3 -OutFile %TEMP%\wtfs-b19-results.md
```

脚本行为：每轮 fresh 本地 clone → B1 CLI 全流程（模板一次性计时）→ C
harness（cargo test，WTFS_BENCH_REPO 指向轮 clone）→ 逐轮落原始结果 +
轮次汇总表。残留 wtfsd 按轮管道名（`\\.\pipe\wtfs-b19-r<N>`）开局清理；
C 腿经 `cmd /c` 超时兜底驱动（`-CargoTimeoutSec`，默认 1800 s），跑完
默认清理 `%TEMP%\wtfs-b19-r<N>` 轮次目录（`-KeepRounds` 保留原始日志）。
注意：脚本文件必须保存为 UTF-8 with BOM（PS5.1 无 BOM 会按 ANSI 误读
中文注释与全角字符串）。
