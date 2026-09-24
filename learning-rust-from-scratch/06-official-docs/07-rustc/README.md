# 07 - The rustc Book（编译器）

> **原文**：<https://doc.rust-lang.org/rustc/index.html>
> **中文**：<https://learnku.com/docs/rustc-book/2020>（社区版，主线章节齐全）
> **本地**：`rustup doc --rustc`
> **规模**：30 章 + 130+ 页平台支持文档（后者不用读）

## 定位

> 【Java】≈ 《javac 指南》+ JVM 选项手册。
> 日常你通过 Cargo 间接使用 rustc（`cargo build --verbose` 可看到每次调用），
> 这本书的价值在于：**看懂编译器在干什么、控制它干什么**。

对 rust-im 有三处直接用途：**Lints 章**（clippy 的地基）、
**Targets 章**（跨平台交叉编译，im-sdk 的 iOS/Android 构建）、
**Codegen Options 章**（性能压测调优）。

## 核心章节导读（30 章中的重点）

| 英文 | 中文 | 导读 | 优先级 |
|---|---|---|---|
| What is rustc? | 什么是 rustc | 编译单元=crate 而非文件（与 C 编译器的本质区别） | 👀 |
| **Command-line Arguments** | 命令行参数 | `--edition`/`--emit`/`-C`/`-Z` 体系总览 | 🔍 |
| **Lints**（含 Levels/Groups/Listing） | **Lint 检查** | lint 分级(allow/warn/deny/forbid)、lint 组（rust-im 用的 `unsafe_code`、clippy::pedantic 全在这套体系上） | ⭐⭐⭐ |
| JSON Output | JSON 输出 | 给 IDE/工具消费的诊断输出格式（RustRover 的红线就是它） | 🔍 |
| Tests | 测试模式 | `--test` 标志的机制（cargo test 背后） | ⭐ |
| **Targets**（Built-in/Custom） | **目标平台** | 交叉编译：`--target aarch64-linux-android`——**im-sdk 移动端构建的核心**；自定义 target 描述裸机 | ⭐⭐ |
| **Codegen Options** | **代码生成选项** | `-C opt-level/lto/codegen-units/target-cpu`——**性能压测调优清单** | ⭐⭐ |
| Profile-guided Optimization | PGO | 用运行时 profile 反哺编译优化（500 万连接压测后可尝试） | ⭐ |
| Instrumentation-based Code Coverage | 覆盖率 | `cargo llvm-cov` 背后的机制 | ⭐ |
| Linker-plugin-based LTO | 链接器 LTO | 跨语言 LTO（FFI 性能优化） | 🔍 |
| **Check Conditional Configurations** | check-cfg | `#[cfg]` 的完整性检查——条件编译 typo 防呆 | ⭐ |
| Exploit Mitigations | 缓解措施 | 栈保护/CET/RELRO——安全加固选项（IM 服务暴露公网，值得看） | ⭐ |
| Symbol Mangling | 符号修饰 | v0 符号格式——看崩溃堆栈/调试器时的字典 | 🔍 |
| Jobserver | 任务服务器 | 并行编译调度（为什么 cargo 和 make 能协作） | 🔍 |
| Contributing | 参与贡献 | rustc 开发的入口（另一个世界的大门） | — |
| Platform Support | 平台支持 | 130+ 页各目标平台说明——**当字典查**，不通读 | 🔍 |

## 与 rust-im 的三处落点

### 1. Lint 体系（第 4 章）

rust-im 根 Cargo.toml 的这两行就是这本书的应用：

```toml
[workspace.lints.rust]
unsafe_code = "warn"          # unsafe 必须显式声明理由
[workspace.lints.clippy]
pedantic = "warn"             # 教学级严格：文档规范/惯用法全检查
```

理解 lint 分级后你就明白：`#[allow(clippy::needless_return)]`
为什么可以精确豁免单处代码，以及 `#![deny(warnings)]`
（CI 里把警告当错误）的完整语义。

### 2. 交叉编译 Targets（第 8 章）

```powershell
rustup target add aarch64-linux-android
cargo build --target aarch64-linux-android -p im-sdk
```

阶段 6 的 im-sdk 要交付 Android/iOS/Windows/macOS/Linux 五端，
每个 target 的链接器配置、动态库后缀、MSVC vs GNU 差异
全部在这本书的 Targets 章和 Platform Support 页里。

### 3. Codegen Options（性能调优清单）

压测前 rust-im 会用的组合（全部有据可查）：

| 选项 | 作用 | 代价 |
|---|---|---|
| `-C target-cpu=native` | 用满本机指令集 | 二进制不可跨机器分发 |
| `-C lto=fat` | 全程序链接期优化 | 编译时间暴涨 |
| `-C codegen-units=1` | 单编译单元 | 编译时间涨，运行时优化更好 |
| `-C opt-level=3` | （release 默认） | — |

## 面试相关

「你怎么定位编译慢？」「clippy 和 rustc lint 什么关系？」
「交叉编译怎么配置？」——答案都在这本书，且北京 Rust 岗
（尤其跨平台 SDK 方向）真的会问。

返回 [官方文档导航](../README.md) | 前往 [中文版](https://learnku.com/docs/rustc-book/2020)
