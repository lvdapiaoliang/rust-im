//! `cargo xtask sdk`：SDK 打包任务（阶段 11）。
//!
//! 一条命令产出 `dist/sdk/`，C/Java 调用方拿到即可用：
//!
//! ```text
//! dist/sdk/
//! ├── README.md          产物说明 + 快速开始（本任务生成）
//! ├── include/
//! │   └── im_sdk.h       C 头文件（错误码/事件码/函数原型 + 契约注释）
//! ├── java/
//! │   └── im/sdk/        JNI 绑定源码（Sdk.java + Demo.java）
//! └── lib/
//!     ├── im_sdk.dll     宿主平台动态库（Windows MSVC）
//!     │   (+ im_sdk.dll.lib 导入库，C 调用方链接用)
//!     └── <target>/      交叉编译产物（--target 指定，按三元组分目录）
//! ```
//!
//! # 为什么构建必须显式 `--features jni`
//!
//! 阶段 11 实测踩坑：cargo 的构建缓存按 feature 集**整体区分**——先
//! `cargo build --features jni` 产出带 JNI 导出的 dll，再 `cargo run
//! --example demo_server`（不带 jni）会把同一个 dll **静默覆盖**成无 JNI
//! 导出的版本，Java 侧表现为 `UnsatisfiedLinkError`（库加载成功、符号
//! 找不到——比"库不存在"迷惑得多）。SDK 打包必须自己钉死 feature 集。

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// workspace 根目录：`crates/xtask` 的上两级（编译期定死，不依赖运行环境）。
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// 打包入口。
///
/// `targets` 为 `--target` 重复参数指定的交叉编译三元组；只构建本机已
/// 安装的 target——没装的诚实跳过并给出安装命令，**绝不假装产出**。
pub fn run(targets: &[String]) -> Result<()> {
    let root = Path::new(ROOT);

    // ── 1. 构建宿主平台动态库（feature 集钉死，见模块文档）──
    run_cargo(&["build", "-p", "im-sdk", "--release", "--features", "jni"])?;
    copy_platform_artifacts(&root.join("target/release"), &root.join("dist/sdk/lib"))
        .context("拷贝宿主平台产物")?;

    // ── 2. 头文件 + Java 源码（跨平台共享，直接拷）──
    copy_tree(&root.join("crates/im-sdk/include"), &root.join("dist/sdk/include"))
        .context("拷贝 im_sdk.h")?;
    copy_tree(&root.join("crates/im-sdk/java"), &root.join("dist/sdk/java"))
        .context("拷贝 Java 绑定源码")?;

    // ── 3. 交叉编译：先查 rustup 已装 target，缺的跳过并提示 ──
    let installed = installed_targets()?;
    for target in targets {
        if !installed.iter().any(|t| t == target) {
            println!("跳过 {target}：本机未安装该 target（`rustup target add {target}` 后重跑）");
            continue;
        }
        println!("==> 交叉编译 {target}");
        run_cargo(&[
            "build",
            "-p",
            "im-sdk",
            "--release",
            "--features",
            "jni",
            "--target",
            target.as_str(),
        ])?;
        copy_platform_artifacts(
            &root.join("target").join(target).join("release"),
            &root.join("dist/sdk/lib").join(target),
        )
        .with_context(|| format!("拷贝 {target} 产物"))?;
    }

    // ── 4. 产物清单 README（不手写：打包时生成，避免与实际产物漂移）──
    write_readme(&root.join("dist/sdk"))?;
    println!("==> SDK 已打包到 dist/sdk/");
    Ok(())
}

/// 跑 cargo（继承 stdio：构建输出直接透传，与手动执行观感一致）。
fn run_cargo(args: &[&str]) -> Result<()> {
    let status = Command::new("cargo").args(args).status()?;
    anyhow::ensure!(status.success(), "cargo {} 失败：{status}", args.first().unwrap_or(&""));
    Ok(())
}

/// 把 `release_dir` 里的 cdylib 产物拷到 `out_dir`，返回是否找到产物。
///
/// 三种平台命名一起找（Windows `.dll` + `.lib` / Linux `.so` / macOS `.dylib`）——
/// 这段代码本身要在任何 host 上都能跑，不能只写死 Windows。
fn copy_platform_artifacts(release_dir: &Path, out_dir: &Path) -> Result<()> {
    // (文件名, 是否主产物)——.lib/.exp 是 MSVC 的导入库副产品，一并带走
    const CANDIDATES: &[(&str, bool)] = &[
        ("im_sdk.dll", true),
        ("im_sdk.dll.lib", false),
        ("im_sdk.dll.exp", false),
        ("libim_sdk.so", true),
        ("libim_sdk.dylib", true),
    ];
    std::fs::create_dir_all(out_dir)?;
    let mut found = false;
    for (name, primary) in CANDIDATES {
        let src = release_dir.join(name);
        if src.exists() {
            std::fs::copy(&src, out_dir.join(name))
                .with_context(|| format!("拷贝 {}", src.display()))?;
            if *primary {
                found = true;
            }
        }
    }
    // 主产物一个都没有 = 构建配置出了问题（比如 crate-type 丢了 cdylib）
    anyhow::ensure!(found, "{} 里没有任何 cdylib 产物", release_dir.display());
    Ok(())
}

/// 递归拷贝目录（保留相对结构；SDK 的头文件/Java 源都是小树，无需过滤）。
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("读取 {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::create_dir_all(dst)?;
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("拷贝 {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// rustup 已安装的 target 三元组列表（`rustup target list --installed`）。
fn installed_targets() -> Result<Vec<String>> {
    let out = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .context("启动 rustup 失败")?;
    anyhow::ensure!(out.status.success(), "rustup target list 失败：{}", out.status);
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .map(str::to_owned)
        .filter(|l| !l.is_empty())
        .collect())
}

/// 生成 `dist/sdk/README.md`（简短清单 + 指向 docs/17）。
fn write_readme(dist: &Path) -> Result<()> {
    let text = "\
# rust-im FFI SDK

由 `cargo xtask sdk` 生成（构建参数：`--release --features jni`）。

## 目录

- `include/im_sdk.h`：C 头文件——错误码/事件码/`im_sdk_event_t`/7 个函数原型，
  三条跨语言契约（内存/线程/错误）写在注释里
- `java/im/sdk/`：JNI 绑定源码（`Sdk.java` 绑定类 + `Demo.java` 冒烟演示）
- `lib/`：宿主平台动态库（Windows 为 `im_sdk.dll` + 导入库 `im_sdk.dll.lib`）；
  交叉编译产物按 target 三元组分目录

## 快速开始（Java 冒烟）

```text
javac -d out java/im/sdk/*.java
cargo run -p im-sdk --release --example demo_server   # 另开一个终端（127.0.0.1:18888）
java -Djava.library.path=lib -cp out im.sdk.Demo
```

完整契约与设计取舍见仓库 `docs/17-ffi.md`。
";
    std::fs::write(dist.join("README.md"), text).context("写 dist/sdk/README.md")
}
