//! wtfs-vfs 构建脚本：WinFSP delayload 链接（winfsp-rs 要求）。
//!
//! 与 `winfsp::build::winfsp_link_delayload` 相同的逻辑内联展开——build
//! 脚本无法直接使用受 feature 门控的 crate 自身。
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_abi = std::env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
    if target_os != "windows" {
        panic!("wtfs-vfs only builds on windows");
    }
    let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default().as_str() {
        "x86_64" => "winfsp-x64.dll",
        "x86" => "winfsp-x86.dll",
        "aarch64" => "winfsp-a64.dll",
        arch => panic!("unsupported arch: {arch}"),
    };
    match (target_env.as_str(), target_abi.as_str()) {
        ("msvc", _) => {
            println!("cargo:rustc-link-lib=dylib=delayimp");
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        ("gnu", "llvm") => println!("cargo:rustc-link-arg=-Wl,--delayload={dll}"),
        (env, abi) => panic!("unsupported toolchain: {env}-{abi}"),
    }
}
