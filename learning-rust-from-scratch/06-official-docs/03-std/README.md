# 03 - The Rust Standard Library（标准库文档）

> **原文**：<https://doc.rust-lang.org/std/index.html>
> **中文**：<https://rustwiki.org/zh/std/>（完整中文版，术语与社区一致）
> **本地**：`rustup doc --std`——**日常开发最高频入口，建议固定为浏览器书签**
> **规模**：数万个 API 条目，不可能「读」——本文教你**怎么查、查什么、按什么顺序学**

## 一、先搞清四个 crate：std 不是全部

Rust 的「标准库」实际是四个 crate：

| crate | 内容 | 何时用 |
|---|---|---|
| `core` | 无操作系统依赖的内核：类型/迭代器/Option/Result… | no_std（嵌入式、内核）——你现阶段不用管 |
| `alloc` | 需要分配器的部分：Box/Vec/String/Rc… | no_std 但有堆时 |
| `std` | 全家桶（re-export 了上面两个 + OS 层） | **99% 场景，默认就是它** |
| `proc_macro` | 过程宏 API | 写宏时（阶段 G 以后） |

> 【Java】类比：`core`≈`java.lang` 里的纯逻辑部分，`std`≈JDK 全量。
> 写 `use std::collections::HashMap` 时，HashMap 其实来自 `alloc`，
> std 只是转发——文档页左侧能看到「Re-exported from alloc」。

## 二、std 模块地图（按你的学习/求职优先级分组）

### 第一梯队：天天用（对应 01-basics / 02-core）

| 模块 | 内容 | 学习系列对照 |
|---|---|---|
| `std::collections` | Vec/HashMap/BTreeMap/VecDeque/**BinaryHeap** | [core/08](../../02-core/08-集合迭代器闭包.md)、[algorithms/03-04](../../04-algorithms/) |
| `std::fmt` | Display/Debug、格式化宏的原理 | basics/02 |
| `std::option` / `std::result` | Option/Result 全部方法 | basics/03,05 |
| `std::iter` | Iterator 全部适配器 | [core/08](../../02-core/08-集合迭代器闭包.md) |
| `std::string` / `str` | String/&str、UTF-8 处理 | core/08 |
| `std::cmp` / `std::ops` | Ord/PartialOrd、运算符重载、Range | basics/02-03 |
| `std::convert` | From/Into/TryFrom/AsRef——适配器模式标准化 | [patterns/02](../../05-patterns/02-创建型与结构型.md) |

### 第二梯队：并发与异步（对应 03-tokio，求职重点）

| 模块 | 内容 | 备注 |
|---|---|---|
| `std::sync` | Mutex/RwLock/**atomic**/mpsc/Once/Arc 所在地 | tokio/06 讲 std vs tokio 锁的选型 |
| `std::thread` | 线程、JoinHandle、线程局部存储 | The Book 16 章 |
| `std::future` / `std::task` | Future trait、Context/Waker、RawWaker | **tokio/02 的原始出处** |
| `std::pin` | Pin/Unpin——async 自引用的守护者 | [docs/02](../../../docs/02-send-sync-pin.md) |
| `std::time` | Instant/Duration/SystemTime | 超时/心跳的基础 |

### 第三梯队：rust-im 项目核心依赖

| 模块 | 内容 | rust-im 用途 |
|---|---|---|
| `std::net` | TcpListener/TcpStream/UdpSocket/SocketAddr | 阶段 2 传输层（tokio 版是其异步镜像） |
| `std::io` | Read/Write/BufReader/Error/ErrorKind | echo 服务器已用；`ErrorKind::WouldBlock` 是 epoll 世界的钥匙 |
| `std::fs` / `std::path` / `std::os` | 文件/路径/平台特定 API | 挂载盘阶段 8 |
| `std::process` | 子进程、ExitStatus | xtask 构建脚本 |
| `std::mem` | size_of/take/replace/swap | 算法系列的常客 |
| `std::cell` | Cell/RefCell/OnceCell | 内部可变性 |
| `std::marker` | PhantomData/Send/Sync | Nomicon 深水区 |
| `std::borrow` / `std::rc` / `std::boxed` | Cow/Rc/Box | [core/09](../../02-core/09-智能指针.md) |

## 三、怎么读 std 文档页（5 个被忽略的神器）

1. **`[src]` 链接**：每个条目右上角——标准库源码就是最好的 Rust 教材
   （比如看 `Vec::push` 如何处理扩容，直接印证 [algorithms/01](../../04-algorithms/01-复杂度分析与Vec.md)）
2. **Trait 展开**：条目左侧的小三角 ▸ 展开实现的所有 trait——
   看到 `u8` 实现了 50 个 trait 就明白为什么不需要记 API
3. **搜索**：顶部搜索框支持 `Vec::push`、`push`（带签名提示）和模糊匹配；
   前缀 `struct:`/`fn:` 可限定类型
4. **「Auto Trait Implementations」段**：看一个类型是否 Send/Sync/Unpin——
   判断「能不能跨线程/能不能进 spawn」直接查这里，不用推导
5. **doc aliases**：搜 `hashmap` 也能到 `HashMap`——官方埋了大量别名

## 四、学习路径建议

```
第 1 步：通读 Vec/HashMap/Option/Result 的方法列表（不求记住，求「知道存在」）
第 2 步：Iterator 适配器家族（map/filter/take/skip/collect）——core/08 的实战
第 3 步：io::Read/Write trait 层次（rust-im 传输层的地基）
第 4 步：sync::atomic 与 Ordering（tokio/06 的 std 侧）
第 5 步：其余全部「用到再查」，靠 rustup doc --std 肌肉记忆
```

> 【Java】使用心法与 Javadoc 相同：**类型页看 trait 实现，方法页看 Examples**。
> 区别是 std 文档的示例全部是可运行代码（doctest），
> 而 Javadoc 示例常常过期。

## 五、中文版使用建议

- rustwiki 中文版适合**通读理解**（母语效率高）
- **版本敏感的 API 细节**以本地 `rustup doc --std` 为准
  （中文版可能滞后，例如 edition 2024 相关的新 API）

返回 [官方文档导航](../README.md) | 前往 [中文版](https://rustwiki.org/zh/std/)
