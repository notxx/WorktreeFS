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

## 4. 还没做的部分

`wtfs detect` 只回答“这个项目能不能用 wtfs”。真正执行 worktree 创建/删除的
CLI 命令还没接：

```bash
wtfs worktree-add  --repo <repo> --path <path> --branch <branch> --base <commit>
wtfs worktree-remove --repo <repo> --path <path>
```

`worktree-add` 需要：

1. `git worktree add --no-checkout`；
2. 加载 manifest；
3. seed 或注册 absorbing 视图 + `reset --hard`；
4. 通过 daemon 管道注册 CAS 视图；
5. 清 skip-worktree，校验 status；
6. 失败回落真实 `git worktree add`。

这一步完成后，Kimi Code 的 backend 才能真正从 `detect` 走到执行。

## 5. 实测

- WorktreeFS 仓库（有 `.tower/comms`）：`supported=true`，store/mount 正确；
- dashboard（有 Tower workspace）：`supported=true`；
- 普通临时目录：`supported=false`，`reason=not-a-git-repo`；
- `.tower` 临时残留但无 `comms/worktrees`：`supported=false`，
  `reason=tower-workspace-missing`。

## 6. 提交

- `crates/wtfs-cli/src/lib.rs` 新增 `detect` 子命令；
- `cargo test -p wtfs-cli` / `cargo check --workspace` 通过。
