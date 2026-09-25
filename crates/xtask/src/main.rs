//! # xtask：项目任务脚本
//!
//! 用 Rust 本身写构建脚本（替代 Makefile/just），
//! 交叉编译、SDK 打包、发布流程统一通过 `cargo xtask <task>` 调用。
//!
//! 优点：脚本与项目同语言、同工具链、可测试，没有外部依赖。
//! 阶段 6/9 将扩展 cross-compile / package-sdk / release 任务。

mod sdk;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "rust-im 项目任务脚本")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// 运行全量测试（等同于 cargo test --workspace）
    Test,
    /// 打包 FFI SDK 到 dist/sdk/（动态库 + 头文件 + Java 绑定源码）
    Sdk {
        /// 交叉编译目标三元组（可重复；未安装的 target 会跳过并提示）
        #[arg(long = "target")]
        targets: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().task {
        Task::Test => {
            // 直接调用 cargo，保持与开发者手动执行一致的行为
            let status =
                std::process::Command::new("cargo").args(["test", "--workspace"]).status()?;
            anyhow::ensure!(status.success(), "cargo test 失败：{status}");
            Ok(())
        }
        Task::Sdk { targets } => sdk::run(&targets),
    }
}
