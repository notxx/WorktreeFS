# WorktreeFS 实施计划

- 日期：2026-11-12
- 状态：Draft
- 范围：WinFSP 文件系统 + 可选 Git shim；M0 先做 dedupe-only，M1 起做 O(1) 创建
- 目标仓库：`WorktreeFS`

## 1. 背景

Kimi Code Tower 模式为每个 mission 创建一个独立 worktree：

- `TowerSpawn` 内部调用 `git worktree add <path> <branch>`，全量 checkout。
- worktree 位于 `<repo>/.tower/worktrees/wt-<n>`。
- merge/abandon 不删除 worktree，只有 `TowerTeardown` 删除已注册的 worktree；未注册的目录会被跳过。
- 实测 `dashboard` 仓库：790 个 tracked 文件，单次全量 checkout 约 1.22s / 45MB；277 个 mission 累计产生约 6.8G `.tower/worktrees`，其中约 6.4G 是 50 个 dead 目录（无 `.git`、未注册）。
- 主要浪费：重复 checkout 的磁盘/IO、未跟踪构建产物、dead 目录不回收。

本计划采用上一轮讨论确定的“口径”：

- **最终一个 WinFSP 挂载点覆盖整个 workspace**，不是每个 worktree 一个 mount。
- 普通路径 passthrough 到真实 workspace；`.tower/worktrees/*` 使用 COW overlay。
- **M0 先不拦截命令**：FS 挂 `<repo>/.tower/worktrees`，让 `git worktree add` 正常跑，在写入路径 dedupe；先验证 FS/overlay/index/磁盘收益。
- **M1 起 Git shim 拦截** `git worktree add`，调用 WorktreeFS 的 `worktree-add` API，使用 `git worktree add --no-checkout` + 模板 + overlay + index 改写，实现 O(1) 创建。
- 不依赖 Tower 吞掉 `git worktree add` 的报错；不使用 `PreToolUse` 预挂载。
- Kimi Code hooks 只用于清理/审计（`TowerMerge`/`TowerMission`/`TowerTeardown`），不参与创建。
- ReFS 仅作为可选后端（模板/upper 存储），不是前提。

## 2. 目标与非目标

### 2.1 目标

1. 最终一个挂载点覆盖整个 workspace，路径对 Tower/Git/构建工具透明。
2. **M0 先做 dedupe-only**：不依赖 hook/shim，`git worktree add` 正常跑，FS 在写入路径丢弃与模板相同的内容；目标是磁盘/写 IO 收益，不追求 O(1)。
3. **M1 起** worktree 创建从“全量 checkout”变成“元数据 + overlay + index”，目标 <300ms（790 文件基线）。
4. 未修改文件不复制、不重复写盘；磁盘只保留模板 + 每个 worktree 的改动。
5. `git status`/`git diff`/`git add`/`git commit`/`git checkout` 行为正确。
6. 构建/测试工具在 worktree 内正常工作（mmap、文件锁、通知、长路径）。
7. merge/abandon/teardown 后自动清理 overlay，dead 目录可 GC。
8. 崩溃后真实文件不丢；overlay 可恢复或安全丢弃。

### 2.2 非目标

- 不做 ReFS-only 的 clone 工具形态（B2）。
- 不做每个 worktree 一个 mount。
- 不支持远程 workspace（hooks/FS 都在本地）。
- v1 不支持 submodule 的共享元数据、ADS、reparse point、sparse index、split index。
- 不承诺构建产物（`target/`、CMakeCache 等）跨 worktree 共享；路径敏感产物仍可能重建。
- 不做通用文件系统产品；只服务 Tower 的 worktree 场景。

## 3. 关键决策

| 决策 | 选择 | 理由 |
|---|---|---|
| 挂载形态 | 最终一个 WinFSP volume 覆盖 workspace；M0 先挂 `.tower/worktrees` | 避免 per-worktree mount；先简化 |
| M0 挂载点 | `<repo>/.tower/worktrees` | 无 passthrough；主仓库保持原生 |
| M2+ 挂载点 | workspace 根（passthrough） | 覆盖主仓库/`node_modules`/审计 |
| 普通文件 | M0 不经过 FS；M2+ passthrough | 先简化，再扩展 |
| worktree 路径 | COW overlay（lower=模板，upper=私有） | 只存改动 |
| M0 创建入口 | 不拦截；`git worktree add` 正常跑，FS 写入路径 dedupe | 不依赖 hook/shim；先验证 FS |
| M1+ 创建入口 | Git shim 拦截 `worktree add` | O(1)；不依赖 Tower 吞错误 |
| M1+ 创建方式 | `git worktree add --no-checkout` + FS API + index 改写 | O(1)，不跑全量 checkout |
| 模板 | 每个 repo + base commit 一份正常 checkout + index 快照 | 复用 Git 的 filter/CRLF/LFS 结果 |
| index | M0 由 FS 控制 stat；M1+ 复制模板 index 并改写 | `git status` 不读全量内容 |
| 清理 | `wtfs gc` + 可选 Kimi Code hooks | 解决 dead 目录/未跟踪产物 |
| ReFS | 可选后端 | 不阻塞 v1；后续可优化 upper/模板 |

## 4. 总体架构

```
Kimi Code / Tower / git / build tools
        │  Win32 file API
        ▼
┌──────────────────────────────────────────────┐
│ WinFSP volume                                │
│  M0: <repo>/.tower/worktrees                 │
│  M2+: workspace root 或 W:\                  │
│  wtfs-vfs                                    │
│   ├─ namespace router                        │
│   ├─ passthrough backend (M2+ only)          │
│   └─ overlay backend (.tower/worktrees/wt-*) │
│        lower = template                      │
│        upper = per-worktree store            │
└──────────────────┬───────────────────────────┘
                   │
┌──────────────────▼───────────────────────────┐
│ wtfsd (Rust daemon)                          │
│  · mount lifecycle                           │
│  · template manager                          │
│  · overlay + SQLite metadata                 │
│  · dedupe / index coordinator                │
│  · named-pipe API                            │
│  · gc                                        │
└──────────────────┬───────────────────────────┘
                   │
┌──────────────────▼───────────────────────────┐
│ git shim (M1+ only)                          │
│  · intercept worktree add/remove             │
│  · delegate everything else to real git      │
└──────────────────────────────────────────────┘
```

### 4.1 路径路由

M0（mount 在 `<repo>/.tower/worktrees`）：

```
<mount>/
├─ wt-1/     → overlay
├─ wt-2/     → overlay
└─ ...
```

M2+（mount 在 workspace 根）：

```
<mount>/
├─ <repo>/                          → passthrough
│  ├─ .git/                         → passthrough（真实）
│  ├─ src/...                       → passthrough
│  └─ .tower/
│     ├─ comms/                     → passthrough（Tower 状态）
│     └─ worktrees/
│        ├─ wt-1/                   → overlay
│        └─ wt-2/                   → overlay
└─ ...
```

路由规则：

- M0：mount 根下所有 `wt-*` → overlay。
- M2+：路径匹配 `<repo>/.tower/worktrees/<wt>/...` → overlay；其他路径 → passthrough。
- 模板/upper 存储在 workspace 外（`%LOCALAPPDATA%\wtfs\...`），不暴露给 Git。

## 5. 组件

### 5.1 `wtfs-vfs`

- 实现 WinFSP `FileSystemContext`。
- 路径解析、路由、目录合并。
- passthrough 后端（M2+）：委托真实 workspace 文件。
- overlay 后端：lower/upper/whiteout/rename。
- dedupe：写入路径 hash/compare，丢弃与模板相同的内容（M0）。
- `get_file_info`、`read`、`write`、`read_directory`、`rename`、`cleanup`、`flush` 等回调。
- 通知：`FspFileSystemNotify`。
- 并发：`DashMap`/`RwLock`，每个文件句柄持有 `Arc<FileNode>`。

### 5.2 `wtfs-core`

- 模板管理：创建/更新/快照 index。
- overlay 元数据：SQLite WAL。
- copy-up、whiteout、rename map、file id、逻辑 mtime。
- dedupe：模板文件 hash、写入路径比较、lower/upper 决策。
- index 读写（M1+；优先 `gix-index`，必要时自写 v2）。
- 路径/大小写/保留名校验。

### 5.3 `wtfs-daemon`

- 启动/停止 mount。
- named pipe API（`\\.\pipe\wtfs`）。
- 模板/overlay 注册表。
- M0：监听 `<main>/.git/worktrees/<name>/index`，完成 `initializing → ready` 切换。
- GC：dead worktree、旧模板、孤儿 upper。
- 崩溃恢复：SQLite WAL、启动时对账。

### 5.4 `wtfs-cli`

```
wtfs mount [--root <path> | --drive W:]
wtfs unmount
wtfs template create --repo <path> --base <ref>
wtfs template update --repo <path> --base <ref>
wtfs worktree-add --repo <path> --path <path> --branch <branch> --base <ref>   # M1+
wtfs worktree-remove --repo <path> --path <path>
wtfs gc [--dry-run]
wtfs status
```

### 5.5 `wtfs-git-shim`

M0 不使用 shim；M1 起启用。

- 一个名为 `git.exe` 的 Rust 二进制，放在 PATH 前面。
- 解析 argv：
  - `worktree add ...` 且目标在 `.tower/worktrees/` → 调 `wtfs worktree-add`；成功则输出 Git 风格信息并 exit 0；失败则 exec 真实 git（全量 checkout 兜底）。
  - `worktree remove ...` → 先调 `wtfs worktree-remove`，再 exec 真实 git。
  - 其他命令 → exec 真实 git。
- 防递归：`WTFS_REAL_GIT` 环境变量或解析自身路径排除。
- 透明性：stdin/stdout/stderr、exit code、信号、cwd 透传。

## 6. 关键流程

### 6.1 挂载

M0（dedupe-only）：

1. `wtfsd` 启动，读取配置。
2. 确认 `<repo>/.tower/worktrees` 存在（TowerInit 创建）；不存在则创建。
3. 目录挂载到 `<repo>/.tower/worktrees`。
4. 加载模板/overlay 注册表；恢复未完成的 worktree。
5. 启动 named pipe；等待 CLI 请求。

M2+（workspace 根）：

1. 解析 workspace 根；校验目录存在。
2. 尝试目录挂载到 workspace 根。
   - 如果 WinFSP 要求空目录或行为不稳定，退回盘符 `W:`，workspace 映射为 `W:\workspace`。
3. 加载模板/overlay 注册表；恢复未完成的 worktree。
4. 启动 named pipe；等待 shim/CLI 请求。

**M0 必须验证**：`.tower/worktrees` 目录挂载、`git worktree add` 的空目录检查、index 监听/ready 切换。
**M2 必须验证**：WinFSP 目录挂载能否覆盖非空 workspace；不能则确定盘符方案和路径迁移策略。

### 6.2 模板创建/更新

```
wtfs template create --repo <repo> --base <ref>
```

1. `git -C <repo> worktree add --detach <template> <base>`（或 `git archive` + `checkout-index`）。
2. 可选：安装依赖/预热构建。
3. `git -C <template> update-index --refresh`。
4. 快照 index 到 `<template-store>/index`。
5. 删除模板中的 `.git` 文件（模板不再跑 Git）。
6. 设置只读 ACL。
7. 计算模板文件 hash（M0 dedupe 用），写入 `template_files` 表。
8. 写入 `templates` 表：repo、base commit、路径、index 路径、状态。

更新：

- base 前进时，`git -C <template> fetch/checkout/reset` 只写变化文件；刷新 index；原子切换。
- 模板更新期间加锁，避免 clone 读到半更新状态。

### 6.3 M0：Dedupe-only（无 hook/shim）

目标：先验证 FS + overlay + index + dedupe，不拦截 `git worktree add`。

1. FS 挂在 `<repo>/.tower/worktrees`（主仓库保持原生）。
2. Git 正常执行 `git worktree add <path> <branch>`。
3. FS 对 `wt-*` 维护状态机：
   - `initializing`：`readdir` 返回空，让 Git 通过“目录必须为空”检查。
   - Git 写 `.git`/文件：FS 逐文件 hash；与模板相同 → 丢弃，记 lower；不同 → 写 upper。
   - FS 对写入文件报告固定 stat（size 实际、mtime/ctime 固定 epoch、ino 路径哈希、dev 固定）。
   - Git 写 index（`<main>/.git/worktrees/<name>/index`，mount 外）：daemon 监听 index 写入/关闭。
   - index 完成后 → `ready`：视图切成“模板 + upper”。
4. 模板来源：
   - 读 `.tower/comms/state.json`：`mission.worktree == wt-N` → branch/base；`git rev-parse` 得 commit。
   - 读不到则用主仓库当前 base branch 的模板；只 dedupe 内容完全一致的文件。
   - 模板缺失/创建失败 → 全部写 upper（全量 copy），worktree 仍然正确。
5. index 对齐：
   - FS 在 checkout 期间控制 stat，Git 写进 index 的值与 ready 后 FS 报告的值一致。
   - daemon 在 worktree 创建后写 `config.worktree`：`core.checkStat=minimal`、`core.trustctime=false`、`core.untrackedCache=false`。
   - stat 对不上时允许 Git 首次全量哈希；正确性优先。
6. 失败兜底：
   - 模板/状态读取失败 → 全量 copy。
   - index/ready 切换失败 → 保持 upper 视图；worker 仍可工作。
   - 不允许 worker 在部分初始化的 worktree 上启动；必要时标记/清理。
7. 验收：
   - `git status` 干净。
   - 无改动 worktree upper <5MB。
   - 创建时间可接受（预计 1.5–3s，需实测）。
   - 不依赖 hook/shim。

### 6.4 M1+：Worktree 创建（O(1)）

M1 起，Git shim 收到：

```
git worktree add <path> <branch>
```

1. 判断 `<path>` 在 `<repo>/.tower/worktrees/` 下。
2. 解析 branch/base；读取 `.tower/comms/state.json` 辅助（可选）。
3. 调 `wtfs worktree-add`：
   - `git worktree add --no-checkout <path> <branch>` 创建 metadata 和 `.git`。
   - 在 `worktrees` 表注册：repo、path、branch、base、template、upper。
   - 创建 upper 目录；初始化 SQLite。
   - 复制模板 index；改写 stat；写入 `<main>/.git/worktrees/<name>/index`。
   - 设置 worktree config：
     ```
     core.checkStat = minimal
     core.trustctime = false
     core.untrackedCache = false
     core.splitIndex = false
     index.sparse = false
     ```
   - 返回成功。
4. shim 输出类似：
   ```
   Preparing worktree (new branch 'feat/...')
   HEAD is now at <sha> <subject>
   ```
   exit 0。

失败兜底：

- 任何一步失败 → 清理半成品 worktree/overlay → shim exec 真实 `git worktree add`（全量 checkout）。
- 不允许 worker 在部分初始化的 worktree 上启动。

### 6.5 读/写

- 读：upper 命中 → upper；否则 whiteout → 不存在；否则 lower；否则 passthrough（M2+）。
- 写：首次写触发 copy-up：lower 文件复制到 upper；之后写 upper。
- 删除：upper 删除；lower 存在则写 whiteout。
- 重命名：文件级 whiteout + copy-up/upper rename；目录级 rename map。
- stat：
  - lower：固定 dev/ino/mtime/ctime，size/mode 来自模板 index。
  - upper：真实/逻辑 mtime；稳定 file id。
- 目录枚举：lower + upper 合并，应用 whiteout/rename。
- 通知：变更后 `FspFileSystemNotify`。
- mmap/锁：WinFSP 原生支持；M2/M4 用真实工具验证。

### 6.6 删除/merge/abandon/teardown

- `git worktree remove`：
  - M1+：shim 调 `wtfs worktree-remove`：删除 overlay 记录、upper、whiteout/rename；保留模板。
  - M0：FS 直接处理删除；daemon 清理 overlay。
  - 再 exec 真实 `git worktree remove` 删除 Git metadata。
- `TowerMerge` 成功后：
  - 可选 Kimi Code hook（`PostToolUse`）调 `wtfs gc --merged`，删除已合并 worktree 的 overlay。
  - 不删除分支/commits；只释放工作区。
- `TowerMission status=abandoned`：
  - 可选 hook 调 `wtfs gc --abandoned`。
- `TowerTeardown`：
  - 可选 hook 调 `wtfs gc`。
- `wtfs gc`：
  - 扫描 `.tower/worktrees/*`，删除无 `.git`/未注册的 dead 目录（先 dry-run）。
  - 删除无引用的 upper/旧模板。
  - 不碰 `dashboard-fork-*` 等非 Tower 目录。

## 7. Index 算法

### 7.1 输入

- 模板 index（`<template-store>/index`）。
- 模板文件 stat（size/mode）。
- 虚拟 volume serial、路径 ID 映射。

### 7.2 输出

- worktree index（`<main>/.git/worktrees/<name>/index`），v2，无扩展（M1+）。
- M0：不写 index；FS 控制 stat，让 Git 写出的 index 与 FS 报告一致。

### 7.3 步骤

1. 用 `gix-index` 读取模板 index（M1+）。
2. 对每个 entry：
   - 保留 `oid`、`size`、`mode`、`path`。
   - `dev` = 固定虚拟 volume serial。
   - `ino` = 稳定路径 ID（持久化映射；32 位）。
   - `mtime`/`ctime` = 固定 epoch（例如 2000-01-01）。
   - `uid`/`gid` = 0。
3. 写入 v2；丢弃 `TREE`/`UNTR`/`FSMN`/`link`/`sdir` 扩展。
4. FS 对 lower 文件报告同一组 stat。
5. `core.checkStat=minimal`、`core.trustctime=false`。
6. 固定时间戳避开 racy-git。
7. M1+：只在新 worktree 创建时写 index；之后由 Git 维护，不覆盖。
   M0：不写 index；FS 在 checkout 期间控制 stat，让 Git 写出的 index 与 FS 报告一致。

### 7.4 兜底

- 如果 index 改写失败或 stat 对不上：允许 Git 首次 `status` 全量哈希并回写；正确性优先。
- 如果模板 index 缺失：从模板重新 `git update-index --refresh` 生成；警告可能丢失 staged changes。

## 8. Overlay 与元数据

### 8.1 存储布局

```
%LOCALAPPDATA%\wtfs\
├─ repos\<repo-id>\
│  ├─ templates\<commit>\
│  │  ├─ tree\            # 模板 checkout
│  │  └─ index            # 模板 index 快照
│  └─ worktrees\<wt-id>\
│     ├─ upper\           # 改动文件
│     └─ meta.db          # SQLite WAL
└─ logs\
```

### 8.2 SQLite 表

```sql
repos(id, root, common_git_dir, created_at);
templates(id, repo_id, base_commit, tree_path, index_path, state, created_at);
template_files(template_id, path, hash, size, mode, PRIMARY KEY(template_id, path));
worktrees(id, repo_id, path, branch, base_commit, template_id, upper_path, status, created_at);
dedupe(worktree_id, path, state, PRIMARY KEY(worktree_id, path));
whiteouts(worktree_id, path, PRIMARY KEY(worktree_id, path));
renames(worktree_id, from_path, to_path, PRIMARY KEY(worktree_id, from_path));
file_ids(worktree_id, path, id, PRIMARY KEY(worktree_id, path));
stat_cache(worktree_id, path, mtime_ns, size, flags);
```

### 8.3 一致性

- SQLite WAL；写操作事务化。
- upper 文件是数据真相；启动时扫描 upper 与 DB 对账。
- whiteout/rename 丢失会导致被删 lower 文件重新出现；可接受，不静默丢数据。
- 逻辑 mtime 单调前进，避免构建工具因 mtime 倒退漏重建。
- M0 dedupe 失败时保留 upper 文件；正确性优先。

## 9. Git shim 细节

M0 不使用 shim；M1 起启用。

### 9.1 安装

- `wtfs shim install` 把 shim 目录写到用户 PATH 前面，或生成 `kimi.cmd` 包装器：
  ```
  set PATH=C:\...\wtfs\shim;%PATH%
  kimi %*
  ```
- 验证 `where git` 第一条是 shim。

### 9.2 解析

- 只拦截 `git worktree add` / `git worktree remove`。
- 目标路径必须在 `<cwd>/.tower/worktrees/` 或 `<repo>/.tower/worktrees/` 下。
- 其他 `worktree` 子命令（`list`、`prune`、`repair`、`lock`）透传。
- 非 worktree 命令透传。

### 9.3 兜底

- `wtfs worktree-add` 失败 → 清理 → exec 真实 git 原始参数。
- 真实 git 路径通过 `WTFS_REAL_GIT` 或安装时探测；避免递归。
- shim 不改变 stdout/stderr 语义；Git 风格输出。

### 9.4 备选：源码 patch

如果 shim 在某些环境不稳定，改用 kimi-code 源码：

- 在 `packages/agent-core-v2/src/features/tower/protocol/git.ts` 的 `worktreeAdd`/`worktreeAddNewBranch` 里调用 `wtfs worktree-add`。
- 保留 `git worktree add` 兜底。
- 优点：无 PATH 依赖；可直接拿到 mission/branch/base。
- 缺点：维护 fork。

## 10. Tower 集成

### 10.1 创建

M0：

- 不拦截；Tower 正常 `git worktree add`，FS 在写入路径 dedupe。
- 如果 FS 未挂载/模板缺失，退化为全量 copy；功能正确，只是没优化。

M1+：

- shim 拦截 `TowerSpawn` 内部的 `git worktree add`。
- 不需要 `PreToolUse` 预建；不需要依赖 Tower 吞错误。
- 如果 shim 未生效，Tower 走普通全量 checkout；功能正确，只是没优化。

### 10.2 清理（可选 hooks）

Kimi Code hooks 只做观察/清理：

```toml
[[hooks]]
event = "PostToolUse"
matcher = "TowerMerge"
command = "C:/tools/wtfs.exe hook-merge"
timeout = 600

[[hooks]]
event = "PostToolUse"
matcher = "TowerMission"
command = "C:/tools/wtfs.exe hook-mission"
timeout = 60

[[hooks]]
event = "PostToolUse"
matcher = "TowerTeardown"
command = "C:/tools/wtfs.exe hook-teardown"
timeout = 600
```

- `hook-merge`：读取 `tool_input.branch`，映射 mission/worktree，删除已合并 overlay；更新模板到新 base。
- `hook-mission`：`status=abandoned` 时删除 overlay。
- `hook-teardown`：gc dead 目录/旧模板。
- 这些 hook 失败不影响主流程（fail-open）。

### 10.3 不做的

- 不用 `PreToolUse` 预挂载。
- 不用 `PostToolUse` 替换已启动 worker 的 worktree。
- 不修改 Tower 的 merge gate/review 逻辑。

## 11. 兼容性

### 11.1 v1 支持

- 普通文件、目录。
- symlink 作为普通文件（`core.symlinks=false`）。
- CRLF/`.gitattributes` filter/LFS：模板是正常 checkout，已由 Git 处理；FS 原样服务。
- `git status/diff/add/commit/checkout/reset/stash`。
- `git worktree add/remove/list`。
- `core.checkStat=minimal`、`core.trustctime=false`。
- 长路径（`core.longpaths=true`）。

### 11.2 v1 不支持（需检测并回退）

- submodule：模板里的 `.git` 指向共享 gitdir，agent 写入可能污染模板。检测到 `.gitmodules` 时回退普通 `git worktree add`，或后续做 per-agent submodule metadata。
- ADS、reparse point、hardlink。
- sparse index、split index、sparse-checkout。
- 大小写冲突、Windows 保留名、非 UTF-8 路径：模板创建时检测并报错。
- 远程 workspace。

### 11.3 构建产物

- 模板可选包含 `node_modules`/依赖；Node worktree 嵌套在 repo 下，会向上解析 `node_modules`，通常不需要 per-worktree 安装。
- Rust `target/`、CMakeCache 等路径敏感；不承诺共享；用 `sccache`/共享 registry。
- 模板中的构建产物可能因绝对路径触发重建；benchmark 后决定是否纳入模板。

## 12. 性能目标

| 指标 | 目标 | 基线 |
|---|---|---|
| M0 dedupe 创建（790 文件） | 1.5–3s（待测） | 1.22s 全量 checkout |
| M1+ O(1) 创建（790 文件） | <300ms | 1.22s 全量 checkout |
| 无改动 worktree upper | <5MB | 45MB 全量 checkout |
| `git status` 热 | <100ms | 普通 worktree 同级 |
| `git status` 冷 | 首次 stat 通过 | 不读全量内容 |
| 读吞吐 | 原生 80%+ | NTFS 基线 |
| 构建时间 | 原生 +20% 内 | 需实测 |
| 内存 | 模板 stat 表共享 | 每 worktree 仅 upper 元数据 |

基准：

- 创建时间、磁盘、`git status`、`git diff`、`git checkout`、真实 build/test。
- 对比普通 worktree、ReFS clone、WorktreeFS。

## 13. 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| M0 `.tower/worktrees` 目录挂载/空目录检查 | Git 拒绝或行为异常 | M0 验证；必要时改用盘符/子目录 |
| M0 dedupe 写路径/ready 切换 | worker 看到半成品/性能下降 | index 关闭作为 ready 信号；失败保持 upper；benchmark |
| M0 无 shim 兜底 | 模板/状态失败时退化为全量 copy | 模板缺失/失败时全量 copy；index 不匹配时允许哈希 |
| M2 WinFSP 目录挂载非空 workspace 不稳定 | 无法透明挂载 | M2 验证；退回盘符 `W:` |
| passthrough 语义不完整（mmap/锁/通知/安全） | 构建/工具异常 | 用真实工具矩阵测试；必要时缩小范围 |
| index stat 对不上 | `git status` 全量哈希 | 固定 stat + `core.checkStat=minimal`；兜底 |
| shim 透明性/PATH/递归 | Git 命令异常 | 严格透传；`WTFS_REAL_GIT`；集成测试 |
| FS daemon 崩溃 | workspace 不可用 | 自动重启/重挂载；真实文件不动；告警 |
| submodule/LFS/filter | 内容错误 | 模板正常 checkout；submodule 回退 |
| 模板更新竞态 | worktree 基于旧 base | 模板锁；创建前校验 base |
| 脏 base 的 `spawnBase` | 模板不含 WIP | 回退普通 checkout，或按 spawnBase 建临时模板 |
| 构建产物路径敏感 | 重建/磁盘增长 | 不承诺共享；用生态缓存 |
| 杀软/EDR | 性能下降 | 排除挂载点/模板；Dev Drive 可选 |
| WinFSP license | 分发限制 | GPLv3 + FLOSS 例外；商业分发需 license |

## 14. 里程碑

### M0：Dedupe-only FS（2–3 周）

- FS 挂 `<repo>/.tower/worktrees`；主仓库保持原生。
- `git worktree add` 正常跑；FS 写入路径 dedupe（hash vs 模板）。
- 模板创建/更新 CLI；从 `.tower/comms/state.json` 解析 base。
- FS 控制 stat；daemon 监听 index；`initializing → ready` 切换；`git status` 干净。
- 失败退化为全量 copy。
- 验收：无改动 worktree upper <5MB；创建时间实测；不依赖 hook/shim。

### M1：O(1) 创建（2–4 周）

- Git shim + `wtfs worktree-add`。
- `git worktree add --no-checkout` + overlay + index。
- `worktree-remove`、merge/abandon/teardown 清理；`wtfs gc`。
- 失败回退普通 checkout。
- 验收：790 文件仓库创建 <300ms；`git status` 干净。

### M2：Workspace 根挂载与主仓库覆盖（3–4 周）

- 扩展到 workspace 根：passthrough 普通路径；`.tower/worktrees/*` overlay。
- 覆盖主仓库 `node_modules`/`.git`/`.tower/comms`。
- 验证目录挂载 vs 盘符 `W:`；passthrough 的 mmap/锁/通知/安全。
- 验收：主仓库原生行为不退化；worktree 优化仍生效。

### M3：Audit、Trash 与 Mass-deletion（2–4 周）

- `audit_events`：PID/进程名/路径/操作/时间；`wtfs audit` 查询。
- 受保护路径（`node_modules`、`.git`、`.tower/comms`）；删除进 trash；`wtfs trash list/restore`。
- mass-deletion 阈值/告警/事件流；可选 Kimi Code hooks 做清理。
- 验收：能定位误删进程；可恢复；不破坏合法 `npm install`/`git clean`。

### M4：性能、兼容性与打包（2–3 周）

- mmap、文件锁、通知、长路径、大小写；真实 build/test。
- 性能基准与优化（缓存、批量 stat、copy-up chunk）。
- 安装器/服务/托盘；配置/日志/诊断；WinFSP license/分发检查。
- 验收：构建时间可接受；工具矩阵通过；可安装/回滚。

## 15. 验证

### 15.1 单元测试

- 路径路由、overlay、whiteout、rename、file id。
- dedupe：模板 hash、写入比较、lower/upper 决策。
- index 读写/stat 改写（M1+）；M0 stat 控制。
- 模板/GC。

### 15.2 集成测试

- 小仓库：`git status` 干净、`git diff`、`add/commit`、`checkout/reset`。
- `dashboard`：worktree 创建、真实 build/test、merge/teardown。
- M0：不装 shim/hook，`git worktree add` 正常跑；验证 dedupe/ready/index。
- M1：shim 所有 Git 命令透传；`worktree add/remove` 拦截；失败回退。
- `git fsck`。
- 崩溃：kill daemon mid-write；重挂载；验证真实文件/overlay。

### 15.3 验收标准

1. M0：不依赖 hook/shim；`git status` 干净。
2. M0：无改动 worktree upper <5MB；创建时间实测并记录。
3. M0：模板/状态失败时退化为全量 copy，worktree 仍可用。
4. M1：worktree 创建 <300ms（790 文件）。
5. `git diff`/`git add`/`git commit` 正确。
6. 真实 build/test 通过。
7. merge/abandon/teardown 后 overlay 清理；dead 目录可 GC。
8. daemon 崩溃后真实 workspace 不丢数据；可恢复。
9. 与普通 worktree 对比基准达标。

## 16. 待确认

1. M0 挂载点：`<repo>/.tower/worktrees`（建议）还是直接 workspace 根？
2. M0 dedupe 策略：先 post-hoc（写 upper 后再删相同文件，简单）还是流式丢弃（省写 IO，复杂）？
3. M1 集成：Git shim 还是 kimi-code 源码 patch？
4. 模板是否包含 `node_modules`/构建产物？
5. submodule 策略：回退普通 checkout，还是做 per-agent metadata？
6. 是否支持多 repo workspace？M0 先单 repo，M2 预留多 repo。
7. WinFSP 分发方式：内部工具（GPL/FLOSS）还是商业产品（需 license）？

## 附录 A：`wtfs worktree-add` 请求/响应

> 注：M0 不调用 `worktree-add`；M1 起 shim 使用此 API。

请求：

```json
{
  "repo": "C:/Users/notxx/Documents/workspace/dashboard",
  "path": "C:/Users/notxx/Documents/workspace/dashboard/.tower/worktrees/wt-278",
  "branch": "feat/example-m278",
  "base": "dashboard",
  "spawn_base": null
}
```

响应：

```json
{
  "ok": true,
  "worktree": "wt-278",
  "template": "dashboard@<sha>",
  "index": "C:/.../.git/worktrees/wt-278/index",
  "elapsed_ms": 180
}
```

失败：

```json
{
  "ok": false,
  "code": "template-missing",
  "message": "...",
  "fallback": "git worktree add"
}
```

## 附录 B：目录结构

```
WorktreeFS/
├─ Cargo.toml
├─ crates/
│  ├─ wtfs-vfs/        # WinFSP FileSystemContext
│  ├─ wtfs-core/       # overlay/template/index/metadata
│  ├─ wtfs-daemon/     # mount/pipe/gc
│  ├─ wtfs-cli/        # wtfs.exe
│  └─ wtfs-git-shim/   # git.exe wrapper（M1+）
├─ specs/
└─ tests/
```
