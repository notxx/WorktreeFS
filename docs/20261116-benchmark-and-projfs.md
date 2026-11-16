# 基准结果与 ProjFS 现状（换机交接）

日期：2026-11-16
基线：`master` @ `ce43dff`（M21 合并后）

这份文档给换机后的自己/同事一个入口：先看基准结论，再看 ProjFS 能不能用、怎么开，最后是新机器检查清单。

---

## 1. 基准结论（native / B1 / C 三方，10 wt × 10 轮）

夹具：git/git @ `923ecfe`，4,874 tracked files，10 个 worktree，每轮 fresh clone，native / B1 / C 同轮交替。

- **B1** = WinFSP per-worktree overlay（template lower + upper + 标记日志）。
- **C** = CAS 虚拟视图（Git 对象库 + manifest + 按需 hydrate + upper），显式 `--no-checkout` + seed + 注册 + 清位。
- native = 普通 NTFS checkout。

### 中位（ms）

| 指标 | native | B1 | C |
|---|---:|---:|---:|
| worktree add | 2,735 | 4,169 + ready 1,868 = 6,037 | 38 + seed 101 = **139**（含注册清位 ~190） |
| remove --force | 1,292 | 3,250 | **34** |
| status 冷 | 168 | **101** | 147 |
| status 热 | **128** | 101 | 145 |
| diff | **125** | 121 | 231 |
| commit | **275** | 227 | 293 |
| 冷首读 200 文件 | **43** | 142 | 835（0.22 / 0.71 / 4.18 ms/文件） |
| 10 wt 磁盘 | 492.6 MB | 50.5 MB | **3.6 MB** |

B1 一次性成本：template 3,666 ms + mount 5,107 ms（每轮重建 store 口径，不计入 add 全程）。

### 口径与限制

- native / B1 是 **100 个 per-wt 原始值**；C 是每轮 harness 表值，**10 个轮中位样本**。C 的 p95/std 不能与 native/B1 的 100 样本直接并列，只能作量级参考。
- 与 3×10 基线相比，除 B1 ready +14%（高方差轮询）和 mount +31%（环境噪声嫌疑）外，偏差 ≤7.2%。
- 离群值保留未剔除：r1 全腿预热偏慢；B1 add/remove 重尾；C r10 remove 单点 97.1 ms（其余 33.3–36.0）。

### 结论

- **C 的强项**：add ~139 ms（比 B1 快 ~43×）、remove 34 ms（~95×）、磁盘 3.6 MB。
- **C 的弱项**：status/diff/commit 比 B1 慢 24%–2×；冷首读 4.18 ms/文件。
- **B1 的强项**：status 冷 101 ms 甚至快于 native；diff/commit 接近 native；plain `git worktree add` 直接可用。
- **C 的硬前置**：必须 `--no-checkout` + seed + 注册 + 清位；没有 shim/Tower hook 时，plain `git worktree add` 会退化为全量复制（见下）。

### C 不带 `--no-checkout` 的退化（M20）

- 真实 plain `git worktree add` 命中挂载根 `auto_register`，建的是 B1 overlay；C store 无模板 → full-copy 兜底。
- add **22,825 ms**（B1 的 5.7×），upper 49.3 MB / 4,875 文件，status 整树 4,874 条 D，视图不可用，remove 失败/不确定。
- 模拟强制物化（CAS 视图 + `reset --hard`）：upper 32.3 MB / 1,540 文件（内容与 manifest 一致的写入在 close 时回收），hydration 缓存 48.1 MB / 4,757 文件，首读 0 miss。
- 结论：**shim / `--no-checkout` 是 C 的硬前置**；没有它，C 不是慢，而是没接线。

详细数据：

- `docs/m19-b1-vs-c.md`（主报告）
- `docs/m19-b1-vs-c-10x10-raw.md`（10×10 原始输出）
- `docs/m20-c-plain.md`（C-plain 退化实测）
- 复跑脚本：`scripts/bench-m19-b1-vs-c.ps1`、`scripts/bench-m20-c-plain.ps1`

---

## 2. ProjFS 现状

### 它是什么

**Windows Projected File System (ProjFS)**，微软随 Windows 提供的用户态投影文件系统：

- 把一个真实目录作为 virtualization root，文件以 placeholder 形式存在；
- provider 在枚举/打开/读取时收到回调，按需提供元数据和内容；
- 微软自己的 VFS for Git (GVFS)、OneDrive Files On-Demand 基于它。

### 为什么值得关注

`PRJ_CALLBACK_DATA` 带两个 WinFSP 没有的字段：

```c
UINT32 TriggeringProcessId;
PCWSTR TriggeringProcessImageFileName;
```

- 在 `PRJ_GET_PLACEHOLDER_INFO_CB`、`PRJ_GET_FILE_DATA_CB`、`PRJ_NOTIFICATION_CB` 中可用；
- 拿不到时 PID 为 0、image 为 NULL；
- 可以据此做进程感知的陷阱/审计（例如区分 `git.exe` 和编辑器），而 WinFSP 只给用户 SID。

### 当前机器状态（2026-11-16）

```powershell
Get-CimInstance Win32_OptionalFeature -Filter "Name='Client-ProjFS'" |
  Select-Object Name, InstallState
```

结果：

```
Name          InstallState
Client-ProjFS            2
```

`InstallState = 2` 表示 **Disabled**；同时：

- `sc query ProjFS` → 服务未安装
- `Test-Path C:\Windows\System32\projectedfslib.dll` → False

即：**这台机器没有开启 ProjFS**，ProjFS 原型暂时跑不起来。

### 怎么检查

非管理员：

```powershell
Get-CimInstance Win32_OptionalFeature -Filter "Name='Client-ProjFS'" |
  Select-Object Name, InstallState
```

`InstallState`：1 = Enabled，2 = Disabled，3 = Absent。

管理员：

```powershell
Get-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
# 或
dism /online /Get-FeatureInfo /FeatureName:Client-ProjFS
```

辅助信号：`sc query ProjFS`、`Test-Path C:\Windows\System32\projectedfslib.dll`。

### 怎么开启

管理员 PowerShell：

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -All
# 或
dism /online /Enable-Feature /FeatureName:Client-ProjFS /All
```

可能需要重启。开完再验证 `InstallState = 1`、`sc query ProjFS` 有服务、`projectedfslib.dll` 存在。

### 坑

- PID/image 可能为 0/NULL：后台 hydration、杀毒、系统进程、受保护进程都会这样。
- PID 会复用；image 路径可被重命名/伪造，**不能当安全边界**，只能当提示。
- ProjFS 根目录通常是 NTFS；ReFS 支持需按 Windows 版本确认。
- ProjFS 读多写少，Git checkout 这种大量创建/写入要专门验证。
- 需要启用 Windows 可选功能，部署多一步。

### 和当前项目的关系

- 现有 B1/C 都基于 WinFSP；ProjFS 是“能拿到 PID”的备选路线，不是当前实现的依赖。
- 如果只是想避免 shim，路径/序列陷阱（新 worktree 目录 + `.git` 创建）就够了，不需要 PID。
- 如果要做进程审计（例如 node_modules 被谁删），ProjFS 只覆盖投影根；更通用的是 ETW `Microsoft-Windows-Kernel-File` 或 minifilter。

---

## 3. 换机检查清单

1. **Rust 工具链**：按 `rust-toolchain.toml` 安装；`cargo --version` 可用。
2. **WinFSP**：安装 WinFSP 2.x（B1/C 都需要）；确认 `winfsp-x64.dll` 可被找到。
3. **LLVM/libclang**：`winfsp-sys` 构建需要 `LIBCLANG_PATH`。当前机器是 `%LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin`；新机器装 LLVM 后设置同样变量。
4. **Git**：2.40+；benchmark 脚本需要 PowerShell 5.1+。
5. **ProjFS（可选）**：按上面的命令检查/开启；不开不影响现有实现。
6. **构建**：
   ```powershell
   cargo build --release
   cargo test --workspace
   ```
7. **跑基准**：
   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\bench-m19-b1-vs-c.ps1 -Rounds 10 -Worktrees 10
   ```
   夹具路径和参数见脚本头部注释。
8. **确认 `git status` 干净**，再开始新任务。

---

## 4. 远程仓库

当前项目 **没有配置 git remote**（`git remote -v` 为空）。换机前需要二选一：

- 在 GitHub/GitLab/内网建一个空仓库，然后：
  ```bash
  git remote add origin <url>
  git push -u origin master
  ```
- 或者直接打包整个目录（含 `.git`）拷到新机器；本地分支和 `.tower/comms` 审计记录都会保留。

推送前注意：`.tower/comms/` 里有 agent 名称、任务记录和本地路径；如果仓库要公开，先确认这些内容是否可以公开。
