# M19/B210100 10×10 三方复测原始输出（native / B1 / C，2026-11-16）

> 本文件为 scripts/bench-m19-b1-vs-c.ps1 -Rounds 10 -Worktrees 10 的原始输出
> （逐轮 native/B1 per-wt 明细、C harness 全表、全程汇总与轮次汇总），
> 分析与口径见 docs/m19-b1-vs-c.md 主表。仅格式归一（LF、去 BOM），数据未改动。


- fixture: C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo (fresh local clone per round)
- B1: commit 0922092 @ C:\Users\notxx\AppData\Local\Temp\wtfs-b19-b1 (release CLI: wtfs + wtfsd)
- C: ff4216f @ C:\Users\notxx\Documents\workspace\WorktreeFS\.tower\worktrees\wt-21 (cargo test cas_mount_bench, in-process harness)
- rounds: 10; worktrees: native = 10 / B1 = 10（-Worktrees 参数）/ C = 10（harness 固定）; first-read files: 200

## Round 1 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 3,220.6 ms；各 wt: 2992.2, 3222.1, 3219.1, 3921.5, 3084.6, 4036.4, 3763, 3840, 3207.7, 2922.1
- status 冷/热（中位）: 214.9 / 85.6 ms
- diff --stat（中位）: 56.5 ms
- commit（add -A + commit 两进程，中位）: 216.2 ms（add -A 单列中位: 104.4 ms）
- remove --force（中位）: 1,338.3 ms（首个 1,315.9 ms）
- 首读 200 个 .c/.h: 77.2 ms 合计（0.386 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 1 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 4,790.9 ms；mount: 6,360.0 ms
- add（plain worktree add，10 wt 中位）: 4,137.3 ms；各 wt: 5635.3, 4674.5, 4658.6, 3994.6, 4231.8, 4153.3, 4081.1, 4121.3, 4072.5, 4056.6
- ready（轮询至 ready，10 wt 中位）: 1,960.8 ms；add+ready 中位: 6,098.2 ms
- status 冷/热（中位）: 102.2 / 108.9 ms
- diff --stat（中位）: 126.6 ms
- commit -am 单文件（中位）: 229.3 ms
- remove --force（中位）: 3,268.4 ms（首个 3,516.4 ms）
- 首读 200 个 .c/.h: 163.9 ms 合计（0.820 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 37.7 MB（wtfsd pid 36380）

## Round 1 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 37.5 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 101.3 ms |
| status 冷（挂载视图，每 wt 中位） | 154.4 ms |
| status 热（每 wt 中位） | 149.1 ms |
| diff --stat 干净树（每 wt 中位） | 236.6 ms |
| commit -am 单文件（每 wt 中位） | 294.4 ms |
| remove（worktree remove + 注销，每 wt 中位） | 36.0 ms |
| 首次读 200 文件（冷，合计） | 854.7 ms（4.274 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 35 MB（cas_mount_bench-2f98276a1311b80d pid 6432；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 2 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,725.9 ms；各 wt: 2975.3, 2713.2, 2708.2, 2738.6, 2745.8, 2697.7, 2847.6, 2709.3, 2702.3, 2778.7
- status 冷/热（中位）: 146.2 / 99.8 ms
- diff --stat（中位）: 109.9 ms
- commit（add -A + commit 两进程，中位）: 267.5 ms（add -A 单列中位: 132.1 ms）
- remove --force（中位）: 1,281.3 ms（首个 1,263.2 ms）
- 首读 200 个 .c/.h: 36.9 ms 合计（0.185 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 2 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,667.4 ms；mount: 4,625.4 ms
- add（plain worktree add，10 wt 中位）: 4,078.8 ms；各 wt: 4792.9, 4445.4, 4006.1, 4020.1, 4067.5, 4071.6, 4086, 4065.7, 4461.7, 4707.5
- ready（轮询至 ready，10 wt 中位）: 1,802.8 ms；add+ready 中位: 5,881.5 ms
- status 冷/热（中位）: 103.3 / 103.3 ms
- diff --stat（中位）: 125.1 ms
- commit -am 单文件（中位）: 229.5 ms
- remove --force（中位）: 3,659.9 ms（首个 3,930.8 ms）
- 首读 200 个 .c/.h: 140.7 ms 合计（0.704 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 37.9 MB（wtfsd pid 20464）

## Round 2 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 43.1 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 115.2 ms |
| status 冷（挂载视图，每 wt 中位） | 170.3 ms |
| status 热（每 wt 中位） | 159.3 ms |
| diff --stat 干净树（每 wt 中位） | 234.9 ms |
| commit -am 单文件（每 wt 中位） | 296.0 ms |
| remove（worktree remove + 注销，每 wt 中位） | 33.8 ms |
| 首次读 200 文件（冷，合计） | 838.7 ms（4.194 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 35.4 MB（cas_mount_bench-2f98276a1311b80d pid 40136；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 3 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,764.3 ms；各 wt: 2631.5, 2808.9, 2648.7, 2699.3, 3635.6, 2676, 2766.6, 2762.1, 2875.4, 2839
- status 冷/热（中位）: 175.0 / 165.7 ms
- diff --stat（中位）: 119.4 ms
- commit（add -A + commit 两进程，中位）: 284.8 ms（add -A 单列中位: 145.2 ms）
- remove --force（中位）: 1,327.2 ms（首个 1,246.0 ms）
- 首读 200 个 .c/.h: 38.7 ms 合计（0.194 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 3 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,726.6 ms；mount: 5,251.3 ms
- add（plain worktree add，10 wt 中位）: 4,433.4 ms；各 wt: 4646.9, 4759.3, 4382.5, 4612.3, 4454, 4438.7, 4331.1, 4158.8, 4033.2, 4428.1
- ready（轮询至 ready，10 wt 中位）: 2,052.3 ms；add+ready 中位: 6,485.7 ms
- status 冷/热（中位）: 103.4 / 103.6 ms
- diff --stat（中位）: 117.4 ms
- commit -am 单文件（中位）: 229.3 ms
- remove --force（中位）: 3,270.7 ms（首个 3,284.2 ms）
- 首读 200 个 .c/.h: 141.7 ms 合计（0.709 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 37.8 MB（wtfsd pid 36076）

## Round 3 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 38.0 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 104.7 ms |
| status 冷（挂载视图，每 wt 中位） | 147.9 ms |
| status 热（每 wt 中位） | 142.6 ms |
| diff --stat 干净树（每 wt 中位） | 217.7 ms |
| commit -am 单文件（每 wt 中位） | 290.7 ms |
| remove（worktree remove + 注销，每 wt 中位） | 34.6 ms |
| 首次读 200 文件（冷，合计） | 819.5 ms（4.098 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 35.4 MB（cas_mount_bench-2f98276a1311b80d pid 35736；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 4 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,797.7 ms；各 wt: 2692.1, 2661.5, 2783.6, 2789.4, 2878.4, 2694.5, 2867.7, 2805.9, 3017.1, 2845.2
- status 冷/热（中位）: 129.5 / 131.6 ms
- diff --stat（中位）: 165.5 ms
- commit（add -A + commit 两进程，中位）: 320.1 ms（add -A 单列中位: 170.5 ms）
- remove --force（中位）: 1,302.0 ms（首个 1,301.7 ms）
- 首读 200 个 .c/.h: 37.1 ms 合计（0.185 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 4 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,598.6 ms；mount: 5,125.9 ms
- add（plain worktree add，10 wt 中位）: 4,146.0 ms；各 wt: 4630.7, 4603.9, 4281.9, 4411.4, 4097.7, 4013.6, 3959.7, 3920.3, 4194.4, 3981.6
- ready（轮询至 ready，10 wt 中位）: 1,814.4 ms；add+ready 中位: 5,960.4 ms
- status 冷/热（中位）: 99.9 / 99.8 ms
- diff --stat（中位）: 119.7 ms
- commit -am 单文件（中位）: 224.5 ms
- remove --force（中位）: 3,222.2 ms（首个 3,177.5 ms）
- 首读 200 个 .c/.h: 140.5 ms 合计（0.703 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 38 MB（wtfsd pid 17808）

## Round 4 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 38.1 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 100.9 ms |
| status 冷（挂载视图，每 wt 中位） | 146.6 ms |
| status 热（每 wt 中位） | 145.7 ms |
| diff --stat 干净树（每 wt 中位） | 230.5 ms |
| commit -am 单文件（每 wt 中位） | 298.9 ms |
| remove（worktree remove + 注销，每 wt 中位） | 33.7 ms |
| 首次读 200 文件（冷，合计） | 852.2 ms（4.261 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 37 MB（cas_mount_bench-2f98276a1311b80d pid 27864；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 5 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,766.8 ms；各 wt: 2679.2, 2669.4, 2790.5, 2743, 2988.1, 2680, 2723.8, 2833, 2808.8, 2837.3
- status 冷/热（中位）: 197.5 / 151.6 ms
- diff --stat（中位）: 70.9 ms
- commit（add -A + commit 两进程，中位）: 220.5 ms（add -A 单列中位: 108.2 ms）
- remove --force（中位）: 1,289.5 ms（首个 1,258.3 ms）
- 首读 200 个 .c/.h: 53.1 ms 合计（0.266 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 5 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,591.4 ms；mount: 4,967.6 ms
- add（plain worktree add，10 wt 中位）: 4,103.0 ms；各 wt: 4520.7, 4508, 4044.8, 4103.2, 4357.1, 4084.2, 3980.7, 4191.6, 4087.3, 4102.8
- ready（轮询至 ready，10 wt 中位）: 1,817.5 ms；add+ready 中位: 5,920.5 ms
- status 冷/热（中位）: 100.3 / 102.2 ms
- diff --stat（中位）: 122.2 ms
- commit -am 单文件（中位）: 235.4 ms
- remove --force（中位）: 3,232.3 ms（首个 3,232.0 ms）
- 首读 200 个 .c/.h: 149.2 ms 合计（0.746 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 37.8 MB（wtfsd pid 1740）

## Round 5 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 38.9 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 100.4 ms |
| status 冷（挂载视图，每 wt 中位） | 141.5 ms |
| status 热（每 wt 中位） | 145.1 ms |
| diff --stat 干净树（每 wt 中位） | 230.6 ms |
| commit -am 单文件（每 wt 中位） | 291.9 ms |
| remove（worktree remove + 注销，每 wt 中位） | 33.5 ms |
| 首次读 200 文件（冷，合计） | 848.7 ms（4.243 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 35 MB（cas_mount_bench-2f98276a1311b80d pid 37244；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 6 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,713.8 ms；各 wt: 2678, 2663.7, 2696.2, 2647.5, 2717.4, 2803.6, 2710.3, 2731.1, 2803, 2776
- status 冷/热（中位）: 153.2 / 128.3 ms
- diff --stat（中位）: 90.2 ms
- commit（add -A + commit 两进程，中位）: 225.5 ms（add -A 单列中位: 114.5 ms）
- remove --force（中位）: 1,279.0 ms（首个 1,239.5 ms）
- 首读 200 个 .c/.h: 49.8 ms 合计（0.249 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 6 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,587.0 ms；mount: 4,836.9 ms
- add（plain worktree add，10 wt 中位）: 4,115.2 ms；各 wt: 4782.8, 4442.7, 4231.4, 3994.3, 4172, 4082.7, 4087.9, 3991.5, 4083.4, 4142.4
- ready（轮询至 ready，10 wt 中位）: 1,843.6 ms；add+ready 中位: 5,958.8 ms
- status 冷/热（中位）: 99.8 / 101.8 ms
- diff --stat（中位）: 120.0 ms
- commit -am 单文件（中位）: 221.3 ms
- remove --force（中位）: 3,235.0 ms（首个 3,251.6 ms）
- 首读 200 个 .c/.h: 141.4 ms 合计（0.707 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 38.2 MB（wtfsd pid 26992）

## Round 6 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 38.7 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 101.0 ms |
| status 冷（挂载视图，每 wt 中位） | 144.1 ms |
| status 热（每 wt 中位） | 145.5 ms |
| diff --stat 干净树（每 wt 中位） | 225.7 ms |
| commit -am 单文件（每 wt 中位） | 273.5 ms |
| remove（worktree remove + 注销，每 wt 中位） | 33.3 ms |
| 首次读 200 文件（冷，合计） | 837.8 ms（4.189 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 35.2 MB（cas_mount_bench-2f98276a1311b80d pid 39392；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 7 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,726.8 ms；各 wt: 2727.9, 2675.2, 2727.2, 2681.4, 2674.8, 2743.8, 2726.4, 2703.9, 2910.6, 2814
- status 冷/热（中位）: 166.2 / 127.2 ms
- diff --stat（中位）: 127.8 ms
- commit（add -A + commit 两进程，中位）: 269.5 ms（add -A 单列中位: 127.8 ms）
- remove --force（中位）: 1,297.3 ms（首个 1,240.2 ms）
- 首读 200 个 .c/.h: 42.8 ms 合计（0.214 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 7 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,692.8 ms；mount: 5,065.7 ms
- add（plain worktree add，10 wt 中位）: 4,178.6 ms；各 wt: 4721.8, 4533.4, 4046.5, 4123, 4228.6, 4128.7, 4279.5, 4046.1, 4101.4, 6157.5
- ready（轮询至 ready，10 wt 中位）: 1,836.1 ms；add+ready 中位: 6,014.7 ms
- status 冷/热（中位）: 98.6 / 99.1 ms
- diff --stat（中位）: 122.5 ms
- commit -am 单文件（中位）: 236.7 ms
- remove --force（中位）: 3,245.9 ms（首个 3,236.9 ms）
- 首读 200 个 .c/.h: 140.8 ms 合计（0.704 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 38.5 MB（wtfsd pid 30276）

## Round 7 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 38.5 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 99.9 ms |
| status 冷（挂载视图，每 wt 中位） | 144.4 ms |
| status 热（每 wt 中位） | 142.9 ms |
| diff --stat 干净树（每 wt 中位） | 232.3 ms |
| commit -am 单文件（每 wt 中位） | 290.3 ms |
| remove（worktree remove + 注销，每 wt 中位） | 33.8 ms |
| 首次读 200 文件（冷，合计） | 825.0 ms（4.125 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 34.1 MB（cas_mount_bench-2f98276a1311b80d pid 40804；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 8 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,735.6 ms；各 wt: 3007.3, 2677.2, 2734.8, 2680.9, 2708.5, 2706.7, 2736.4, 2817, 2795.1, 2739.7
- status 冷/热（中位）: 158.8 / 138.4 ms
- diff --stat（中位）: 158.6 ms
- commit（add -A + commit 两进程，中位）: 293.2 ms（add -A 单列中位: 157.2 ms）
- remove --force（中位）: 1,287.3 ms（首个 1,222.4 ms）
- 首读 200 个 .c/.h: 57.3 ms 合计（0.286 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 8 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,664.9 ms；mount: 5,129.6 ms
- add（plain worktree add，10 wt 中位）: 4,169.3 ms；各 wt: 4524.2, 4512.9, 4062.1, 4220.1, 4256.2, 3958.3, 4123.1, 4215.5, 4120.5, 4109.9
- ready（轮询至 ready，10 wt 中位）: 1,897.5 ms；add+ready 中位: 6,066.8 ms
- status 冷/热（中位）: 99.1 / 98.5 ms
- diff --stat（中位）: 118.1 ms
- commit -am 单文件（中位）: 225.3 ms
- remove --force（中位）: 3,244.6 ms（首个 3,259.5 ms）
- 首读 200 个 .c/.h: 138.7 ms 合计（0.694 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 38.4 MB（wtfsd pid 18888）

## Round 8 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 40.3 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 101.0 ms |
| status 冷（挂载视图，每 wt 中位） | 143.8 ms |
| status 热（每 wt 中位） | 143.1 ms |
| diff --stat 干净树（每 wt 中位） | 231.0 ms |
| commit -am 单文件（每 wt 中位） | 280.9 ms |
| remove（worktree remove + 注销，每 wt 中位） | 34.3 ms |
| 首次读 200 文件（冷，合计） | 830.3 ms（4.151 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 35 MB（cas_mount_bench-2f98276a1311b80d pid 18376；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 9 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,708.5 ms；各 wt: 2709.2, 2679.6, 2678.3, 2661.6, 2707.8, 2733.7, 2753, 2735.5, 2786, 2686
- status 冷/热（中位）: 189.0 / 165.1 ms
- diff --stat（中位）: 152.6 ms
- commit（add -A + commit 两进程，中位）: 278.6 ms（add -A 单列中位: 146.0 ms）
- remove --force（中位）: 1,301.2 ms（首个 1,291.4 ms）
- 首读 200 个 .c/.h: 37.4 ms 合计（0.187 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 9 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,711.5 ms；mount: 5,234.8 ms
- add（plain worktree add，10 wt 中位）: 4,221.7 ms；各 wt: 4488.1, 4623.7, 4098.7, 4143.1, 4300.3, 4088.8, 5516.4, 4045.7, 4355.2, 4026.9
- ready（轮询至 ready，10 wt 中位）: 1,955.8 ms；add+ready 中位: 6,177.4 ms
- status 冷/热（中位）: 96.8 / 99.7 ms
- diff --stat（中位）: 119.3 ms
- commit -am 单文件（中位）: 221.1 ms
- remove --force（中位）: 3,230.5 ms（首个 3,252.2 ms）
- 首读 200 个 .c/.h: 143.9 ms 合计（0.719 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 38 MB（wtfsd pid 23704）

## Round 9 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 37.2 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 101.0 ms |
| status 冷（挂载视图，每 wt 中位） | 147.1 ms |
| status 热（每 wt 中位） | 143.4 ms |
| diff --stat 干净树（每 wt 中位） | 218.9 ms |
| commit -am 单文件（每 wt 中位） | 293.9 ms |
| remove（worktree remove + 注销，每 wt 中位） | 34.9 ms |
| 首次读 200 文件（冷，合计） | 830.1 ms（4.150 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 34.9 MB（cas_mount_bench-2f98276a1311b80d pid 29344；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## Round 10 — native（plain NTFS 挂载外，10 wt）

- add（plain worktree add，10 wt 中位）: 2,705.7 ms；各 wt: 2655.5, 2667.8, 2666.4, 2689.9, 2784.7, 2695.4, 2727.4, 2790.2, 2715.9, 2721.9
- status 冷/热（中位）: 141.6 / 133.9 ms
- diff --stat（中位）: 121.4 ms
- commit（add -A + commit 两进程，中位）: 263.5 ms（add -A 单列中位: 129.6 ms）
- remove --force（中位）: 1,268.3 ms（首个 1,242.6 ms）
- 首读 200 个 .c/.h: 43.5 ms 合计（0.218 ms/文件，3,781 KB）
- 磁盘（10 wt 合计）: 48750 文件 / 492.6 MB（每 wt 中位 49.26 MB，全量树）

## Round 10 — B1（CLI @ 0922092，10 wt）

- template create（一次性）: 3,499.7 ms；mount: 5,088.6 ms
- add（plain worktree add，10 wt 中位）: 4,197.9 ms；各 wt: 4501.2, 4567.4, 4098.3, 4184, 4398, 4139.6, 4071.2, 4211.7, 4366.3, 4165.4
- ready（轮询至 ready，10 wt 中位）: 1,866.9 ms；add+ready 中位: 6,064.8 ms
- status 冷/热（中位）: 100.6 / 101.2 ms
- diff --stat（中位）: 114.8 ms
- commit -am 单文件（中位）: 237.1 ms
- remove --force（中位）: 3,249.2 ms（首个 3,277.5 ms）
- 首读 200 个 .c/.h: 147.6 ms 合计（0.738 ms/文件，3,781 KB）
- upper 磁盘（10 wt 合计）: 20 文件 / 0.9 KB；templates: 4875 文件 / 49.7 MB；store 总计: 50.5 MB
- 驱动进程峰值 WS: 37.9 MB（wtfsd pid 30748）

## Round 10 — C（harness @ ff4216f，固定 10 wt）

## M18 CAS 挂载基准（release，4874 文件 / 10 worktree）
| 指标 | 值 |
| ---- | -- |
| add（worktree add --no-checkout，每 wt 中位） | 38.3 ms |
| 种子（read-tree + stat 对齐 + 配置，每 wt 中位） | 102.2 ms |
| status 冷（挂载视图，每 wt 中位） | 152.5 ms |
| status 热（每 wt 中位） | 151.5 ms |
| diff --stat 干净树（每 wt 中位） | 220.2 ms |
| commit -am 单文件（每 wt 中位） | 308.9 ms |
| remove（worktree remove + 注销，每 wt 中位） | 97.1 ms |
| 首次读 200 文件（冷，合计） | 832.6 ms（4.163 ms/文件，3649.7 KB） |
| status 期间 hydrate | 14 次（全部 .gitignore exclude，oid 缓存全局共享一次；tracked 内容 0） |
| hydration 缓存（首读后） | 216 文件 / 3658.8 KB |
| upper 磁盘（10 wt 合计，含 .git 指针 + 提交残留） | 20 文件 / 0.9 KB |
| 标记日志磁盘 | 0 文件 / 0.0 KB |
| manifest 装载（一次，跨 wt 共享） | 4874 文件 / 48.2 MB 树 |
- 驱动进程峰值 WS: 36.9 MB（cas_mount_bench-2f98276a1311b80d pid 22220；进程内 harness，含 manifest/registry 内存，非纯 FS 层）

## 全程汇总（median / p95 / min / max / 样本标准差，ms）

样本口径：native/B1 = per-wt 原始值（10 轮 × 10 wt = 100 样本/指标）；C = 每轮 harness 表值（10 wt 中位 → 10 样本/指标）；首读合计为每轮 1 值（10 样本）。

| 腿 | 指标 | n | median | p95 | min | max | std |
| -- | ---- | - | ------ | --- | --- | --- | --- |
| native | add | 100 | 2,735.2 | 3,242.8 | 2,631.5 | 4,036.4 | 264.32 |
| native | status 冷 | 100 | 168.1 | 265.9 | 57.1 | 283.2 | 56.10 |
| native | status 热 | 100 | 127.9 | 204.0 | 47.9 | 223.4 | 52.32 |
| native | diff --stat | 100 | 125.1 | 253.3 | 39.2 | 279.5 | 79.20 |
| native | commit | 100 | 275.1 | 346.9 | 166.1 | 371.6 | 61.75 |
| native | remove --force | 100 | 1,292.3 | 1,397.9 | 1,221.4 | 2,147.9 | 98.46 |
| native | 首读合计 | 10 | 43.2 | 68.3 | 36.9 | 77.2 | 12.71 |
| B1 | add | 100 | 4,168.7 | 4,760.5 | 3,920.3 | 6,157.5 | 348.35 |
| B1 | ready（B1）/ seed（C） | 100 | 1,867.9 | 2,357.6 | 1,679.1 | 3,331.3 | 238.58 |
| B1 | status 冷 | 100 | 100.5 | 113.5 | 93.8 | 140.5 | 7.09 |
| B1 | status 热 | 100 | 101.3 | 118.7 | 93.3 | 183.7 | 12.01 |
| B1 | diff --stat | 100 | 120.5 | 142.6 | 95.9 | 183.9 | 12.61 |
| B1 | commit | 100 | 227.3 | 268.4 | 183.0 | 347.4 | 25.32 |
| B1 | remove --force | 100 | 3,250.2 | 3,672.6 | 3,174.4 | 5,129.7 | 247.07 |
| B1 | 首读合计 | 10 | 141.6 | 157.3 | 138.7 | 163.9 | 7.48 |
| C | add | 10 | 38.4 | 41.8 | 37.2 | 43.1 | 1.71 |
| C | ready（B1）/ seed（C） | 10 | 101.0 | 110.5 | 99.9 | 115.2 | 4.56 |
| C | status 冷 | 10 | 146.9 | 163.1 | 141.5 | 170.3 | 8.39 |
| C | status 热 | 10 | 145.3 | 155.8 | 142.6 | 159.3 | 5.24 |
| C | diff --stat | 10 | 230.6 | 235.8 | 217.7 | 236.6 | 6.81 |
| C | commit | 10 | 292.9 | 304.4 | 273.5 | 308.9 | 9.60 |
| C | remove --force | 10 | 34.1 | 69.6 | 33.3 | 97.1 | 19.90 |
| C | 首读合计 | 10 | 835.2 | 853.6 | 819.5 | 854.7 | 11.77 |

## 轮次汇总（每腿各自 wt 数的中位数，ms；native = 10 wt / B1 = 10 wt / C = 10 wt harness 固定）

| 轮 | 实现 | add | ready/seed | status 冷 | status 热 | diff | commit | remove | 首读合计 | 驱动峰值 WS MB |
| -- | ---- | --- | ---------- | --------- | --------- | ---- | ------ | ------ | -------- | -------------- |
| 1 | native | 3,220.6 | — | 214.9 | 85.6 | 56.5 | 216.2 | 1,338.3 | 77.2 | — |
| 1 | B1 | 4,137.3 | 1,960.8 | 102.2 | 108.9 | 126.6 | 229.3 | 3,268.4 | 163.9 | 37.7 |
| 1 | C | 37.5 | 101.3 | 154.4 | 149.1 | 236.6 | 294.4 | 36.0 | 854.7 | 35 |
| 2 | native | 2,725.9 | — | 146.2 | 99.8 | 109.9 | 267.5 | 1,281.3 | 36.9 | — |
| 2 | B1 | 4,078.8 | 1,802.8 | 103.3 | 103.3 | 125.1 | 229.5 | 3,659.9 | 140.7 | 37.9 |
| 2 | C | 43.1 | 115.2 | 170.3 | 159.3 | 234.9 | 296.0 | 33.8 | 838.7 | 35.4 |
| 3 | native | 2,764.3 | — | 175.0 | 165.7 | 119.4 | 284.8 | 1,327.2 | 38.7 | — |
| 3 | B1 | 4,433.4 | 2,052.3 | 103.4 | 103.6 | 117.4 | 229.3 | 3,270.7 | 141.7 | 37.8 |
| 3 | C | 38.0 | 104.7 | 147.9 | 142.6 | 217.7 | 290.7 | 34.6 | 819.5 | 35.4 |
| 4 | native | 2,797.7 | — | 129.5 | 131.6 | 165.5 | 320.1 | 1,302.0 | 37.1 | — |
| 4 | B1 | 4,146.0 | 1,814.4 | 99.9 | 99.8 | 119.7 | 224.5 | 3,222.2 | 140.5 | 38 |
| 4 | C | 38.1 | 100.9 | 146.6 | 145.7 | 230.5 | 298.9 | 33.7 | 852.2 | 37 |
| 5 | native | 2,766.8 | — | 197.5 | 151.6 | 70.9 | 220.5 | 1,289.5 | 53.1 | — |
| 5 | B1 | 4,103.0 | 1,817.5 | 100.3 | 102.2 | 122.2 | 235.4 | 3,232.3 | 149.2 | 37.8 |
| 5 | C | 38.9 | 100.4 | 141.5 | 145.1 | 230.6 | 291.9 | 33.5 | 848.7 | 35 |
| 6 | native | 2,713.8 | — | 153.2 | 128.3 | 90.2 | 225.5 | 1,279.0 | 49.8 | — |
| 6 | B1 | 4,115.2 | 1,843.6 | 99.8 | 101.8 | 120.0 | 221.3 | 3,235.0 | 141.4 | 38.2 |
| 6 | C | 38.7 | 101.0 | 144.1 | 145.5 | 225.7 | 273.5 | 33.3 | 837.8 | 35.2 |
| 7 | native | 2,726.8 | — | 166.2 | 127.2 | 127.8 | 269.5 | 1,297.3 | 42.8 | — |
| 7 | B1 | 4,178.6 | 1,836.1 | 98.6 | 99.1 | 122.5 | 236.7 | 3,245.9 | 140.8 | 38.5 |
| 7 | C | 38.5 | 99.9 | 144.4 | 142.9 | 232.3 | 290.3 | 33.8 | 825.0 | 34.1 |
| 8 | native | 2,735.6 | — | 158.8 | 138.4 | 158.6 | 293.2 | 1,287.3 | 57.3 | — |
| 8 | B1 | 4,169.3 | 1,897.5 | 99.1 | 98.5 | 118.1 | 225.3 | 3,244.6 | 138.7 | 38.4 |
| 8 | C | 40.3 | 101.0 | 143.8 | 143.1 | 231.0 | 280.9 | 34.3 | 830.3 | 35 |
| 9 | native | 2,708.5 | — | 189.0 | 165.1 | 152.6 | 278.6 | 1,301.2 | 37.4 | — |
| 9 | B1 | 4,221.7 | 1,955.8 | 96.8 | 99.7 | 119.3 | 221.1 | 3,230.5 | 143.9 | 38 |
| 9 | C | 37.2 | 101.0 | 147.1 | 143.4 | 218.9 | 293.9 | 34.9 | 830.1 | 34.9 |
| 10 | native | 2,705.7 | — | 141.6 | 133.9 | 121.4 | 263.5 | 1,268.3 | 43.5 | — |
| 10 | B1 | 4,197.9 | 1,866.9 | 100.6 | 101.2 | 114.8 | 237.1 | 3,249.2 | 147.6 | 37.9 |
| 10 | C | 38.3 | 102.2 | 152.5 | 151.5 | 220.2 | 308.9 | 97.1 | 832.6 | 36.9 |

