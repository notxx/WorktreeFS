# Worktree backend 探测：`wtfs detect`

日期：2026-11-19
目的：让 Kimi Code 只负责“找到 wtfs 可执行文件”，**是否适用于当前项目由 wtfs 自己判断**。

## 1. 为什么探测放在 wtfs 里

Kimi Code 不应该知道：

- 当前目录是不是 Tower workspace；
- `.tower/comms` / `.tower/worktrees` 在不在；
- store root、mount point、WinFSP 依赖；
- linked worktree 的主仓库根怎么解析。

这些都属于 wtfs 的领域。Kimi Code 只需要：

1. 启动时找到 `wtfs`（`WTFS_BIN` / 安装目录 / PATH）；
2. 对当前项目调用 `wtfs detect --json`；
3. `supported=true` → 用 wtfs backend；否则用 git。

## 2. 命令

```bash
wtfs detect [--repo <path>] [--store-root <path>] [--json]
```

- `--repo` 省略：用当前目录；
- 从当前目录向上跑 `git rev-parse --show-toplevel`；
- 用 `git rev-parse --path-format=absolute --git-common-dir` 找主仓库
  （当前目录在 linked worktree 内也能解析）；
- 检查主仓库的 `.tower/comms` 或 `.tower/worktrees`（只认真正的 Tower
  workspace，避免 `.tower` 临时残留误判）；
- 只读，不 mount、不创建目录。

### 输出（`--json`）

```json
{
  "supported": true,
  "reason": null,
  "repo_root": "C:/Users/notxx/Documents/workspace/WorktreeFS",
  "git_common_dir": "C:/Users/notxx/Documents/workspace/WorktreeFS/.git",
  "tower_dir": "C:/Users/notxx/Documents/workspace/WorktreeFS/.tower",
  "store_root": "C:/Users/notxx/AppData/Local/wtfs/repos/f3f65b219ee7beb3",
  "mount_point": "C:/Users/notxx/Documents/workspace/WorktreeFS/.tower/worktrees",
  "version": "0.1.0"
}
```

`supported=false` 时 `reason` 是机器可读的短字符串：

- `not-windows`
- `repo-path-not-found`
- `not-a-git-repo`
- `tower-workspace-missing`

## 3. Kimi Code backend 侧

启动时：

```ts
const wtfs = findWtfs(); // WTFS_BIN -> 安装目录 -> PATH
const backend = process.env.KIMI_WORKTREE_BACKEND ?? "auto";
```

每个 repo 缓存一次探测结果：

```ts
const detect = wtfs
  ? JSON.parse(execFileSync(wtfs, ["detect", "--repo", repo, "--json"]))
  : null;

if (detect?.supported) {
  // wtfs backend
} else {
  // git backend
}
```

- 探测失败/超时/文件找不到 → 静默回落 git；
- `KIMI_WORKTREE_BACKEND=git` 强制禁用；
- `KIMI_WORKTREE_BACKEND=wtfs` 强制启用（失败按 worktree 回落）；
- 启动日志只打一行：
  ```
  worktree backend: auto -> wtfs (repo=..., store=...)
  worktree backend: auto -> git (reason=tower-workspace-missing)
  ```

## 4. 执行命令（已接）

```bash
wtfs worktree-add  --repo <repo> --name <wt-N> --base <commit> [--branch <b>] [--create-branch]
wtfs worktree-remove --repo <repo> --name <wt-N>
```

`worktree-add` 的实际流程：

1. 确保 daemon/mount 在线（不在则 `wtfs mount`）；
2. `git worktree add --no-checkout` 到 store 的 `cas-wt/<name>`（挂载点外）；
3. 通过 daemon 管道 `RegisterCas` 注册 CAS 视图，`absorbing=true`；
4. 在虚拟路径 `<repo>/.tower/worktrees/<name>` 上 `git reset --hard`；
5. `FinishCas` 关闭吸收窗口，校验 `git status` 干净；
6. 失败时 `RemoveCas` + `git worktree remove --force` 清理。

`worktree-remove`：`RemoveCas` → `git worktree remove --force` → `prune`。

已实测（scratch repo，3 文件）：

- `wtfs worktree-add` 成功，虚拟路径 `.tower/worktrees/wt-test4` 可见 `.git`；
- `git rev-parse --show-toplevel` = 虚拟路径，`git status` 干净，分支 `test-branch4`；
- real worktree 在 store 的 `cas-wt/wt-test4`，upper 很小。

期间修了一个真实 bug：daemon watcher 可能先 `auto_register` 了同名 B1
overlay；`register_cas` 原来遇到已有 B1 会“返回新 backend 但不替换路由”，
导致虚拟视图仍是 B1（没有 `.git`）。现在显式 CAS 注册会覆盖 B1 路由。

## 5. Kimi Code bundle 补丁（已装，未跑完整 Tower 会话）

已在本机 Kimi Code 2.5.0 的 `dist/main.mjs` 打补丁：

- `worktreeAdd` / `worktreeAddNewBranch` / `worktreeRemove` 在
  `KIMI_WORKTREE_BACKEND=auto|wtfs` 且 `wtfs detect` 成功时调用
  `wtfs worktree-add/remove`，失败回落 git；
- 备份：`dist/main.mjs.wtfs-backup`。

`kimi --version` 仍正常。完整 Tower 会话没有跑：Tower 模式只能由用户在 TUI
里 `/tower on` 开启，`kimi -p` 的非交互模式不接受该 slash command。交互测试步骤：

```powershell
$env:KIMI_CODE_EXPERIMENTAL_TOWER = "1"
$env:KIMI_WORKTREE_BACKEND = "auto"   # 或 wtfs
$env:WTFS_BIN = "C:\path\to\wtfs.exe"
kimi
# 在 TUI 输入 /tower on，然后让主 agent 创建 worker
```

观察点：`wtfs status --json` 出现 worktree，虚拟路径有 `.git`，`git status`
干净，upper 很小；失败时日志应有 `wtfs worktree-add failed, falling back to git`。

## 5. 实测

- WorktreeFS 仓库（有 `.tower/comms`）：`supported=true`，store/mount 正确；
- dashboard（有 Tower workspace）：`supported=true`；
- 普通临时目录：`supported=false`，`reason=not-a-git-repo`；
- `.tower` 临时残留但无 `comms/worktrees`：`supported=false`，
  `reason=tower-workspace-missing`。

## 6. 提交

- `crates/wtfs-cli/src/lib.rs` 新增 `detect` 子命令；
- `cargo test -p wtfs-cli` / `cargo check --workspace` 通过。
