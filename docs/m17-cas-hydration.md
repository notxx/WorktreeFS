# M17/B170100：CAS manifest 与 hydration core（C 方案库层）

C 方案的定义（用户原话："整个 workspace 住在 FS 自己的存储里，有虚拟命名
空间/CAS/overlay；路径是 FS 合成的，文件按内容寻址，按需物化"）。本文是
M17 的实现报告：架构、Git-object CAS 取舍、索引种子 spike 的方法与代价、
core 基准数字。挂载（WinFSP）在 M18/B180100 接入。

## MVP 边界（与 B1 公平对比的前提）

* **CAS 直接复用 Git 对象库**：blob 已内容寻址且全局去重，不另建独立
  CAS 存储。读取走常驻 `git cat-file --batch`（单子进程 + Mutex 串行化
  协议），落盘缓存按 oid 分片。
* **虚拟命名空间 = 每 worktree 一份 manifest**：`git ls-tree -r -l -z
  <commit>` 一次解析为 path → (oid, mode, size)，派生目录集合与每目录
  子项索引；打开 O(文件数)，stat/list 纯内存查询。
* **挂载点约定不变**：仍是 `<repo>/.tower/worktrees`，主仓库保持原生
  （spec §6.3），保证与 B1 同夹具、同挂载点对比。
* **本 mission 不写 WinFSP**：交付库 + 测试 + core 基准；stat/list/read/
  write/remove 是纯 Rust API（`wtfs_cas::CasView`）。
* **写路径 MVP 恒落 upper**：不做写路径内容 dedupe（B1 的 ToLower 回落
  是它的差异化路径，C 的 M18 形态再议）；目录级 rename 默认 EXDEV（与
  M12 后的 B1 相同）；redirect 标记不实现（无 v3 迁移负担）。
* **不依赖 wtfs-core**：新 crate 独立（C 与 B1 分离评测、互不污染）；
  常量与派生算法逐字镜像（`FIXED_STAT_EPOCH_UNIX_NS`、`file_id` 的
  blake3(小写路径) 62 位 ino、`validate_relative_path` 的 spec §11.2
  规则），保证对比口径一致、后续可统一。

## 架构

```text
crates/wtfs-cas/
├─ cat_file.rs    # git cat-file --batch 常驻读取器（按 oid 取 blob）
├─ manifest.rs    # ls-tree -r -l -z 解析 → path→(oid,mode,size) + 目录/子项索引
├─ cache.rs       # <store>/cas-cache/<aa>/<oid> 分片缓存（temp+fsync+rename）
├─ markers.rs     # whiteout 日志（复用 M12 语义与记录格式）
├─ view.rs        # CAS 视图：upper > whiteout > manifest；stat/list/read/write/remove
└─ index_seed.rs  # 索引种子：read-tree + skip-worktree（+ gix-index stat 对齐）
```

* **stat 规则**（与 B1 逐字对齐）：manifest 文件固定 dev/ino/mtime/ctime
  （size/mode 来自 manifest，mtime = 2000-01-01 固定纪元）；upper 文件真实
  mtime/size（mode 取 manifest 可执行位，upper-only 默认 0o100644）；目录
  0o040755（有 upper 用真实 mtime，否则固定纪元）；ino = blake3(小写路径)
  62 位，根目录固定 1。whiteout/not-found → None。
* **read**：upper 命中读真实文件；manifest 命中经缓存 `ensure`（未缓存 →
  cat-file 读 blob → 同分片 temp 写 → fsync → 原子 rename）后读缓存文件
  ——"读到即已物化"，未读路径零对象 I/O。
* **remove**：upper 删真实文件/目录；manifest 有内容（自身或子树）则记
  whiteout。纯 manifest 删除零 upper 文件操作、零对象读取（删除不
  hydrate）。
* **gitlink（160000）**：对象库无对应 blob，读报 NotFound（size 列为
  `-`，按 0 记账）；symlink（120000）按 blob 内容读出（链接目标字节）。
* **M14 式 upper 负存在缓存未移植**：MVP 每次解析一次真实探测
  （`symlink_metadata`）；B1 的 M14 收益在逐回调探测序列上，core 基准
  （stat 4,874 文件）不受影响，M18 按需移植。

## 索引种子 spike（C 可行性的关键，30 分钟时间盒内定稿）

问题：`git worktree add --no-checkout` 之后，如何让 `git status` 干净且
不 hydrate 全部文件？

| 路线 | 做法 | 结果 |
| ---- | ---- | ---- |
| **route A（选定）** | `read-tree <commit>` → `ls-files -z` → `update-index -z --skip-worktree --stdin` 批量置位 | **status 干净**、磁盘零文件、零对象读取（demo 10 文件与 4,874 文件夹具实测） |
| route B（对照，弃用） | 完整 checkout 一次 → 复制 `<gitdir>/index` → 删树 | 未挂载 status **必报全量 ` D` 删除**（index stat 对齐的是已删除的真实文件）；叠加 skip-worktree 才干净 = route A + 一次无谓 checkout；其 stat 是 checkout 时刻的真实 mtime，与 CAS 固定纪元不对齐，挂载态仍需改写 |

结论：**未挂载的干净 status 只能靠 skip-worktree 位**（git 对该位条目跳过
工作树 stat，sparse-checkout 的同款机制），与 index 来源无关；因此 route A
严格占优——零 checkout、零文件 I/O，三个 git 子进程 + 一次 index 重写。

**mtime/stat 方案**：`seed_stat_aligned` 在 read-tree 之后用 gix-index 把
每条 entry 的 stat 字段改写为 manifest 值（dev=卷序列号、ino=file_id 截
32 位、size=manifest、mtime=ctime=固定纪元 946684800s），并置 skip-worktree
位。**挂载态切换（M18）**：FS 报告 manifest stat 后批量清除 skip-worktree
位（`clear_skip_worktree`，一条 `update-index --stdin`），status 干净性由
"FS stat == index stat" 保证——与 B1 模板 index 机制同构。

实现细节（评审注意）：skip-worktree 是高位标志（1<<30），gix-index 落盘
要求同条目置 `EXTENDED`（1<<14）且 index 升 v3（writer 检测 EXTENDED 自动
选择）；漏置 EXTENDED 会被静默丢位（已由 `stat_aligned_seed` 测试锁住）。

**语义边界**：全树 skip-worktree 属于超范围使用——`git add`/`git stash`
等对 skip-worktree 路径按 sparse 规则跳过。M18 挂载后必须在入口批量清位
（成本见基准"挂载态切换"行）；未挂载窗口内用户 git 操作只保证 status/
log/diff-tree 等不触碰工作树的命令正确。

## 基准（release，synthetic 4,874 文件夹具）

harness：`tests/cas_bench.rs`（`#[ignore]`，与 m0_benchmark 同一 markdown
报告约定；`WTFS_BENCH_REPO` 可指向真实 4,874 文件仓库，缺省生成每文件
内容唯一的合成仓库——内容唯一保证 blob 不去重、hydrate 路径不失真）。
报告尾部自检断言：route A 种子后 status 0 行、种子全程零 hydrate、
stat 对齐种子后 status 0 行。

### 基准记录（2026-11-15，Windows，release，3 迭代取中位数）

| 指标 | 值 |
| ---- | -- |
| 来源仓库 | synthetic/4874（setup 22.0s，一次性） |
| manifest 加载（`ls-tree -r -l -z` + 解析 + 目录/子项索引） | **592.0 ms**（4,874 文件，579.6 KB） |
| stat 全部文件（4,874 个，逐个 `view.stat`） | **161.7 ms**（33.2 µs/个） |
| list_stat 全树遍历（26 目录，4,899 条目，每目录一次 readdir） | **2.8 ms** |
| hydrate 500 文件冷态（cat-file + temp+fsync+rename） | **2,398.8 ms**（~4.8 ms/文件，fsync 主导；59.0 KB） |
| hydrate 500 文件热态（全命中，零对象读） | **42.4 ms**（~85 µs/文件，纯探测） |
| 种子 route A（read-tree + 批量 skip-worktree） | **88.2 ms**（read-tree 38.6，置位 49.6）→ status 干净（0 行，32.3 ms） |
| 种子 route A 完整形态（+ gix-index stat 对齐） | **60.3 ms**（read-tree 34.3，改写 25.9）→ status 干净 |
| 挂载态切换（批量清 4,874 个 skip-worktree 位） | 48.5 ms → 未挂载 status 4,874 行（预期全量删除） |
| 对照 route B（全量 checkout → 复制 index → 删树） | checkout **2,735.7 ms** + 删树 939.9 ms → 未挂载 status 4,874 行（必脏，弃用） |
| 种子期间 hydrate | **0 次**（misses 前后差值断言） |

读法：

* **route A vs route B 头条对比**：88 ms（零文件 I/O）vs 2,736 + 940 ms
  （两次全树物理写读删）——C 方案每 worktree 冷启的下限比"checkout 一次"
  低一个数量级以上，且随文件数线性劣化的只有 index 重写，不是文件 I/O。
* stat 34 µs/个的上限构成 = upper 探测（NT `symlink_metadata`，本夹具
  upper 为空，全部走"查无"分支）+ blake3 ino 派生；`list_stat` 批量口径
  每目录一次 readdir，全树仅 2.8 ms——挂载后 git 的 status 走 index-stat
  对齐路径，FS 侧成本按 list_stat 口径估算。
* hydrate 冷态 ~4.8 ms/文件被 fsync 主导（与 B1 的 M9 纪律一致：每物化
  文件恰好一次 fsync）；热态 85 µs/文件 = 纯存在探测。缓存体量 = 已读
  blob 字节数（59 KB/500 文件，本夹具小文件；真实仓库按
  `WTFS_BENCH_REPO` 实测）。

## Git-object CAS 取舍

**得**：
* 零存储工程：对象库已有内容寻址、全局去重、压缩（zlib）、fsck/_gc、
  传输（fetch/push 直接复用）；新 CAS 要重造的每一层都免费获得。
* manifest 与对象库天然一致（同 commit），无模板同步窗口；换 base 只需
  重读 ls-tree。
* 去重跨 worktree 全局生效：同 blob 只存一份、缓存按 oid 全局共享。

**失/边界**：
* 读路径多一跳子进程协议（常驻后 µs 级，见基准 hot 行）；大文件一次性入
  内存（流式接口 M18 随 VFS 读回调再议）。
* 写路径内容不回流对象库（MVP）：upper 文件与 CAS 双存；M18 若做"写入
  blake3→回查"也只能对 git 已写入的对象生效（`git hash-object -w` 需要
  追加写对象库的授权与撕裂缝合语义——超出 MVP）。
* gitlink/空洞对象语义要在视图层显式建模（已做：读报 NotFound）。

## 与 B1 的核心差异（公平对比口径）

| 维度 | B1（overlay + 模板） | C（CAS + manifest，本 mission） |
| ---- | -------------------- | ------------------------------ |
| lower 数据 | 模板 checkout（SQLite 清单 + 物理树） | Git 对象库（manifest + cat-file） |
| 模板创建 | 每 commit 一次全量 checkout + refresh | 无（ls-tree O(文件数) 一次） |
| 每工作树冷启 | 注册 + 挂载（模板树共享） | manifest 装载 + 索引种子（零文件 I/O） |
| 读未物化文件 | 直接读模板树（物理文件） | hydrate（cat-file → 缓存 → 读） |
| 删除 | whiteout（M12 日志） | 相同（复用 M12 语义） |
| stat 规则 | 固定纪元 + blake3 ino | 相同（逐字镜像） |
| 索引对齐 | 模板 index 经 FS refresh | read-tree + gix-index 改写 |

## 运行

```bat
:: 全部测试（不挂载，纯库）
cargo test -p wtfs-cas

:: core 基准（release）
cargo test --release -p wtfs-cas --test cas_bench -- --ignored --nocapture
```
