//! # xtask：项目任务脚本
//!
//! 用 Rust 本身写构建脚本（替代 Makefile/just），
//! 交叉编译、SDK 打包、发布流程统一通过 `cargo xtask <task>` 调用。
//!
//! 优点：脚本与项目同语言、同工具链、可测试，没有外部依赖。
//! 阶段 6/9 将扩展 cross-compile / package-sdk / release 任务。

use clap::Parser;

#[derive(Parser)]
#[command(name = "xtask", about = "rust-im 项目任务脚本")]
enum Task {
    /// 运行全量测试（等同于 cargo test --workspace）
    Test,
}

fn main() -> anyhow::Result<()> {
    let task = Task::parse();
    match task {
        Task::Test => {
            // 直接调用 cargo，保持与开发者手动执行一致的行为
            let status =
                std::process::Command::new("cargo").args(["test", "--workspace"]).status()?;
            anyhow::ensure!(status.success(), "cargo test 失败：{status}");
            Ok(())
        }
    }
}
