//! wtfs-daemon 构建脚本：补齐 WinFSP 的 delay-load 链接参数。
//!
//! 背景：wtfs-vfs → winfsp-sys 的 build.rs 用
//! `cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll` 实现延迟加载——进程启动
//! 不需要 winfsp-x64.dll，首次调用 WinFSP API 时由
//! `wtfs_vfs::win::ensure_winfsp_runtime`（注册表/PATH 探测 + LoadLibrary）
//! 先把模块驻留，延迟导入再绑定。但 cargo 的 `rustc-link-arg`（无后缀变体）
//! 只对本包自己的构建产物生效，不会传播到下游 crate 的 bin/test——实测
//! wtfsd.exe 与集成测试二进制仍带 winfsp-x64.dll 静态导入，加载即失败
//! （STATUS_DLL_NOT_FOUND），M3 的 delay-load 设计完全落空。
//!
//! 本脚本用带后缀的变体把 `/DELAYLOAD:winfsp-x64.dll` 显式补到 wtfs-daemon
//! 自己的 bin（wtfsd）与 test（集成测试）上；`delayimp.lib` 已由
//! winfsp-sys 经 `rustc-link-lib` 传递链接。效果：
//!
//! * `wtfsd`：启动不再需要 PATH 里有 WinFSP；挂载时 ensure_winfsp_runtime
//!   按注册表加载后延迟绑定。
//! * 测试：不触碰 WinFSP 的单测/集成测试二进制直接可加载运行；
//!   `#[ignore]` 真实挂载测试在首次调用时经 ensure_winfsp_runtime 解析。
//!
//! 注意（评审 P3 备注）：link args 只对本 crate 的 bin/test 生效。当前
//! wtfs.exe 不引用任何 WinFSP 符号（链接器死代码剥离后无 winfsp 导入），
//! 无需处理；若将来 CLI 直接调用 wtfs-vfs，需在 wtfs-cli 侧加同样的
//! /DELAYLOAD link-arg（或 build.rs），否则其二进制将带静态导入、
//! 无 PATH 即启动失败。

fn main() {
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winfsp-x64.dll");
    println!("cargo:rustc-link-arg-tests=/DELAYLOAD:winfsp-x64.dll");
}
