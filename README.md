# WorktreeFS

为 Kimi Code Tower 的 git worktree 提供 COW overlay 的 WinFSP 文件系统（仅 Windows）。
实施计划见 `specs/20261112-worktreefs-implementation-plan.md`。

## 当前里程碑：M0（dedupe-only）

- **不依赖 hook，也不拦截 git**：`git worktree add` 正常执行，FS 在写入路径逐文件比对模板 hash——与模板一致则丢弃写入（记 lower），不同才写 upper。
- FS 挂载在 `<repo>/.tower/worktrees`，主仓库保持原生。
- 初始化期 `readdir` 返回空（通过 Git 的"目录必须为空"检查）；Git 写完 worktree index 后由 daemon 切到 `ready`。
- 失败兜底：模板/状态读取失败 → 全量 copy，worktree 仍然正确；正确性优先。
- 非 M0 目标：O(1) 创建（M1 起用 git shim）、workspace 根 passthrough（M2）。

## Workspace 布局

| crate | 职责 |
|---|---|
| `wtfs-core` | 共享类型与存储契约：模板、overlay 元数据（SQLite WAL）、dedupe 决策、stat、路径校验 |
| `wtfs-vfs` | WinFSP `FileSystemContext`：路径路由、overlay 后端（lower=模板 / upper=私有） |
| `wtfs-daemon` | 挂载生命周期、named pipe API、模板/overlay 注册表、index 监听、gc |
| `wtfs-cli` | `wtfs` 命令行 |
| `wtfs-git-shim` | `git.exe` 包装器（M1 起启用；M0 不构建使用） |

`wtfs-core` 的 API 骨架是 M2（实现）与 M3+（消费）之间的稳定契约：
值类型 `TemplateId` / `WorktreeId` / `TemplateSpec` / `Template` / `TemplateFile` /
`Stat` / `DedupeDecision` / `WorktreeState` / `OverlayConfig`，
存储 trait `TemplateStore` / `OverlayStore`（对象安全，可放 `Arc<dyn _>`）。

## 构建 / 测试

- Windows + Rust stable（`rust-toolchain.toml` 固定 stable 通道）。
- `wtfs-vfs` 链接 WinFSP：`winfsp-sys` 默认用内置导入库编译，运行时需要系统安装 WinFSP（`winfsp.dll`）。
- `winfsp-sys` 构建期用 bindgen 生成 FFI 绑定，**需要 libclang**（`LIBCLANG_PATH` 指向 LLVM 的 `bin` 目录）。

本机 LLVM（portable，无管理员权限安装）：

- 位置：`%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5`（来源：llvm-project 23.1.5 官方 win64 发行包）
- 构建 / 检查时设置环境变量：
  `set LIBCLANG_PATH=%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin`
- gix-index 需显式选定哈希算法：workspace 已为 `gix-index` 开启 `sha1` feature
  （gix-hash 0.28 `default = []`，不选算法会编译失败）。

```powershell
cargo check --workspace
cargo test --workspace
cargo run -p wtfs-cli            # 打印版本
cargo run -p wtfs-cli -- --version
```

`Cargo.lock` 入库（workspace 含二进制目标）；`target/`、`*.db`、`*.db-wal`、`*.db-shm`、`.tower/` 不入库。
