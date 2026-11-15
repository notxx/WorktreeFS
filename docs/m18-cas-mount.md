# M18/B180100：CAS WinFSP 挂载视图与管线

M17 交付了 CAS 库层（manifest + hydration + 索引种子）。本 mission 把 CAS
视图接到 WinFSP：挂载 `<repo>/.tower/worktrees`，每个 worktree 是一个 CAS
视图；主仓库保持原生（spec §6.3），与 B1 同挂载点、同夹具对比。核心指标：
worktree add O(1)（零树复制）、status 不 hydrate（只读 manifest stat）、
diff/commit 只 hydrate 需要的文件、首次读有 hydration 延迟。

## 挂载模型

```text
<repo>/.tower/worktrees          ← WinFSP 目录挂载（junction，B1 同款）
├─ wt-cas-1/                     ← CAS 视图（wtfs_cas::CasView）
│   ├─ .git                      ← upper：真实 worktree 目录里的指针文件
│   ├─ <HEAD 树文件>             ← manifest 呈现（读时 hydrate）
│   └─ <用户改动/新文件>         ← upper（真实文件，落真实 worktree 目录）
└─ wt-1/…                        ← B1 overlay 视图（同一回调层，模板树 lower）
```

* **后端抽象**：`wtfs_vfs::ViewBackend` enum（`Overlay(Arc<Overlay>)` /
  `Cas(CasBackend)`）——`context.rs` 的全部 WinFSP 回调（open/create/read/
  write/cleanup/rename/set_*/enum）只看见后端无关的 stat/resolve/copy_up/
  finish_staged 接口。enum 分发（无 dyn vtable；stat/枚举是 status 热路径）。
  B1 直通零适配；C 侧 `CasBackend` 用 `Mutex<CasView>` 收敛 `&mut self` 写
  方法（WinFSP 回调并发 `&self` 进入）。
* **C 视图布局**：upper = **真实 worktree 目录**（`git worktree add
  --no-checkout` 的目标，`.git` 指针在其中，经 upper 自然可见——git 无需
  特判）；lower = manifest（HEAD 树的 path→oid/mode/size 内存索引）；meta =
  `<store_root>/cas-meta/<name>`（M12 标记日志，重开视图重放）；cat-file 与
  hydration 缓存 per-挂载共享（`<store_root>/cas-cache`，按 oid 全局分片）。
  真实 worktree 目录必须在**挂载点之外**——挂载根下的 mkdir 会被 B1 的
  auto_register 拦截。
* **挂载态切换（先 mount 再清位）**：`git worktree add --no-checkout` →
  `read-tree` + `seed_stat_aligned`（M17：skip-worktree 位 + gix-index stat
  改写为 manifest 值）→ `write_stat_config`（**新增**：`config.worktree` 写
  `core.checkStat=minimal / trustctime=false / untrackedCache=false`——
  Windows mingw lstat 不报卷序列号 dev、index ino 是 32 位截断，dev/ino 与
  FS 报告值不可逐位比较；checkStat=minimal 让 git 只比 mtime/size，这正是
  M17 种子对齐的 mtime/size 面）→ 挂载 + `register_cas_worktree`（FS 已在
  服务）→ **此后才** `clear_skip_worktree` 批量清位（48ms/4,874）。顺序
  颠倒（清位后未挂载）status 必报全量删除——E2E 锚点用例锁死该边界。

## 读写路径

* **stat/枚举零 hydrate**：manifest 命中 → 固定 stat（与 index 种子逐位对
  齐：dev=卷序列号、ino=blake3 路径 id（git 按 32 位截断比对，B1 同款）、
  size/mode=manifest、mtime=ctime=固定纪元）；upper 命中 → 真实 mtime。
  status/diff 干净树全程零对象读取（E2E 断言）。
* **读**：upper 真实文件；manifest 命中经 `HydrationCache::ensure`（cat-file
  → 分片 temp+fsync+rename 原子落盘）→ 读缓存文件——读到即物化。首读
  延迟 = cat-file 往返 + 一次 fsync（E2E 断言恰好 1 miss）；二读命中缓存。
* **写**（与 B1 同语义，dedupe 基准换成 git blob sha1）：
  * 首次写 copy-up：hydrate blob → 复制到 upper → mtime 归一固定纪元
    （copy-up 本身不打脏 index）；`CREATE_ALWAYS`/`TRUNCATE_EXISTING` 覆写
    跳过 copy-up（内容将被整体替换）。
  * 终裁（cleanup/flush）：内容按 `sha1("blob <len>\0" + 内容)` 回查 manifest
    oid——一致 → 回收 upper 回落 manifest（零 upper、零 fsync，写回原内容
    的 `git checkout --` 上限语义）；不一致 → 恰好一次 fsync 落 upper。
  * 新建文件：create 当场 `prepare_created_file` 保证路径可解析（B100100
    同款：whiteout 撤销 / 空 upper 占位）；零写入关闭补终裁（M10）。
* **删除/rename**：复用 M12 标记日志语义（upper 删真实文件；manifest 有
  内容记 whiteout；重开视图重放）。文件级 rename = upper 搬移/hydrate 复制
  + whiteout 源 + 撤销目标标记（POSIX 覆盖）；**目录 rename：manifest 有
  内容默认 EXDEV**（`STATUS_NOT_SAME_DEVICE`，与 M12 后的 B1 相同）。
* **refresh（HEAD/index 变化）**：`CasBackend::refresh_head()` 解析 worktree
  HEAD，commit 前进 → 重载 manifest + 剪枝失效 whiteout（新 manifest 已无
  该路径的标记删除，未提交的删除保留）。stat 不含 oid，重载不动摇 index
  对齐——commit 后 status 保持干净，无需立即刷新；跨分支切换的完整语义
  （路径复用冲突等）不在 M18。

## 缓存与 GC

* **hydration 缓存**：`<store_root>/cas-cache/<oid 前 2 位>/<oid>`（256 分
  片），跨 worktree 全局共享（同 blob 只存一份）。命中 = 一次
  `symlink_metadata` 探测（大小校验，不符即删重灌）；灌入 = cat-file 读 +
  temp+fsync+rename 原子落盘。统计（hits/misses/hydrated_files/bytes）是
  "status 零 hydrate" 的断言依据（`CasBackend::cache_stats()`）。
* **GC**：`HydrationCache::gc(max_bytes)` 按 mtime 最旧优先逐出至预算内，
  顺带清扫 `.tmp-*` 崩溃残留（M17 既有，显式调用）。删缓存不丢数据（真相
  在对象库）。upper/标记日志随真实 worktree 目录与 `cas-meta` 存活，由
  `wtfs gc` 的目录清理扩展点接管（M18 未挂接进 gc 计划——后续 daemon
  集成时接线）。

## 与 B1 的边界

| 维度 | B1（overlay + 模板） | C（CAS + manifest） |
| ---- | -------------------- | ------------------- |
| lower 数据 | 模板 checkout 物理树（共享只读） | Git 对象库 + manifest |
| 每 worktree 冷启 | 注册 + 挂载（模板已备） | add --no-checkout + 种子 + 注册（零树复制） |
| status | index-stat 对齐（模板 index 经 FS refresh） | skip-worktree 清位 + checkStat=minimal |
| 写路径 dedupe | blake3 对模板 hash（ToLower 回落） | git blob sha1 对 manifest oid（同语义回收） |
| 目录 rename | lower 有内容 EXDEV | manifest 有内容 EXDEV（同语义） |
| worktree remove | 目录清空即 rmdir（delete-on-close 注销） | **需 --force**（稀疏 upper：回收文件物理不在，git 对真实路径的干净检查必报删除；shim/daemon "先注销再 remove" 留后续） |
| initializing 暂存流 | checkout 期 staged 写（M8/M9） | 无（注册即 ready；checkout 语义由种子替代） |
| daemon/watcher | index watcher 驱动 mark_ready/config.worktree | M18 未接线（显式流程：库 API 驱动） |
| 主仓库 | 原生 | 原生（同一挂载点，两方案视图可共存注册） |

同挂载点混合注册受支持：`VfsRegistry` 按 `ViewBackend` 分派，`wt-N`（B1）
与 `wt-cas-N`（C）可同卷枚举。daemon 的 `wtfs status` 对 CAS 视图报
`ready (cas)`（无 DB 状态机）。

## 运行

```bat
:: 全部非挂载测试
cargo test -p wtfs-cas -p wtfs-core -p wtfs-vfs

:: E2E（真实 WinFSP + git）
cargo test --release -p wtfs-vfs --test cas_mount_integration -- --ignored --test-threads 1

:: B1 回归（M9 零 fsync / M10 / B100100 / B070100）
cargo test --release -p wtfs-vfs --test mount_integration -- --ignored --test-threads 1

:: 挂载基准（4,874 文件 / 10 worktree，同 M12 夹具）
set WTFS_BENCH_REPO=C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo
cargo test --release -p wtfs-vfs --test cas_mount_bench -- --ignored --nocapture
```

## 基准记录（2026-11-15，Windows，release，M12 夹具 git.git 4,874 文件 / 10 worktree）

harness：`tests/cas_mount_bench.rs`（`#[ignore]`，`WTFS_BENCH_REPO` 指向
`C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo`，与 B1 同夹
具同口径）。

| 指标 | 值 |
| ---- | -- |
| add（`worktree add --no-checkout --detach`，每 wt 中位） | **82.3 ms** |
| 种子（read-tree + gix-index stat 对齐 + stat 配置，每 wt 中位） | 99.6 ms |
| **add 全程（add + 种子 + 注册 + 清位）** | **~182 ms/worktree**（对比全量 checkout ~2.7 s；零树复制，O(1) 达成） |
| status 冷（挂载视图全树，每 wt 中位） | **153.0 ms** |
| status 热（每 wt 中位） | 152.1 ms |
| diff --stat 干净树（每 wt 中位） | 228.5 ms |
| commit -am 单文件（含刷新，每 wt 中位） | 302.3 ms |
| remove（worktree remove --force + 注销，每 wt 中位） | 65.2 ms |
| 首次读 200 文件（冷，合计） | 837.3 ms（**4.19 ms/文件**，fsync 主导，与 M17 hydrate 口径一致） |
| status 期间 hydrate | 14 次——**全部是 `.gitignore` exclude 读取**（见下），tracked 内容 **0** |
| hydration 缓存（首读 200 + excludes 后） | 216 文件 / 3.57 MB（oid 全局共享，10 wt 共用一份） |
| upper 磁盘（10 wt 提交 1 文件后合计） | **20 文件 / 1.0 KB**（= 10 个 .git 指针 + 10 个已提交编辑；回收机制让脏 worktree 近零落盘） |
| 标记日志磁盘 | 0 文件 / 0.0 KB（bench 无删除） |
| manifest 装载（一次，跨 wt 共享） | 4,874 文件 / 50.5 MB 树（ls-tree 一次） |

**status 期间 hydration 的机制核验**（E2E 小仓库断言 0 miss，大夹具
14 miss 的差异根因）：`core.checkStat=minimal` 下 tracked 内容的 stat 全部
逐位对齐，`git status` 对 tracked 文件零读取；但 git 的 untracked 走查要读
per-directory `.gitignore` 做 exclude 分类（`--untracked-files=no` 时零读
取，已实测）。git.git 全树 14 个 `.gitignore` 在**首个 worktree 的首个
status 各读一次**，hydration 缓存按 oid 全局共享 → 其余 9 个 worktree 全部
缓存命中。bench 断言口径：misses 全部落在 `.gitignore`（非 exclude 文件
零 hydrate），tracked 内容零 hydrate 的口径由 demo 仓库 E2E 锁死（misses
== 0）。

**读法**：

* add O(1) 达成：~180 ms/worktree 全程，只有 index 重写随文件数线性
  （与 M17 route A 数字一致），零文件树复制；B1 同夹具的 add 走模板
  checkout，一次性成本更高但后续 worktree 共享模板——对比口径见 m17 文档。
* status 143 ms（4,874 文件全树）：FS 侧按 list_stat 口径（每目录一次
  readdir + 逐项一次 metadata），33 µs/文件的 stat 上限被 WinFSP 回调
  开销摊平；冷热一致（无 stat 缓存衰减）。
* 首读 4.15 ms/文件 = cat-file 往返 + 恰好一次 fsync（M17 hydrate 冷态
  同口径）；缓存命中后为纯探测（µs 级）。
