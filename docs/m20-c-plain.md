# M20/B200100：C 挂载视图上 plain `git worktree add`（不带 `--no-checkout`）实测

M19 对比了 B1（plain `git worktree add`，模板 + overlay）与 C（显式
`--no-checkout` + seed + 注册，CAS + manifest）。本 mission 补第三组：**在 C
挂载视图上直接跑 plain `git worktree add`（不带 `--no-checkout`）会怎样**。
用户原话："测试完再比较一下，不带no checkout选项会是什么样？"

## 结论先行

1. **C 挂载上 plain `git worktree add` 根本不走 C**。挂载根 mkdir 命中的是
   `VfsRegistry::auto_register`（`crates/wtfs-vfs/src/context.rs:917-941`，
   registry 级共享逻辑）——建出的是 **B1 overlay**；C 的 harness store 没有
   模板 → 走 B1 的 **full-copy 兜底**（每文件全量写 upper + 逐文件 fsync）。
   CAS/manifest/hydration 全程不参与（实测路由证据见下）。
2. **无 daemon 时 plain add 的产物不可用且不可回收**。B1 的设计闭环是
   auto_register（Initializing）→ git 写 index → daemon watcher →
   mark_ready。C 挂载没有 daemon：视图永远停在 Initializing，而 Initializing
   期"**除 worktree 根外所有按名查找都报不存在**"（`context.rs:8-12` 模块注
   释，写入路径不受影响）——于是：
   * `git status` 把整树报成删除（D 条数 = 全部 index 条目）；
   * 后续任何 `git worktree` 操作（含下一个 `worktree add` 内部的
     prunability 检查）看到"没有 `.git` 的目录"→ **把前一个 worktree 的
     admin 元数据 prune 掉**（实测：第 2 个 wt add 后，第 1 个 wt 的
     `.git/worktrees/wt-plain-1` 整目录消失；第 2 个 wt 自己的 admin 只剩
     `HEAD/index/logs`、丢 `gitdir/commondir` → "not a git repository"）；
   * `git worktree remove --force` 直接 `Permission denied`
     （`context.rs:1372-1384`：Initializing 的根 rmdir 拒绝），git 退出且
     **4,875 个文件全残留**在 upper，admin 已被移走 → 只能原生手动清理。
   这就是"shim/daemon watcher 是 C 的关键前置"的实证形态：B1 的 daemon 在
   index 落盘后立即 mark_ready 关掉这个窗口；C 挂载上窗口永远开着。
3. **强制经 CAS 视图全量物化（模拟 plain add 的 checkout 阶段）**：4,874 文
   件 `reset --hard` 稳态 25.5 s（冷首跑 41.4 s）。
   upper **并未**如预期被物化到模板大小（~49.7 MB），而是部分物化：内容与
   manifest blob 一致（二进制、无 EOL 转换）的写入在 close 时被回收回虚拟
   （`wtfs-cas/src/view.rs:503-565` 的回收语义），只有经 CRLF/text 转换、内
   容与 blob 不一致的文件留在 upper；与此同时 **hydration 缓存吃满整树**
   （copy-up 逐文件 `cache.ensure`）——"磁盘优势消失"的形态不是 upper 膨
   胀，而是**缓存膨胀 + checkout 时间爆炸**；缓存热了之后首读反而 0 miss。

## 实测口径（必读）

* **leg A（真实路径）**：`git worktree add --detach <挂载根>/wt-plain-N HEAD`
  （不带 `--no-checkout`），git 目标路径在挂载根下，触发真实的
  auto_register 路由。**这是真实的 plain add**；但因 C 挂载无 daemon，其
  后续 git 操作（status/diff/commit）如实记录失败与脏输出——这些失败本身
  就是发现。
* **leg B（模拟路径，明确标注）**：`worktree add --no-checkout`（挂载外真实
  目录）+ `seed_stat_aligned` + `write_stat_config` + 注册 CAS 视图 +
  `clear_skip_worktree`（M18 显式流程）→ **删 index + `git reset --hard`
  强制经 CAS 视图全量物化**——这是 plain add 在 checkout 阶段会做的事的
  近似等价（plain add 内部即新空 index + hard reset 逐文件 checkout）。
  **这是模拟，不是真实 plain add**（真实 plain add 见 leg A——它被
  auto_register 拦去 B1 兜底，根本到不了 CAS 层）。
* 夹具：git/git @923ecfe（4,874 文件，本地 clone 每轮新建），与 M19 同夹
  具同口径；M19 的 B1/C 数字直接引用 m19 文档。
* 驱动：`scripts/m20-driver`（进程内 `VfsRegistry` + WinFSP 挂载，与 M18
  `cas_mount_bench` 同机制；lib API 驱动，**非 CLI/shim**）。编排脚本
  `scripts/bench-m20-c-plain.ps1`（轮次目录、超时 + taskkill 兜底、清理）。
* C 挂载的 store 无 B1 模板（模板表为空）→ leg A 的 auto_register 走
  full-copy 兜底——这是"C 方案的 store 形态"下的真实兜底行为（B1 落地时
  会有模板，其 plain add 见 M19 表）。
* 每腿 3 wt；leg A/B 的 ops 行取 2 轮正式 + 1 轮干净复跑（remove 流程修复
  后）共 3 次运行，轮间 fresh clone + 独立 store；两轮数据逐项一致（复现
  性见各表）。leg A 的 remove 三行取干净轮（此前轮次的 remove 流程有
  prune 污染，弃用并在表中标注）。
* 时间与机器：2026-11-16，Windows，release 构建。

## leg A：真实 plain add（挂载根 mkdir → auto_register 兜底）

<!-- leg A 表：ops 行取 2 轮正式 + 干净复跑共 3 次运行（每值 = 该轮 3 wt 中位）；
     remove 三行取修复后的干净轮（r1/r2 的 remove 流程有 prune 污染，弃用） -->

| 指标（每 wt 中位） | r1 | r2 | 干净轮 | 取值 |
| ------------------ | -- | -- | ------ | ---- |
| **plain add（挂载根 mkdir）** | 22,876.5 ms | 22,773.5 ms | 22,792.2 ms | **22,825 ms（ok 9/9）** |
| add 后端实测 | B1 overlay（auto_register 兜底），initializing=true | 同 | 同 | — |
| upper（add 后，每 wt） | ≈50,447 KB / 4,875 文件 | 同 | 同 | **≈49.3 MB / 4,875 文件（full-copy）** |
| status 冷 / 热 | 127.4 / 126.4 ms | 126.5 / 127.1 ms | 127.7 / 127.8 ms | ~127 ms，**脏 = 4,874 条 D（全部条目）** |
| diff --stat | 1,527.5 ms | 1,509.9 ms | 1,491.8 ms | ~1,510 ms |
| commit -am 单文件 | 1,552.2 ms | 1,580.3 ms | 1,584.5 ms | ~1,580 ms（把整树 D 提交掉） |
| remove 首试（无 daemon） | —（污染弃用） | — | ok 0/2，145.5 ms，`Permission denied` | **失败；upper 4,875 文件全残留、admin 已被 git 移除** |
| remove 兜底（原生删 upper） | — | — | 1,077.0 ms | **1,077 ms（no-shim 唯一脱手方式）** |
| remove（mark_ready 后 = 模拟 daemon，末位 wt） | — | — | ok，9,700.8 ms | **9,700.8 ms（经视图删完整 upper + rmdir）** |
| upper 合计（3 wt，commit 后） | 151,335.7 KB | 151,335.7 KB | 151,335.7 KB | 147.8 MB（= 3 × 49.3 MB） |

行为不一致备注：r1/r2（修复前的 remove 流程）中 wt-1 的等价无 daemon 尝试曾
"成功"（~135 ms，文件删净）；修复后的干净轮同状态两次均 `Permission denied`。
成功/失败的边界条件未定位，已归档 TowerFinding（产品层核查 Initializing
状态机 × 根 rmdir 门控）。

**路由证据**（driver 实测 `registry.get_backend`）：`B1 overlay（挂载根
mkdir 命中 auto_register；store 无模板 → full-copy 兜底）`，且 add 后
`is_initializing = true`。git 的 add 输出含
"Updating files: 100% (4874/4874), done."——checkout 确实逐文件走完了视图。

**三个破坏点**（无 daemon 时，全部实测）：

1. **status 报整树删除**：cold/hot status 均非空，D 条数见上表（Initializing
   按名查找抑制 → 视图对 git 呈"空目录"）。`diff --stat` 同理慢一个数量级
   （对比 M19 B1 的 113 ms）。
2. **commit 语义破坏**：`commit -am` "成功"（把 D 条目全部提交掉）。
3. **remove 失败 + 残留**：`git worktree remove --force` 报
   `error: failed to delete '…wt-plain-N': Permission denied`（Initializing
   根 rmdir 拒绝，干净轮 0/2 成功），upper 的 4,875 文件全部残留、admin 元
   数据已被 git 移除。唯一脱手方式 = 原生手动删 upper（1,077 ms/wt）。
   mark_ready（模拟 daemon）后 remove 可正常完成（9,700.8 ms，经视图删完
   整 upper + rmdir）。跨 wt 的连带破坏见"结论先行"第 2 条。

## leg B：强制经 CAS 视图全量物化（模拟 plain add 的 checkout 阶段）

<!-- leg B 表（2026-11-16，2 轮 × 3 wt，每值 = 该轮 3 wt 中位；两轮一致性即复现性） -->

| 指标（每 wt 中位） | r1 | r2 | 两轮中位 |
| ------------------ | -- | -- | -------- |
| add `--no-checkout` 元数据 | 35.1 ms | 36.6 ms | 35.9 ms |
| seed + stat 配置 | 110.1 ms | 106.8 ms | 108.5 ms |
| 清位（4,874 entries） | 56.5 ms | 54.8 ms | 55.7 ms |
| **强制物化 checkout（删 index + `reset --hard`）** | **25,493.6 ms** | **25,608.0 ms** | **25,550.8 ms** |
| upper（checkout 后，含 .git 指针） | 33,049.5 KB / 1,540 文件 | 同左（逐位一致） | 32.3 MB / 1,540 文件 |
| status 冷 / 热 | 364.8 / 271.7 ms | 323.5 / 306.5 ms | 344.2 / 289.1 ms（脏 0） |
| status 期间 hydrate | 0 miss | 0 miss | 0 |
| diff --stat | 382.8 ms | 429.5 ms | 406.2 ms |
| commit -am 单文件 | 495.2 ms | 503.0 ms | 499.1 ms |
| 首读 200 文件（冷） | 317.9 ms（1.59 ms/文件） | 308.4 ms（1.54 ms/文件） | 313.2 ms，**miss 0** |
| hydration 缓存（首读后，全局共享） | 4,757 文件 / 49,288.8 KB | 同左 | 48.1 MB（总 miss 4,757） |
| remove（--force + 注销） | 483.5 ms | 470.5 ms | 477.0 ms |

备注：首次冒烟（冷 OS 页缓存/首建缓存）的 checkout 观测为 41.4 s；稳态两轮
均为 ~25.5 s。上表取稳态值。

**机制解读**：

* checkout 慢（稳态 25.5 s ≈ 5.2 ms/文件；冷首跑 41.4 s）的成本构成：每文件经视图写 →
  copy-up（`cache.ensure`：cat-file + temp+fsync+rename 落缓存）→ git 内容
  写入 → close 终裁 sha1 比对：一致 → 回收 upper（零 fsync）；不一致（CRLF
  转换过的文本文件）→ 恰好一次 fsync 落 upper。缓存灌满整树（~48 MB）+
  文本文件的 upper 双写，主导了总时长。
* **upper 部分物化而非全量**：推翻了"upper 接近模板 49.7 MB"的预期——CAS
  写路径对"内容与 manifest 一致"的写入回收（`view.rs:503-565`），只有 EOL
  转换造成的差异文件（本轮 1,540 个）留在 upper。C 的磁盘优势没有消失在
  upper，而是转移成 hydration 缓存的全树膨胀（48.1 MB ≈ 树大小）——净效果
  与全量物化相当，还叠加了 checkout 的时间成本。
* **status 干净且冷热一致**（~330 ms，比 C-lazy 的 ~144 ms 慢 2.3×）：
  `reset --hard` 重写的 index 直接取自视图 stat（回收文件 = 固定纪元，
  upper 文件 = 真实 mtime），无需 M17 的 stat 种子就自然对齐；变慢来自
  1,540 个真实 upper 文件的逐项探测 + 无 skip-worktree 的全量 index 走查。
* **首读 0 miss**：checkout 的 copy-up 已把全部 blob 灌进缓存——首读从
  C-lazy 的 ~4.2 ms/文件降到 ~1.4 ms/文件（缓存命中，µs 级探测 + PS/Rust
  读差异）。"首读不再触发 hydrate"成立，但原因是**缓存已热**（checkout 时
  付过费），不是"内容已在 upper"。
* diff/commit 都比 C-lazy 慢（406/499 ms vs 221/286 ms）：同上，upper 真实
  文件的 stat 与内容探测更多。
* remove 变慢（~477 ms vs C-lazy ~34 ms）：要删的不再是稀疏 upper，还有
  1,540 个物化文件。

## 三组对比：B1 plain add / C lazy add / C plain（模拟物化）

<!-- 对比：B1 / C-lazy 数字引自 docs/m19-b1-vs-c.md（同夹具 git/git @923ecfe
     4,874 文件、3 轮中位）；C-plain 两列为本 mission。 -->

| 指标（每 wt 中位） | B1 plain add（模板路径，M19） | C lazy add（`--no-checkout`+seed，M19） | C plain add（真实路径，M20 leg A） | C 强制物化（模拟，M20 leg B） |
| ------------------ | ----------------------------- | --------------------------------------- | -------------------------------- | ----------------------------- |
| add 全程 | 4,005 + ready 1,639 = **5,632 ms**（daemon 闭环） | **~138 ms**（add 38.4 + seed 99.9，含注册/清位 ~190 ms） | **22,825 ms**（full-copy 兜底） | 元数据 34.2 ms + 强制 checkout 25,532 ms |
| add 后磁盘 / wt | upper <1 KB（模板一次性 49.7 MB 共享） | upper ≈1 KB（缓存按读增长） | **≈49.3 MB（全量物化）** | upper 32.3 MB + 缓存 48.1 MB |
| status 冷（干净树口径） | 99.3 ms | 144.5 ms | 127.7 ms 但**整树 D（不可用）** | 336.7 ms（净） |
| diff --stat 干净树 | 113.1 ms | 221.4 ms | 1,491.8 ms | 428.0 ms |
| commit -am 单文件 | 230.2 ms | 286.2 ms | 1,584.5 ms（提交了整树删除） | 513.2 ms |
| remove --force | 3,195 ms | 34.0 ms | 失败（denied）→ 兜底 1,077 ms；daemon 路 9,700.8 ms | 472.6 ms |
| 首读 200 文件（冷） | 146.2 ms | 848.5 ms（4.2 ms/文件） | —（视图不可用） | 281.2 ms（0 miss——缓存已热） |

归因：

* **add**：B1 模板路径 O(files) 但有模板 dedupe（4.0 s + ready 1.6 s）；C
  lazy O(1)（~138 ms 全程）；C-mount plain add 是 full-copy 兜底（23 s，逐
  文件 fsync），**比 B1 慢 5.7×**——C 的 store 没有模板，plain add 连 B1 的
  速度都拿不到。
* **status/diff/commit**：B1/C-lazy 都是干净树快路径（99-221 ms）；C-mount
  plain add 因无 daemon 卡 Initializing，status 语义已破坏（整树 D），数字
  不可比也不可用。
* **磁盘**：B1 模板 49.7 MB（一次性）+ 每 wt upper <1 KB；C-lazy 每 wt
  upper ≈ 1 KB + 全局缓存按读增长；C-mount plain add 每 wt upper ≈ 50 MB
  （full-copy）——10 wt 场景下磁盘成本最差。
* **首读**：B1 µs 级（树已物化）；C-lazy ~4.2 ms/文件（冷，cat-file+fsync）；
  C 模拟物化 ~1.4 ms/文件（缓存已热——但这是 checkout 多花 25.5 s 换来的）。

## 为什么 shim / `--no-checkout` 是 C 的关键前置（本 mission 的回答）

1. **不拦截就退化为 B1 的最差路径**：plain add 在 C 挂载上命中
   auto_register → B1 overlay full-copy 兜底——比 B1 正常的模板路径慢
   5.7×、磁盘最差、CAS 的全部收益（O(1) add、稀疏 upper、零树复制）一个
   都拿不到。C 存在的意义要求 add 必须走 `--no-checkout` + seed 的显式流
   程，而用户手敲的 `git worktree add` 不带 `--no-checkout`。
2. **就算拦截了 checkout，也还需要 daemon 关掉 Initializing 窗口**：B1 用
   daemon watcher 在 index 落盘后 mark_ready；C 挂载没这条链，视图永远
   Initializing → status 报整树删除、后续 worktree 操作把 admin prune 掉、
   remove 直接拒绝。**shim 拦截 + daemon/等价的 ready 化，缺一不可**。
3. **`--no-checkout` + seed（M18 显式流程）是当前唯一可用路径**，其代价已
   由 M19 量化（add 全程 ~190 ms）；本 mission 补充的数字说明：替代这条显
   式流程的任何"让 plain add 直接工作"的方案，要么拦截改写（shim），要么
   接受 full-copy 兜底 + Initializing 窗口的全部破坏。

## 后续工作（超出本 mission，不在本分支）

* shim 拦截 `worktree add`（M18 遗留）：把显式流程藏进 plain add；拦截点
  需同时处理"目标在挂载根下"的 mkdir 路由（现在会被 auto_register 抢走）。
* daemon watcher 的 CAS 等价物：即使 shim 拦截失败回落真实 plain add，也
  需要在 index 落盘后把视图 ready 化（或 auto_register 对 CAS store 生成
  CAS 视图而不是 B1 overlay——需要产品决策）。
* `remove` 前先注销（M19 改进清单第 5 条）在 plain-add 场景同样适用。

## 复跑

```powershell
# 前置：仓库可构建（cargo），LIBCLANG_PATH 缺省 %LOCALAPPDATA%\wtfs\tools，
# WinFSP 运行时已安装（driver 动态加载 winfsp-x64.dll，脚本会补 PATH）。
powershell -ExecutionPolicy Bypass -File scripts\bench-m20-c-plain.ps1 `
    -FixtureRepo C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo `
    -Worktrees 3 -Rounds 2 -OutFile %TEMP%\wtfs-m20-results.md
```

脚本行为：每轮 fresh clone → driver（leg A + leg B）→ 逐轮落 driver 的
markdown 报告 + KV → 轮次汇总表。driver 首次构建 ~30 s（复用仓库 target
产物），`-Build` 强制重建；`-DriverTimeoutSec`（默认 1500 s）超时 kill 进程
树；跑完默认清理轮次目录（`-KeepRounds` 保留 driver-stderr.log /
driver-stdout.log 原始输出）；`-RoundDirPrefix` 改轮次目录前缀（并发跑防
冲突）。PS 脚本与 driver 报告均 UTF-8（脚本需带 BOM 保存，PS5.1 兼容）。
