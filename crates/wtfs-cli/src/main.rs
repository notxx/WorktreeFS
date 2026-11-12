//! `wtfs` 可执行入口。实现全部在 lib：拆出的目的是让 `tests/` 下的
//! B070100 回归测试能直接驱动 [`wtfs_cli::spawn_detached`] 的生产
//! spawn 路径。

fn main() {
    if let Err(e) = wtfs_cli::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
