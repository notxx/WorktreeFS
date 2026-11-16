# M19/B190100：B1 与 C 两种实现对比（性能 + 优缺点 + 适用场景）

B1 = master 此前基线（commit `0922092`，M14 merge）：WinFSP per-worktree
overlay，模板 lower + upper + 标记日志。C = 新方案（`ff4216f`，M18 merge）：
CAS 虚拟视图，Git 对象库 + manifest + 按需 hydrate + upper。两者挂载点相同
（`<repo>/.tower/worktrees`）、夹具相同（git/git @923ecfe，4,874 文件），
保证可比。

2026-11-16 复测（B210100）升级为 **native / B1 / C 三方 10 wt × 10 轮**，
作为本文主表（见"性能表（10 wt × 10 轮三方复测）"）；原先的 3 轮 B1/C
数据保留为历史对照。

## 对比口径与边界（必读）

* **驱动方式不同**：B1 走真实 release CLI（`wtfs template create` →
  `wtfs mount` → plain `git worktree add` → … → `git worktree remove
  --force` → `wtfs unmount`，wtfsd 守护进程常驻）；C 的 add/seed/注册在
  M18 **没有 CLI/daemon 接线**（显式流程，库 API 驱动），只能用 M18 的
  `cas_mount_bench` harness（进程内挂载）驱动。两者的 FS 路径与 git 操作
  口径一致，但驱动进程形态不同（外部 CLI + 守护进程 vs 进程内 harness）。
* **C 的 add 是显式流程**：`git worktree add --no-checkout` + `seed_
  stat_aligned` + `write_stat_config` + `register_cas_worktree` +
  `clear_skip_worktree`。M18 边界：Tower 的 plain `git worktree add`
  拦截/shim 自动注册不在本 mission。**下表 C 的 add 数字不得冒充 Tower
  自动流程**；B1 的 add 是 plain `git worktree add`（挂载点内
  auto_register）。
* remove 两者都用 `--force`（C 的稀疏 upper 需要；B1 同口径）。
* wt 数按腿不同：native 与 B1 腿由 `-Worktrees` 参数控制（默认 10）；C 腿是
  harness（cas_mount_bench）**固定 10 wt**，不受参数影响。
* native 腿（10×10 复测新增）：挂载点外目录（轮次目录下 `native/wt-M`）plain
  NTFS 上逐 wt `git worktree add --detach` → status 冷/热 → diff --stat →
  编辑 README → `git add -A` + `git commit` → remove --force。无模板/mount
  一次性成本，磁盘 = 每 wt 全量树。commit 口径与 B1/C 不同（`add -A` +
  `commit` 两进程 vs `commit -am` 单进程，计时含两段之和）。
* 每轮 fresh 本地 clone 夹具；3×10 基线为 B1/C 交替共 3 轮（2026-11-15），
  10×10 复测为轮内 native → B1 → C 交替共 10 轮（2026-11-16）；轮间清理
  store 与 worktree。B1/C 各自独立 checkout + target dir 构建 release。
* 指标：add（native 原生 checkout；B1 含 ready 轮询；C 含 seed，注册/清位
  未单列计时）、status 冷/热、diff --stat 干净树、commit 单文件、remove、
  磁盘（native 每 wt 全量树；B1 模板+upper；C upper+标记+hydration 缓存）、
  首读 200 个 .c/.h、hydration 缓存大小；驱动进程峰值工作集（500ms 采样，
  native 不采样）。10×10 复测起脚本直接输出全程汇总
  median/p95/min/max/样本标准差（p95 线性插值）。
* 时间与机器：2026-11-15（3×10）、2026-11-16（10×10 三方），同一 Windows
  机器、release 构建，scripts/bench-m19-b1-vs-c.ps1 可复跑（见文末）。

## 性能表（10 wt × 10 轮三方复测，2026-11-16，主表）

native（挂载外 plain NTFS 地板）/ B1（overlay+模板）/ C（CAS+manifest）三方。
同夹具每轮 fresh clone，轮内 native → B1 → C 交替，10 轮全部成功、无剔除
任何样本。可比性：C 侧代码与 3×10 基线逐位同源（`git diff ff4216f..master
-- crates/ Cargo.lock` 为空，harness 二进制同哈希 `2f98276a1311b80d`），B1
仍为 `0922092` 同一二进制。C 腿 harness 固定 10 wt，本轮 `-Worktrees 10`
使三腿 wt 数一致。

### 全程汇总（ms；median / p95 / min / max / 样本标准差）

样本口径：native/B1 = per-wt 原始值（10 轮 × 10 wt = **100 样本/指标**）；
C = 每轮 harness 表值（10 wt 中位 → **10 样本/指标**）；首读合计为每轮 1 值
（各 10 样本）。p95 为线性插值，std 为样本标准差（n-1）。

| 腿 | 指标 | n | median | p95 | min | max | std |
| -- | ---- | - | ------ | --- | --- | --- | --- |
| native | add | 100 | 2,735.2 | 3,242.8 | 2,631.5 | 4,036.4 | 264.3 |
| native | status 冷 | 100 | 168.1 | 265.9 | 57.1 | 283.2 | 56.1 |
| native | status 热 | 100 | 127.9 | 204.0 | 47.9 | 223.4 | 52.3 |
| native | diff --stat | 100 | 125.1 | 253.3 | 39.2 | 279.5 | 79.2 |
| native | commit① | 100 | 275.1 | 346.9 | 166.1 | 371.6 | 61.8 |
| native | remove --force | 100 | 1,292.3 | 1,397.9 | 1,221.4 | 2,147.9 | 98.5 |
| native | 首读合计（200 文件） | 10 | 43.2 | 68.3 | 36.9 | 77.2 | 12.7 |
| B1 | add | 100 | 4,168.7 | 4,760.5 | 3,920.3 | 6,157.5 | 348.4 |
| B1 | ready | 100 | 1,867.9 | 2,357.6 | 1,679.1 | 3,331.3 | 238.6 |
| B1 | status 冷 | 100 | 100.5 | 113.5 | 93.8 | 140.5 | 7.1 |
| B1 | status 热 | 100 | 101.3 | 118.7 | 93.3 | 183.7 | 12.0 |
| B1 | diff --stat | 100 | 120.5 | 142.6 | 95.9 | 183.9 | 12.6 |
| B1 | commit -am | 100 | 227.3 | 268.4 | 183.0 | 347.4 | 25.3 |
| B1 | remove --force | 100 | 3,250.2 | 3,672.6 | 3,174.4 | 5,129.7 | 247.1 |
| B1 | 首读合计（200 文件） | 10 | 141.6 | 157.3 | 138.7 | 163.9 | 7.5 |
| C | add | 10 | 38.4 | 41.8 | 37.2 | 43.1 | 1.7 |
| C | seed | 10 | 101.0 | 110.5 | 99.9 | 115.2 | 4.6 |
| C | status 冷 | 10 | 146.9 | 163.1 | 141.5 | 170.3 | 8.4 |
| C | status 热 | 10 | 145.3 | 155.8 | 142.6 | 159.3 | 5.2 |
| C | diff --stat | 10 | 230.6 | 235.8 | 217.7 | 236.6 | 6.8 |
| C | commit -am | 10 | 292.9 | 304.4 | 273.5 | 308.9 | 9.6 |
| C | remove --force | 10 | 34.1 | 69.6 | 33.3 | 97.1 | 19.9 |
| C | 首读合计（200 文件） | 10 | 835.2 | 853.6 | 819.5 | 854.7 | 11.8 |

① native commit = 编辑 README → `git add -A` + `git commit` 两进程计时之和
（`add -A` 单列中位 ~131 ms，各轮 104.4-170.5）；B1/C = `commit -am` 单进程。
native 拆开后 `commit` 步本身 ≈144 ms，仍低于 B1 的 227——NTFS 直连与
经挂载视图逐项探测的 stat 单价差所致（与 3×10 归因一致）。

### 三方中位比（10×10）

| 指标 | native | B1 | C | C ÷ B1 | B1 ÷ native | C ÷ native |
| ---- | ------ | -- | - | ------ | ----------- | ---------- |
| add | 2,735.2 | 4,168.7 | 38.4 | 0.009 | 1.52 | 0.014 |
| add 全程（+ready/seed）② | 2,735.2 | 6,036.6 | 139.4 | 0.023 | 2.21 | 0.051 |
| status 冷 | 168.1 | 100.5 | 146.9 | 1.46 | **0.60** | 0.87 |
| status 热 | 127.9 | 101.3 | 145.3 | 1.43 | **0.79** | 1.14 |
| diff --stat | 125.1 | 120.5 | 230.6 | 1.91 | 0.96 | 1.84 |
| commit（口径见①） | 275.1 | 227.3 | 292.9 | 1.29 | 1.21 | 1.07 |
| remove --force | 1,292.3 | 3,250.2 | 34.1 | 0.010 | 2.52 | 0.026 |
| 首读合计（200 文件） | 43.2 | 141.6 | 835.2 | 5.90 | 0.31 | 19.3 |

② B1 全程 = add + ready；C 全程 = add + seed，未含注册与批量清
skip-worktree（M18 口径 ~48 ms/4,874 + 内存注册），即 C 全程实为 ~190 ms
量级（对 B1 ~32×）。native 无 ready 概念，add 即全程。

native 腿给出的地板参照：**add 2.7 s / remove 1.3 s / 首读 0.22 ms/文件
是 NTFS 的物理地板**。B1 的生命周期成本 = 地板的 1.5-2.5×（模板 checkout
与删除都要经 WinFSP 逐文件回调搬流量）；C 把生命周期操作压到地板之下
（add/remove 不搬树，只改 index/指针），代价集中在读路径（冷首读为地板的
~19×）。另一个反直觉发现：**B1 的 status/diff/commit 持平甚至快于 native**
（status 冷 100.5 vs 168.1 ms，0.60×）——模板 index 与 FS stat 对齐后走
index-refresh 快路径，省掉 NTFS 全树 lstat 扫描；native 的 status/diff
方差也最大（std 52-79 vs B1 的 7-13）。

### 一次性成本与磁盘（10 wt 口径）

| 维度 | native | B1 | C |
| ---- | ------ | -- | - |
| lower 构建 | 无（无模板） | template create：10 轮 median **3,666** / p95 4,312 / max 4,791 ms（物化 4,875 文件树 + index） | manifest 装载一次跨 wt 共享（M17 计 ~592 ms；规模 4,874 文件 / 48.2 MB 树） |
| mount | 无（不挂载） | 10 轮 median **5,107** / p95 5,861 / max 6,360 ms（spawn wtfsd + WinFSP；3×10 时 3,903，本轮整体偏高 ~1.2 s，见离群说明） | harness 进程内挂载，未单列 |
| 10 wt 磁盘（各提交 1 文件后） | 48,750 文件 / **492.6 MB**（每 wt 49.26 MB 全量树，10 轮全部一致） | upper 20 文件 / 0.9 KB + 共享模板 49.7 MB（store 总计 **50.5 MB**） | upper 20 文件 / 0.9 KB + hydration 缓存 216 文件 / **3.6 MB**（读过才灌）+ 对象库零复制 |
| 驱动进程峰值 WS | —（git 子进程，不采样） | wtfsd 37.7-38.5 MB | harness 34.1-37.0 MB（含 manifest/registry，非纯 FS 层） |

10 wt 磁盘比：native 492.6 MB ≈ B1 50.5 MB 的 **9.8×** ≈ C ~3.6 MB 的
**~137×**。

### 噪声与离群轮次（如实记录，未剔除任何样本）

* **第 1 轮整体偏慢（预热效应）**：native add 3,221（其余轮 2,706-2,798）、
  native 首读 77.2（其余 36.9-57.3）、B1 template 4,791 / mount 6,360
  （双双全程最高）、B1 ready 1,961。机制：空闲一夜后 OS 文件缓存冷启动。
  保留不剔除——它恰是"冷机器第一次跑"的真实体验上界。
* **B1 重尾**：add 单 wt 最大 6,157.5 ms（r7-wt10，1.5× 中位）、remove 单 wt
  最大 5,129.7 ms；r2 的 remove 轮中位 3,659.9（+12%）。B1 add/remove 的
  std（348/247）为三腿最宽，符合逐文件 IPC 流量对瞬时干扰的敏感。
* **C 单点离群**：r10 remove 97.1 ms（其余 9 轮 33.3-36.0，2.85× 中位），
  绝对值仍近零，疑似瞬时干扰（如杀软扫描）；保留。C 腿整体最稳（std
  1.7-11.8）。
* **native status/diff 方差大**（std 52-79；冷态 47.9-283.2 ms 波动）：
  NTFS 元数据缓存敏感；B1（模板 index 快路径）与 C（manifest stat）的
  status std 仅 5-13。
* 10 轮全部成功；脚本任一步失败即 throw（无静默重试），本轮无失败轮次。

### 与 3×10 基线的对比（同机器同夹具同代码，仅轮数与 native 腿不同）

| 指标 | B1：3×10 → 10×10（中位） | Δ | C：3×10 → 10×10（中位） | Δ |
| ---- | ------------------------ | - | ----------------------- | - |
| add | 4,005 → 4,169 | +4.1% | 38.4 → 38.4 | ±0.0% |
| ready / seed | 1,639 → 1,868 | +14.0% | 99.9 → 101.0 | +1.1% |
| add 全程 | 5,632 → 6,037 | +7.2% | 138.3 → 139.4 | +0.8% |
| status 冷 | 99.3 → 100.5 | +1.2% | 144.5 → 146.9 | +1.7% |
| status 热 | 98.9 → 101.3 | +2.4% | 143.5 → 145.3 | +1.3% |
| diff --stat | 113.1 → 120.5 | +6.5% | 221.4 → 230.6 | +4.2% |
| commit -am | 230.2 → 227.3 | −1.3% | 286.2 → 292.9 | +2.3% |
| remove --force | 3,195 → 3,250 | +1.7% | 34.0 → 34.1 | +0.3% |
| 首读合计 | 146.2 → 141.6 | −3.1% | 848.5 → 835.2 | −1.6% |

除 B1 ready +14.0% 外全部指标偏差 ≤7.2%。B1 ready 是轮询口径（含 ~100 ms
CLI 轮询粒度），本轮 std 239、p95 2,358、min 1,679——3×10 记录的 1,639 恰
落在本轮分布最低分位，属小样本运气；量级结论（ready ~1.6-2 s）不变。其余
指标在 100 样本下被钉死：C 的 add 全程优势 ~43×（含注册/清位 ~32×）、
remove 优势 ~95×，与 3×10（~30×/~94×）一致。provenance 备注：3×10 当时的
原始结果文件 C 腿汇总行解析有缺陷（全 0），文中 C 数字系当时从轮次日志
转录（与本轮脚本直出数值吻合，±2% 内）；本轮起脚本直接落盘 C 数值，
无转录环节。

### 逐轮原始数据（10 轮 × 3 腿，每腿 10 wt 中位，ms）

逐轮与 per-wt 原始输出全文（native/B1 逐 wt 明细、C harness 全表）归档于
[docs/m19-b1-vs-c-10x10-raw.md](m19-b1-vs-c-10x10-raw.md)。

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

## 历史对照：3×10 基线（4,874 文件 / 10 worktree，3 轮交替，2026-11-15）

历史口径：每 wt 中位（ms），三轮各自值 → 三轮中位。数字保留原样，供与
10×10 主表对照（native 腿当时未测）。

| 指标 | B1（r1/r2/r3） | B1 中位 | C（r1/r2/r3） | C 中位 | C ÷ B1 |
| ---- | -------------- | ------- | ------------- | ------ | ------ |
| add① | 3,906 / 4,043 / 4,005 | **4,005** | 38.2 / 40.3 / 38.4 | **38.4** | 0.010 |
| ready（B1 watcher 轮询）/ seed（C read-tree+stat 对齐+配置） | 1,639 / 1,692 / 1,628 | **1,639** | 99.9 / 102.9 / 99.9 | **99.9** | — |
| add 全程② | 5,545 / 5,735 / 5,632 | **5,632** | 138.1 / 143.2 / 138.3 | **138.3** | 0.025 |
| status 冷 | 97.7 / 102.0 / 99.3 | **99.3** | 144.5 / 143.6 / 145.2 | **144.5** | 1.45 |
| status 热 | 96.3 / 98.9 / 99.2 | **98.9** | 143.5 / 140.5 / 143.9 | **143.5** | 1.45 |
| diff --stat 干净树 | 115.3 / 113.1 / 105.9 | **113.1** | 220.5 / 221.4 / 224.4 | **221.4** | 1.96 |
| commit -am 单文件 | 230.2 / 222.2 / 230.6 | **230.2** | 292.2 / 286.2 / 285.7 | **286.2** | 1.24 |
| remove --force | 3,200 / 3,185 / 3,195 | **3,195** | 34.0 / 32.9 / 34.6 | **34.0** | 0.011 |
| 首次读 200 文件（冷，合计） | 151.5 / 139.1 / 146.2（0.70-0.76 ms/文件） | **146.2** | 848.5 / 825.6 / 876.2（4.13-4.38 ms/文件） | **848.5** | 5.8 |

① B1 add = plain `git worktree add`（挂载点内，含模板 checkout 经 FS）；
  C add = 显式流程的 `worktree add --no-checkout` 一步。
② B1 全程 = add + ready；C 全程 = add + seed，**未含注册与清位**（harness
  未单列计时；M18 记录批量清 skip-worktree ~48 ms/4,874，注册为内存操作），
  即 C 全程实为 ~190 ms 量级。B1 的 ready 轮询含 ~100 ms 的 CLI 进程轮询
  粒度（上界估计）。

一次性成本（每轮重建 store 口径）：

| 一次性项 | B1 三轮（中位） | C 三轮 |
| -------- | --------------- | ------ |
| lower 构建 | template create：3,678 / 3,737 / 4,707 ms（**3,737**；物化 4,875 文件树 + index） | manifest 装载一次跨 wt 共享（M17 计 ~592 ms；本基准记录规模 4,874 文件 / 48.2 MB 树） |
| mount | 3,866 / 3,903 / 5,028 ms（**3,903**；spawn wtfsd + WinFSP 挂载） | harness 进程内挂载，未单列 |

磁盘与内存（10 wt 各提交 1 文件后）：

| 维度 | B1 | C |
| ---- | -- | -- |
| 每 wt upper（10 wt 合计） | 20 文件 / 0.9 KB | 20 文件 / 0.9 KB |
| 共享 lower | 模板树物化 4,875 文件 / **49.7 MB**（store 总计 50.5 MB） | Git 对象库零复制（复用仓库 .git）+ manifest 内存索引 |
| 读缓存 | 无独立缓存（NTFS 页缓存承担） | hydration 缓存 216 文件 / **3.6 MB**（status+首读后；读过才灌，oid 全局共享，上限≈树大小） |
| 标记日志 | 0 文件（bench 无删除） | 0 文件 |
| status 期间 hydrate | —（读直达模板） | 14 次，全部 `.gitignore` exclude；tracked 内容 0 |
| 驱动进程峰值 WS | wtfsd 37.8 / 38.0 / 38.0 MB | harness 35.3 / 34.8 / 35.1 MB（含 manifest/registry，非纯 FS 层口径） |

首读口径备注：B1 首读经 PowerShell `ReadAllBytes` 驱动（含 PS 循环开销），
C 首读为 harness 内 Rust `fs::read`（含 cat-file 子进程 + fsync）——驱动
差异不影响结论：C 冷首读被 hydrate 的 fsync 主导，比 B1 的 NTFS 直读慢
~6 倍/文件；命中缓存后两者同为 µs 级探测。

## 差距归因

> 本节引用 3×10 基线数字；10×10 三方复测的同指标中位偏差 ≤7.2%（仅 B1
> ready +14.0%，见主表对比），以下归因不变。native 腿补充的地板参照：
> add 2,735 / remove 1,292 ms、首读 0.22 ms/文件——B1 的生命周期成本是
> NTFS 地板的 1.5-2.5×（WinFSP 逐文件回调），C 则把生命周期操作压到地板
> 之下（不搬树，只改 index/指针）；B1 的 status/diff/commit 持平甚至快于
> native（模板 index 快路径 vs NTFS 全树扫描）。

* **C 的 add 是 O(1) 树复制 vs B1 的 O(files) 回调流量（4,005+1,639 ms
  vs 38+100 ms）**。B1 的 plain `git worktree add`（挂载点内）=
  auto_register（建 upper + 模板解析）+ git checkout 经 WinFSP 回调物化
  4,874 个文件（每文件 open/write/cleanup IPC）+ watcher 确认 index 后
  mark_ready——实测 add 4.0 s + ready 1.6 s，成本在"把树搬进可见卷"。
  C = `add --no-checkout`（只写 .git 指针 + 空 index，38 ms）+ seed
  （read-tree + gix index stat 改写，纯 index 重写、零 FS 文件流量，
  100 ms）+ 注册（内存表）+ 批量清 skip-worktree（~50 ms/4,874，M18 口
  径）——成本在"改写 index"，不经过 FS。**一个数量级的差距来自这一结构
  差异**，且 C 的 per-wt 增量成本里只有 seed 随文件数线性（index 重写）。
* **status 双方都不 hydrate 内容（145 vs 99 ms，同量级）；B1 靠模板
  stat、C 靠 manifest stat**。B1：模板 index 与 FS stat 对齐（M13），
  status 走 index-refresh 快路径，读 = 元数据探测。C：skip-worktree 清位
  后 git 只比 mtime/size（checkStat=minimal），manifest 固定 stat 与种子
  逐位对齐 → tracked 内容零读取（实测 10 wt × status 仅 14 次 hydrate，
  全部是 `.gitignore` exclude 分类读取，hydration 缓存按 oid 全局共享，
  其余 9 个 wt 全命中）。C 慢的 ~45 ms = 每目录 list_stat 枚举 + manifest
  查找 vs B1 模板树上的 NTFS stat。
* **首次读是 C 的结构性成本：cat-file 往返 + 恰好一次 fsync（4.2 vs
  0.73 ms/文件）**。B1 的首读是 NTFS 直读（模板树已物化）；C 首读每个
  文件 = `git cat-file` 子进程往返 + temp+fsync+rename 原子落缓存
  （fsync 主导）+ 读缓存。命中后 C 与 B1 同为 µs 级探测。这一差距只作用
  于"冷读集"，且缓存跨 worktree 全局共享——第二个 worktree 读同一 blob
  不再付费。
* **diff/commit 干净树零 hydrate（diff 1.96×、commit 1.24×）；成本在
  git 自身的工作量**。C 干净树 `diff --stat` 与 status 同路径（manifest
  stat，零对象读），慢近一倍的部分是每目录 list_stat 枚举 + git 自身
  工作量（B1 模板树由 NTFS 原生枚举）。commit 的计时区间只覆盖
  `git commit -am` 本身：README 编辑的 copy-up（cat-file 取 blob + 复制
  upper + 终裁 fsync）发生在计时前的写文件步骤，refresh_head 重载
  manifest 在计时后调用，都不计入 286 ms；计时内主要是 commit 的全树
  stat 扫描（等价一次 status 的 per-op VFS IPC，经挂载视图逐项探测）+
  对象与 index 写直落挂载外的真实 .git。与 B1（230 ms，同一 git 流程但
  stat 走 NTFS）的差距主要是 stat 单价差。
* **remove 的 IPC 地板：B1 删可见树，C 删稀疏 upper（3,195 vs 34 ms，
  ~94×）**。B1 `remove --force` 要经 FS 逐项删掉 4,874 文件的可见树
  （set_delete/cleanup/rmdir 回调 = IPC 地板；M14 从 6,427 ms 优化到
  3,306 ms 的正是这一面，本轮 3.2 s 与该口径一致）。C 的 upper 只有
  .git 指针 + 提交残留（干净改动回收后近零），remove 只删这 20 项 +
  回收挂载外的真实目录。这是 C 对"多而短命 worktree"场景的决定性优势。
  注意口径：C 必须 `--force`（稀疏 upper 使 git 干净检查必报删除），
  B1 同口径对齐。

## 优缺点对比

| 维度 | B1（overlay + 模板） | C（CAS + manifest） |
| ---- | -------------------- | ------------------- |
| 实现复杂度 | 组件多：daemon（状态机/watcher/管道）、模板 store（sqlite + 树 checkout + index 复制）、overlay（auto-register、M8/M9 staged 写事务、M12 标记日志、M14 负缓存） | 更薄：独立 `wtfs-cas` crate（manifest/hydration/seed，不依赖 wtfs-core）+ VFS 后端 enum；无状态机（注册即 ready）、无 staged 流；但引入 gix index 改写等 git 内部知识 |
| 故障模式 | 模板缺失/过期 → 回落全量 copy（正确但慢，且需 template update 维护）；标记日志重放；upper/元数据残留 | manifest 与 HEAD 漂移窗口（commit 后 refresh 前；分支切换完整语义 M18 未做）；seed 与 FS stat 失配 → status 假脏（checkStat=minimal + E2E 锁住）；hydration 缓存损毁可弃——真相在 Git 对象库，删缓存不丢数据 |
| Git 兼容性 | 模板 index 原生，git 全操作零特判；普通 remove 可用；分支切换/模板 refresh 原生支持 | 依赖 skip-worktree + checkStat=minimal + stat 种子；status/diff/commit 正常，但稀疏 upper → remove 必须 `--force`；分支切换 refresh（路径复用冲突）未实现；依赖 git 对 checkStat/trustctime 的行为 |
| 缓存/GC | 模板只读共享（天然零拷贝）；读无缓存、直达 NTFS；模板按 max-age-days gc | hydration 缓存按 oid 全局分片共享、可随时删重建；gc(max_bytes) 按 mtime 逐出（显式调用，未接 daemon）；manifest 一次装载跨 wt 共享 |
| 并发 | auto_register 名字幂等；单 daemon watcher；overlay 锁收敛写 | per-mount Mutex 收敛写回调；cat-file 并发灌缓存（temp+rename 原子）；同 oid 并发安全 |
| 磁盘 | 模板一次性 ~50 MB（整树）+ 每 wt upper 改动面 | 每 wt upper ≈ .git 指针 + 改动（同内容回收后近零）+ 缓存（读过才灌）+ 共享对象库零复制；add 后磁盘 ≈ 一个 index |
| 冷热读 | 首读 NTFS µs 级（树已物化） | 首读 ~4 ms/文件（cat-file+fsync，冷）；命中后 µs 级 |
| 可观测性 | daemon status 状态机（initializing/ready）+ M13/M14 回调级 profiling（回调计数/NTFS 计数三分） | 缓存统计 hits/misses/files/bytes（可机器断言"零 hydrate"）；status 报 `ready (cas)`；无生命周期状态可观察 |
| 运维 | CLI/daemon/shim 全接线：mount/unmount/status/template/gc 即开即用；wtfsd 常驻 | 无 daemon 依赖（库挂载即可用），但 add/seed/注册/remove 未接 CLI/shim——目前只能编程驱动；缓存 gc 未接线自动化 |

## 适用场景建议

一句话结论：**读热、长寿、要即插即用选 B1；仓库大、worktree 多而短命、
读稀疏、能改造流程选 C**。实测差距集中在生命周期操作（10×10 主表口径）：
add 全程 6,037 → ~190 ms（~32×）、remove 3,250 → 34 ms（~95×）、10 wt
磁盘 50.5 MB → ~3.6 MB（~14×；native 全量树为 492.6 MB）；C 付出的代价
集中在读路径：冷首读 ~6×/文件（NTFS 地板 0.22 → B1 0.71 → C 4.18
ms/文件）、status/diff 慢 1.43×/1.91×——都是读的工作集换生命周期速度。
B1 相对 native 的亮点是 status/diff/commit 持平甚至更快（模板 index 快
路径 vs NTFS 全树扫描）：status 冷 100.5 vs 168.1 ms（0.60×）。

* **选 B1**：
  * 工作流要求"今天即可用"的完整接线（plain `git worktree add` 透明、
    普通 remove、分支切换、模板 update/gc 已在 CLI/daemon 里）。
  * 首读延迟敏感：IDE 全量索引、构建系统扫全树、测试跑套件——模板物化树
    首读零成本，C 的按需 hydrate 会把成本挪到首次访问。
  * 分支切换频繁 / 模板演进正常（B1 的 refresh 语义完整，C 的跨分支
    refresh 未做完）。
  * wt 少而长寿（模板成本被长生命周期摊薄，upper 改动面小）。
* **选 C**：
  * 大仓库 × 多 worktree / 短生命周期（CI 并发检视、临时审查、批量重建）：
    add/remove 快 1-2 个数量级、add 后磁盘 ≈ 一个 index、remove 只删
    稀疏 upper。
  * 读稀疏：只碰少部分文件的审计/检索工作流，按需 hydrate 总成本低于
    整树物化；hydration 缓存跨 wt 共享，重复读零成本。
  * 能接受/消除首读延迟（预热、预取）且能改造自动化流程（shim 接线后
    C 的适用面显著扩大）。

## C 不带 `--no-checkout` 的退化（M20 补充实测）

M20 在 C 挂载视图上实测 plain `git worktree add`（不带 `--no-checkout`），
完整数据与机制归因见 **docs/m20-c-plain.md**。要点：挂载根 mkdir 命中的是
registry 级 `auto_register`——建出 **B1 overlay** 走 **full-copy 兜底**
（C 的 store 无模板）：add 22.8 s（B1 模板路径的 5.7×），每 wt upper 全量
物化 ≈49.3 MB；且无 daemon 时视图永远 Initializing → status 报整树删除
（4,874 条 D）、后续 `git worktree` 操作会把先建 wt 的 admin 元数据 prune
掉、`remove --force` 结局不确定（Permission denied / 意外成功并存）。
即便强制经 CAS 视图全量物化（模拟 plain add 的 checkout 阶段）：checkout
25.5 s/4,874 文件、hydration 缓存吃满整树 48.1 MB、首读 0 miss 但那是
checkout 时已付的代价。**结论：shim/`--no-checkout` 显式流程是 C 的关键
前置**——下表第 1 条改进的量化依据。

## C 的后续改进（按收益排序）

1. **shim 自动注册**：把显式流程（add --no-checkout + seed + 注册 + 清位）
   藏进 plain `git worktree add`（wtfs-git-shim 拦截或 daemon watcher），
   消除与 B1 的接线差距——这是 C 目前最大的可用性缺口。
2. **缓存预取**：挂载后台按 manifest 预 hydrate 热点子树（如构建依赖、
   README/顶层文件），把首读延迟从用户路径挪到挂载瞬间。
3. **整 workspace 挂载**：一次挂载呈现全部 wt（现有 VfsRegistry 已支持
   混合注册），免去逐 wt 手工注册的流程成本。
4. **独立 CAS 服务**：hydration 缓存与对象访问抽出为跨仓库服务（同一台
   机器多仓库共享 oid 缓存；进一步可对接远端 CAS）。
5. **remove 先注销再 remove**（免 `--force`）：daemon/shim 在
   `worktree remove` 前先 unregister 视图，让 git 干净检查不再看到稀疏
   upper 的"删除"，恢复普通 remove 语义。
6. **分支切换 refresh 完整语义 + upper/缓存清理接进 `wtfs gc` 计划**
   （M18 已留扩展点）。

## 复跑

```powershell
# 前置：B1 checkout（%TEMP%\wtfs-b19-b1 @ 0922092）与 C（本仓库）
# 各自 cargo build --release / cargo test --release -p wtfs-vfs
# --test cas_mount_bench --no-run；或直接 -Build 让脚本代劳。
# 10×10 三方复测（native/B1/C 各 10 wt × 10 轮，实测约 35 分钟）：
powershell -ExecutionPolicy Bypass -File scripts\bench-m19-b1-vs-c.ps1 `
    -FixtureRepo C:/Users/notxx/AppData/Local/Temp/wtfs-bench-large-m12/repo `
    -Rounds 10 -Worktrees 10 -OutFile %TEMP%\wtfs-b19-10x10-results.md
```

脚本行为：每轮 fresh 本地 clone → native（挂载外 plain NTFS，per-wt
计时）→ B1 CLI 全流程（模板一次性计时）→ C harness（cargo test，
WTFS_BENCH_REPO 指向轮 clone）→ 逐轮落原始结果（native/B1 含 per-wt
明细）+ 全程汇总表（median/p95/min/max/样本标准差；native/B1 = per-wt
100 样本，C = 每轮 harness 中位 10 样本）+ 轮次汇总表。残留 wtfsd 按轮
管道名（`\\.\pipe\wtfs-b19-r<N>`）开局清理；C 腿经 `cmd /c` 超时兜底
驱动（`-CargoTimeoutSec`，默认 1800 s），任一步失败即 throw（无静默
重试，失败轮次如实暴露）；跑完默认清理 `%TEMP%\wtfs-b19-r<N>` 轮次
目录（`-KeepRounds` 保留原始日志）。注意：脚本文件必须保存为 UTF-8
with BOM（PS5.1 无 BOM 会按 ANSI 误读中文注释与全角字符串）。
