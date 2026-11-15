# M0：dedupe-only 使用说明与实测

WorktreeFS M0 是 **dedupe-only** 阶段：不拦截任何命令（无 hook、无 git shim），
`git worktree add` 照常执行。WinFSP 把 `<repo>/.tower/worktrees` 挂载为 overlay
卷，FS 在写入路径把与模板一致的内容丢弃（读时回落模板 lower），只有改动才落进
每个 worktree 私有的 upper 目录。收益在磁盘与写 IO；创建时间与原生同级（O(1)
创建是 M1+ 的目标，见 spec §6.3/§12）。

## 构建与前置条件

- Windows 10/11 + [WinFSP](https://winfsp.dev/) 运行时（`winfsp-x64.msi`）。
- git CLI（`git worktree` 流程不经过任何 shim）。
- 构建：`cargo build --release`。`winfsp-sys` 需要 clang：设
  `LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin`（本机路径，无此
  目录时自行安装 LLVM/clang 并指向其 bin）。
- 产物：`target/release/wtfs.exe` 与 `wtfsd.exe`（同目录，`wtfs mount` 会拉起
  同级的 `wtfsd.exe`）。

## 用法

```bat
:: 模板（dedupe 的 lower 来源；mount --base 也会自动创建，已存在则幂等）
wtfs template create --repo <repo> --base <branch>

:: 挂载 <repo>/.tower/worktrees（并启动 wtfsd 守护进程）
wtfs mount --repo <repo> --base <branch> [--store-root <dir>] [--pipe <name>]

:: 状态（daemon 与每个 worktree 的 state）
wtfs status [--json]

:: 清理 dead 目录 / 孤儿 upper / 旧模板（daemon 在线走管道，离线需 --repo）
wtfs gc [--dry-run] [--repo <repo> --store-root <dir>]

:: 停止 daemon 并卸载
wtfs unmount [--pipe <name>]
```

- 挂载点固定 `<repo>/.tower/worktrees`。目录挂载要求挂载点为空或不存在；
  非空（例如已有真实 worktree）会拒绝挂载，不会吞掉现有数据。
- 卸载后挂载点恢复为空目录，Tower 的目录契约不变。
- 存储布局（`--store-root`，默认 `%LOCALAPPDATA%\wtfs\repos\<repo-id>`，
  spec §8.1）：`templates/<commit>/{tree,index}` 为模板；`worktrees/<name>/upper`
  为各 worktree 私有改动。

### Tower 场景（M0：无 hook、无 shim）

1. `wtfs mount --repo <workspace-repo> --base <base-branch>`。
2. Tower 照常 `git worktree add <repo>/.tower/worktrees/wt-N`：
   mkdir 经 FS 自动注册 overlay；checkout 写入逐文件与模板比对去重。
3. git 写完 worktree index 后，daemon watcher 切 `ready`（视图变
   “模板 + upper”）。
4. worker 在 worktree 内正常 `git status/diff/add/commit`、构建、测试。
5. `git worktree remove`（挂载中即可正常完成）；overlay 注销后
   `wtfs gc` 回收孤儿 upper。

## 正确性行为

- `git status` 干净、`git diff`/`git add`/`git commit`/`git checkout`/`git reset`
  行为正确；编辑文件立即以 ` M` 呈现。
- stat 对齐：FS 对模板文件上报固定 mtime/ctime/ino/dev；daemon 为 worktree 写
  `config.worktree`（`core.checkStat=minimal`、`core.trustctime=false`、
  `core.untrackedCache=false`），ready 后 `git status` 不需要全量哈希。
- dedupe 上界：无改动的 worktree，upper 只剩 `.git` 指针文件（几十字节）。
- 编辑 → copy-up 到 upper；删除 lower 文件 → whiteout 标记；删除 lower 目录 →
  opaque 标记；覆写（含缩短）EOF 精确截断（B060100 修复）。
- `git worktree remove` 在挂载中直接成功（B060100 修复：worktree 根目录删除
  已放开，空视图校验后放行）；注销 worktree 时标记树整树删除，无残留。

## whiteout / rename 语义（M12/B120100，append-only 标记日志）

删除与改名的状态不再存 SQLite 表（schema v4 起 `whiteouts` / `renames` 两张表
已删除，v3 及更早的库打开时自动迁移为标记），而是落在每个 worktree 私有的
append-only 标记日志里：

```
<store>/worktrees/<id>/
├─ upper/                  # 改动文件（数据真相，无标记文件）
└─ meta/
   ├─ markers.log          # append-only 删除标记日志（ready 态）
   └─ redirect/<view-rel>  # 内容 = 原路径：v3 目录改名映射的迁移形态（只读）
```

- 记录格式：u32 LE 长度 + op（1=add 删除 / 2=remove 撤销）+ UTF-8 相对路径。
  unlink/rmdir 每文件只追加一条记录（句柄常驻、单次 write_all、µs 级、无
  fsync），**零 SQLite 写事务**——大仓库逐文件删除不再产生逐文件事务。
- 内存镜像为读路径真相：daemon 打开 worktree 时顺序重放日志装载删除集合；
  每次删除/撤销先更新内存再追加。撕裂尾记录（长度越界/非法 op）在重放时
  忽略——崩溃可能丢失部分删除标记，被删 lower 内容重现（spec §8.3 允许，
  与 M11 一致）。
- 压实：日志条目数 >1024 且 > 2× 存活数时，打开时用 temp+rename 压缩为只含
  存活 add 记录的快照。
- 解析顺序：upper > whiteout > lower；删除标记命中自身或任一祖先目录时
  lower 内容隐藏（目录删除与文件删除共用一种标记，阻塞语义同构：目录若在
  upper 重建则只显示 upper 内容）。重建文件（写回）即追加 remove 记录撤销。
- **initializing 期（git checkout）删除标记只缓冲在内存**——checkout 的
  unlink→重建循环每文件产生一对标记增删，缓冲后干净检出零标记写入；
  daemon 切 ready 时把存活集合一次性写成快照（`markers.log`）。
- 文件 rename = 源 whiteout + 目标 upper（搬移或 copy-up）。
- 目录 rename：lower 有内容 → `STATUS_NOT_SAME_DEVICE`（OverlayFS 默认 EXDEV，
  调用方按跨设备语义退化为递归复制+删除）；仅 upper 新建的目录（lower 无内容）
  → 纯物理改名，零标记。
- redirect 标记只由 v3→v4 迁移产生（旧 `renames` 行：`redirect/<to>` 内容为
  `from`），迁移后的改名子树 readdir/stat/read 正确；运行期不再新增。
- 注销 worktree（`git worktree remove` / watcher 元数据消失路径）：DB 行删除；
  日志与 `meta/` 随 upper 目录作为孤儿由 `wtfs gc` 整树回收——注销路径零逐项
  工作。同 id 重新注册时清扫残留日志，旧墓碑不复活。

## 限制（M0 已知）

- 需要 WinFSP 运行时；挂载期间 `<repo>/.tower/worktrees` 被卷占用，直接按
  真实目录访问只能看到 FS 视图（unmount 后恢复）。
- `initializing` 期（checkout 进行中）除 worktree 根外 readdir 为空——git 的
  “目录必须为空”检查依赖这一点；此时其他进程读该 worktree 看不到文件，
  ready 后才完整可见。
- 模板缺失/创建失败 → 全量 copy 兜底：worktree 仍然正确，只是没有磁盘收益
  （spec §6.3.6）。
- index stat 对不上时（兜底路径），git 首次 `status` 做一次全量哈希，慢但正确。
- 不支持（检出/挂载前检测，spec §11.2）：submodule、ADS、reparse point、
  sparse/split index、大小写冲突、Windows 保留名、非 UTF-8 路径。
- 构建产物（`target/`、`CMakeCache` 等）不跨 worktree 共享；路径敏感产物仍会
  重建。
- dead 目录与孤儿 upper 的回收要跑 `wtfs gc`（合并/teardown 后手动执行或由
  hook 触发）。

## 基准

基准 harness 在 `crates/wtfs-daemon/tests/m0_benchmark.rs`（`#[ignore]` 集成
测试，daemon 经库 API 进程内驱动——无 detached 进程、无命名管道）；
`scripts/bench-m0.ps1` 是它的封装：设 `WTFS_BENCH_REPO`/`WTFS_BENCH_ITERS`
后跑 release 测试并提取报告落盘。对同一来源仓库的两个独立 clone 分别测量
（互不共享状态）：

```bat
powershell -ExecutionPolicy Bypass -File scripts\bench-m0.ps1 [-Repo <真实仓库>] [-Iterations 3] [-OutFile bench-results.md]
```

- 默认数据源：`tests/fixtures/demo-repo`（夹具仓库，10 个文件、三层嵌套）。
- `-Repo <path>` 传入真实仓库时按 `git clone --local` 快照后测量，不触碰原
  仓库（原仓库的 live worktree 不受影响）。若快照含 submodule gitlink，
  harness 在本地 clone 内移除 gitlink 再测（M0 模板创建拒绝 submodule 条目，
  spec §11.2 的回退范围），其余 tracked 文件照常测量。
- 指标：worktree add 耗时、磁盘（可见树大小；WorktreeFS 另报私有 upper）、
  `git status --porcelain` 首次（冷）与第二次（热）耗时、remove 耗时、
  WorktreeFS 的一次性模板创建 + 挂载成本。harness 同时断言每轮 `git status`
  干净（正确性优先）。
- 直接调用 harness（不经封装）：
  `cargo test --release -p wtfs-daemon --test m0_benchmark -- --ignored --nocapture`。

### 实测环境

- 2026-11-12，Windows（NT 10.0.26300，AMD64），WinFSP 2.1
  （`C:\Program Files (x86)\WinFsp`），git 2.53.0.windows.1，
  WorktreeFS @ `59b6fb4` + 本次测试提交，release 构建，3 轮迭代取中位数。

### 实测结果：夹具仓库（10 tracked files）

| metric | native worktree | WorktreeFS |
|---|---|---|
| worktree add (median) | 124 ms | 219 ms |
| add + ready switch (median) | - | 260 ms |
| disk: worktree tree (mean) | 1.9 KB (11 files) | 1.9 KB (11 files) |
| disk: private upper store (mean) | - | 0.1 KB |
| git status cold (median) | 34 ms | 46 ms |
| git status hot (median) | 36 ms | 49 ms |
| worktree remove (median) | 69 ms | 155 ms |
| one-off: template + mount | - | 240 ms + 36 ms |

小仓库上 WorktreeFS 创建约慢 100ms（FS 写入路径逐文件 hash/比对与 WinFSP
往返），upper 只剩 `.git` 指针（~85 字节）；磁盘收益随仓库变大放大，见下面
真实仓库数据。

### 实测结果：dashboard（783 tracked files，真实仓库）

来源：本机 `dashboard` 仓库 `git clone --local` 快照（branch dashboard
@ `7b9208e76535`）；快照含 3 个 submodule gitlink（`apps/appointment-system`
、`apps/roombook`、`设计稿…`），M0 模板创建拒绝 submodule 条目（spec §11.2
回退范围），harness 在本地 clone 内移除后测得 783 个 tracked 文件。可见树
42.31 MB。

| metric | native worktree | WorktreeFS |
|---|---|---|
| worktree add (median) | 805 ms | 5064 ms |
| add + ready switch (median) | - | 5125 ms |
| disk: worktree tree (mean) | 42.31 MB (784 files) | 42.31 MB (784 files) |
| disk: private upper store (mean) | - | 0.1 KB (89 B) |
| git status cold (median) | 172 ms | 358 ms |
| git status hot (median) | 177 ms | 358 ms |
| worktree remove (median) | 451 ms | 5389 ms |
| one-off: template + mount | - | 1088 ms + 37 ms |

### 结论（M0 口径）

- **磁盘目标达成**：无改动 worktree 的私有开销是 89 字节（`.git` 指针），
  对比原生全量 42.31 MB；spec §6.3 验收"无改动 upper <5MB"远超达成。6.8GB
  级的 `.tower/worktrees` 堆积理论上可压缩到每个 worktree 几百字节量级。
- **创建时间未达标**：786 文件实测 ~5.1s（add + ready），高于 spec §12 预估
  的 1.5–3s，约为原生（0.81s）的 6.3 倍。瓶颈在 FS 写入路径的逐文件
  hash/比对与 WinFSP 往返。M0 的定位是"验证 FS/overlay/dedupe 正确性与磁盘
  收益"，创建时间优化（O(1) 创建）本来就是 M1+（shim + `--no-checkout` +
  index 改写）的目标。
- **`git status` 变慢一倍**：热 status 358ms（原生 177ms），未达 spec §12
  "<100ms"目标；对 agent 使用可感知但不阻塞（每次编辑后的 status 查询仍在
  亚秒级）。
- **删除明显偏慢**：`git worktree remove` ~5.4s（原生 0.45s）——删除路径
  同样逐文件走 FS；对 Tower teardown 场景可接受但值得后续优化。
- 正确性不受影响：harness 每轮断言 `git status` 干净，三轮全绿。

## 读路径 profiling 与优化（M13/B130100）

### 回调级 profiler

`WTFS_VFS_PROFILE=1`（或 `true`/`on`）启用进程内回调级 profiler
（`wtfs-core::profiling`）；关闭时热路径只剩一次原子 load——零计时、零
分配、零锁，生产路径零开销。计量点分三层，各含次数/累计/均值/最大耗时：

- `cb.*`：16 个 WinFSP 回调（get_security_by_name/open/create/get_file_info/
  read_directory/cleanup/close/…）的 handler 墙钟；
- `ov.*`：overlay 逻辑入口（stat/resolve/list_stat/derive_ino/map_to_lower…）；
- `nt.*`：NTFS 系统调用（symlink_metadata/is_dir 探测/read_dir/
  dir_entry_meta/open/read…）。

耗时 ≥200µs 的调用进 Top-32 慢调用列表（带路径；路径字符串仅慢调用才
物化）。另含 `GetProcessTimes` 进程 user/kernel CPU 快照（基准取窗口差值：
kernel 增量 ≈ NTFS 内核侧开销，user 增量 ≈ handler 逻辑）。

```bat
:: daemon 路径：停机时把累计报告 best-effort 写到 <log_dir>/vfs-profile.txt
set WTFS_VFS_PROFILE=1
wtfsd.exe ...

:: 基准/测试路径：4,874 文件夹具 status 冷/热窗口归因（ignored 集成测试）
set LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin
set WTFS_VFS_PROFILE=1
set WTFS_BENCH_REPO=<大仓库 clone>
set WTFS_BENCH_WTS=2
cargo test --release -p wtfs-daemon --test m13_profile -- --ignored --nocapture
```

### 归因口径

同一窗口内（profiler `reset()` 前后）取差值，三分：

1. **NTFS 元数据** = Σ`nt.*` 墙钟（次数 × 单次延迟，冷/热差异主要来源）；
2. **overlay 逻辑** = Σ`cb.*` − Σ`nt.*`（锁/哈希/内存索引）；
3. **WinFSP IPC/内核派发 + git 自身 CPU** = git status 墙钟 − Σ`cb.*`。

注意 git 的并行 index preload（`core.preloadindex`）会让多个回调在
dispatcher 线程上并发执行，Σ回调可能超过墙钟——比较占比时以次数与单次
均值为准，墙钟差值仅作量级参考。

### 实测：4,874 tracked files（git/git 夹具，status 窗口归因）

优化前（本 mission 起点）：

| 指标 | 数值 |
|---|---|
| git status 冷/热 | 2601 / 2564 ms（native 100-172 ms） |
| Σ回调墙钟 | 7.3 s（read_directory 占 5.2 s） |
| `ov.child_stat`（read_directory 逐子项 stat） | 22.6K 次 / 3.9 s，每子项 2 次 NTFS 探测 |
| `nt.is_dir_probe`（resolve 无条件第二探测） | 23.3K 次 / 1.1 s |
| NTFS 探测合计 | ~47K 次 / 2.4 s（并行下内核开销放大：daemon kernel +13 s） |
| meta 锁持锁探测的争用放大 | get_security_by_name/open 均值 3-4 ms |
| 实测噪音（不改） | `ov.map_to_lower` 11.5 ms、`ov.derive_ino` 8.2 ms（<0.5%） |

改动（全部由上表数据决定）：

- **resolve 元数据复用**（A+B）：`resolve_with_md` 一次 `symlink_metadata`
  同时判定 upper 文件/目录并回传给 stat 复用——upper 臂第二次 metadata 与
  `is_dir` 第二探测整体消失（M0 无 symlink，语义等价）；
- **`Overlay::list_stat`**（C）：父目录解析一次 + readdir 的 file_type/
  metadata 直接推导子项 stat，VFS `read_directory` 弃用逐子项
  `overlay.stat`；lower-only 目录 upper 侧缺失照常跳过（与 `list` 一致）；
- **`LowerLookup` 改 `Cow`**（D）：读路径 redirect 映射零堆分配
  （`Mapped` 仅迁移产物才分配）；
- `map_to_lower`/`derive_ino` 实测为噪音，仅做 Cow 卫生改造，不引入缓存
  （file_id 本就每次 stat 只哈希一次）。

优化后（同一夹具同一窗口）：

| 指标 | 优化前 | 优化后 |
|---|---|---|
| git status 冷 | 2601 ms | **139.7 ms** |
| git status 热（5 轮中位） | 2564 ms | **140.9 ms** |
| native 对照 | 100-172 ms | 100-172 ms（同轮 171/146 ms） |
| `ov.child_stat` / `nt.is_dir_probe` | 22.6K / 23.3K 次 | **0 / 0** |
| NTFS 探测合计 | ~47K 次 | ~1.1K 次 |
| 目标 status <1s | — | **达成（~0.14 s，≈native）** |

### 实测：10 worktree 全量基准（git/git 夹具 4,874 tracked files，10 轮中位）

2026-11-15，release 构建，`m0_benchmark` harness，来源
`wtfs-bench-large-m12/repo`（git/git @ `923ecfe`）：

| metric | native worktree | WorktreeFS |
|---|---|---|
| worktree add (median) | 2790 ms | 5913 ms |
| add + ready switch (median) | - | 5974 ms |
| disk: worktree tree (mean) | 49.27 MB (4875 files) | 49.27 MB (4875 files) |
| disk: private upper store (mean) | - | 89 B |
| **git status cold (median)** | 211 ms | **135 ms** |
| git status hot (median) | 54 ms | 137 ms |
| worktree remove (median) | 1322 ms | 6147 ms |
| one-off: template + mount | - | 3756 ms + 41 ms |

status 冷热全部 <1s（目标达成）：冷态 135ms 已优于 native（211ms，本轮
native 冷态方差大），热态 137ms（native 54ms 的 index 全热缓存更快，但绝对
值仍远低于目标）。M13 只动读路径：add/remove 仍走写入/删除路径（写入路径
优化与 remove/unlink 归 M14）；每 worktree 私有 upper 89 字节，M9 零
fsync/M12 零标记写入性质保持。

## 集成测试

- `cargo test --workspace`：单测 + 无挂载集成（无需 WinFSP 的 watcher 链路）。
- 真实挂载 E2E（需 WinFSP + git，默认 `#[ignore]`）：

```bat
set LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin
cargo test --workspace -- --ignored
```

覆盖三个场景：`wtfs-vfs` 挂载 + dedupe + 覆写回归；`wtfs-daemon` 完整 M0 验收
（index watcher → ready → status 干净 → 编辑/还原 → remove → 卸载 → gc）；
`worktree_lifecycle`（夹具仓库全生命周期：add → status 干净 → dedupe 上界 →
diff → add/commit → remove → 卸载 → gc）。
