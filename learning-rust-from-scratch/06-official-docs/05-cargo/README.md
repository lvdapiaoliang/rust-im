# 05 - The Cargo Book（构建系统与包管理）

> **原文**：<https://doc.rust-lang.org/cargo/index.html>
> **中文**：<https://rustwiki.org/zh/docs/cargo-intro/>（翻译中）+ [learnku Cargo 教程](https://learnku.com/docs/cargo-book)
> **本地**：`rustup doc --cargo`
> **规模**：4 大板块约 40 章
> **前置**：The Book 第 14 章

## 一、定位与读法

> 【Java】Cargo = Maven/Gradle + 中央仓库 + 版本管理，但**零配置文件地狱**：
> 没有 settings.xml、没有 plugin 解析冲突、没有 build.gradle 巨石。
> 代价是你必须理解它的**少数几个核心概念**（workspace/features/profiles），
> 这些概念全部在这本书里。

rust-im 已经是 9-crate workspace 的重度用户，本书是把它用明白的说明书。

## 二、四大板块结构

| 板块 | 内容 | 优先级 |
|---|---|---|
| **Getting Started** | 安装、第一个 crate | 👀（已会） |
| **Guide（指南）** | 按任务组织：依赖/测试/CI/发布 | ⭐⭐ |
| **Reference（参考）** | Manifest 格式/配置/构建脚本/features 的完整规范 | ⭐⭐⭐（查阅主力） |
| **Commands（命令）** | 每条 cargo 子命令的完整参数 | 🔍 |

## 三、Guide 板块章节导读 ⭐⭐

| 英文 | 中文 | 导读 |
|---|---|---|
| Why Cargo Exist | 为什么需要 Cargo | 哲学：约定优于配置的 Rust 实践 |
| Creating a New Package | 新建包 | `cargo new --lib/--bin`、目录布局 |
| Adding Dependencies | 添加依赖 | 版本号语义 `^`/`~`/`=`、git/path 依赖 |
| Package Layout | 包布局 | src/bin、tests、benches、examples 的约定位置 |
| Imports / Using Packages | 导入与使用 | 依赖可见性、特性（features）基本用法 |
| **Cargo Workspaces** | **工作空间** | **rust-im 的骨架**：根 Cargo.toml + [workspace.dependencies] 统一版本 |
| Testing | 测试 | `cargo test` 如何发现/隔离三种测试 |
| Continuous Integration | 持续集成 | GitHub Actions 模板 |
| **Cargo Home / Build Cache** | 缓存 | 构建缓存机制——大项目编译慢的排查起点 |

## 四、Reference 板块章节导读 ⭐⭐⭐（查阅主力）

| 英文 | 中文 | 你会在什么时候查它 |
|---|---|---|
| **The Manifest Format** | **清单格式** | Cargo.toml 每个字段的权威定义——[workspace]/[features]/[profile] 写不全时 |
| **Specifying Dependencies** | 依赖声明 | 版本需求语法、平台特定依赖（`[target.'cfg(windows)'.dependencies]`——im-sdk 跨平台用） |
| **Features** | **特性机制** | **全书最重要章节**：feature 的统一化/加法语义、feature 名与依赖名的坑——tokio 的 `full` feature 就是这么来的 |
| **Overriding Dependencies** | 覆盖依赖 | `[patch]`/`[replace]`——依赖有 bug 时本地替换 |
| **Build Scripts** | **构建脚本** | build.rs：代码生成、探测系统库（im-storage 绑定 FUSE/WinFsp 时必用） |
| **Config** | 配置 | `.cargo/config.toml`：镜像源（国内必配！）、linker、target 配置 |
| **Profiles** | 编译配置 | dev vs release、opt-level/lto/codegen-units——压测调优的第一站 |
| **Publishing** | 发布 | crates.io 发布流程（rust-im 开源发布用） |
| **SemVer Compatibility** | 语义化版本 | 什么改动算 breaking——给 im-* crate 定版本号 |
| **Dependency Resolution** | 依赖解析 | resolver v2/v3 的差异（workspace 里 `resolver = "3"` 的含义） |

## 五、Commands 板块 🔍

每条命令一页：`build`/`run`/`test`/`bench`/`doc`/`tree`/`update`/`publish`/`install`…
日常高频组合：

```powershell
cargo tree -d                # 查看重复依赖（duplicate deps）
cargo tree -i tokio          # 谁依赖了 tokio，什么 feature
cargo build --timings        # 编译耗时报告（优化编译速度）
cargo why --package=...      # （nightly）解释为什么需要这个依赖
```

## 六、与 rust-im 的关联对照

| Cargo 概念 | rust-im 的用法 |
|---|---|
| workspace | 9 crate 单仓库，根 `[workspace.dependencies]` 统一管理版本 |
| features | im-transport 的 `tls` feature、im-client 的按需裁剪 |
| profiles | 压测前 `[profile.release] lto = true` |
| build scripts | 阶段 11 的 SDK 头文件生成、阶段 13 的系统库探测 |
| config.toml | 国内镜像源 + 目标平台 linker 配置 |

## 七、国内开发者必做配置

```toml
# D:\0-develop\...\rust-im\.cargo\config.toml（或全局 ~/.cargo/config.toml）
[source.crates-io]
replace-with = 'rsproxy'

[source.rsproxy]
registry = "sparse+https://rsproxy.cn/index/"
```

返回 [官方文档导航](../README.md) | 前往 [中文版](https://rustwiki.org/zh/docs/cargo-intro/)
