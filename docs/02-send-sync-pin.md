# 02 - Send、Sync 与 Pin：多线程和 FFI 的类型关卡

> 本章目标：讲清楚「什么类型可以跨线程/跨 task 传递」「什么类型可以跨线程共享」
> 「async 里的自引用为什么需要钉住」。
> 这是岗位 JD 里点名的必问点，也是 FFI（阶段 6）最容易翻车的地方。

## 一、先看 Java：为什么 Java 没有这个问题

Java 里任何对象都可以塞给任何线程：

```java
byte[] shared = new byte[1024];
new Thread(() -> shared[0] = 1).start();  // 编译器毫无意见
new Thread(() -> shared[1] = 2).start();  // 编译器毫无意见
// 会不会出事？运行时才知道（这里是安全的，但换成 long[] 就未必）
```

Java 把线程安全完全交给运行时纪律（`synchronized` / `volatile` / `java.util.concurrent`），
编译器对「这个对象线程安全吗」一无所知。

Rust 把「线程安全性」做成**类型的编译期属性**，就是 `Send` 和 `Sync`。
好消息：99% 的场景编译器自动推导；坏消息：FFI 和底层代码里你必须知道规则，
因为**裸指针（`*mut T`/`*const T`）两者都不是**——这正是岗位 JD 里
「为什么 FFI/裸指针经常破坏 Send/Sync」的出处。

## 二、Send 与 Sync 的精确定义

两个 marker trait（无方法的空 trait，纯类型系统标记）：

```rust
// 语义：这种类型的值可以安全地「搬」到另一个线程（转移所有权）
unsafe trait Send {}

// 语义：这种类型的 &T 可以安全地「给」另一个线程（跨线程共享引用）
unsafe trait Sync {}
```

两者的关系可以一句话绑定：

> **`T: Sync` ⟺ `&T: Send`**
> （能把引用安全地交出去，等于说「多线程同时持有引用访问它」是安全的）

### 2.2 常见类型的归属表

| 类型 | Send | Sync | 原因 |
|------|------|------|------|
| `i32`、`String`、`Vec<T>` | ✅ | ✅ | 普通数据，无内部可变 |
| `Arc<T>` | ✅（若 `T: Send + Sync`） | ✅（同左） | 原子引用计数 |
| `Rc<T>` | ❌ | ❌ | 引用计数**非原子**，跨线程会计数错乱 |
| `Cell<T>` / `RefCell<T>` | ✅ | ❌ | 内部可变性无同步 |
| `Mutex<T>` | ✅ | ✅ | 锁保护一切 |
| `*mut T` / `*const T` | ❌ | ❌ | **编译器对裸指针一无所知** |
| `AtomicU64` | ✅ | ✅ | 原子操作 |

### 2.3 自动推导规则

- struct 的所有字段都 `Send` → struct 自动 `Send`（Sync 同理）
- 所以日常写业务代码几乎从不见 `unsafe impl Send`
- **一旦字段里有裸指针或 `Rc`，整个类型的 Send/Sync 就断了**

## 三、本项目现场：`Arc<AtomicU64>` 与 `DashMap`

阶段 0 的 echo server 里已经出现了一个跨 task 共享计数的例子：

```rust
// crates/im-transport/src/echo.rs
static CONNECTIONS_SERVED: AtomicU64 = AtomicU64::new(0);
// ...
let served = CONNECTIONS_SERVED.fetch_add(1, Ordering::Relaxed) + 1;
```

`AtomicU64` 是 `Send + Sync`，多个连接 task 并发 `fetch_add` 安全无锁。
这里故意没用 `Mutex<u64>`——对一个整数加一，原子指令比拿锁快一个数量级，
这是阶段 5 性能优化的基本直觉：**能用原子就别用锁，能用锁就别用全局锁**。

阶段 3 的服务端路由表将使用 `Arc<DashMap<UserId, ConnSender>>`：

- `DashMap` 内部按 key 分片加锁，并发读写路由表时不同 key 不竞争
- 包在 `Arc` 里是因为网关主循环和每个连接 task 都要持有一份

## 四、FFI 场景：裸指针如何破坏 Send/Sync（面试必考）

### 4.1 现场还原

设想阶段 6 的 SDK：Rust 持有一个 C 库（比如 OpenSSL）的句柄：

```rust
struct NativeClient {
    // OpenSSL 的 EVP_PKEY*，一个 C 裸指针
    raw: *mut evp_pkey,
}
```

编译器看到裸指针，立刻撤回 `NativeClient` 的 Send/Sync 自动实现。
为什么这么保守？因为**编译器无法知道 C 库内部对这个指针做了什么**：

- 如果 OpenSSL 的这个对象内部有非线程安全的上下文，跨线程共享 = 数据竞争
- 如果它绑定了创建它的线程（如某些 TLS 库），跨线程使用 = 未定义行为

### 4.2 正确的处理方式：显式声明 + 论证

```rust
// 前提：你【查过文档并验证】OpenSSL 对象确实可以跨线程使用（引用计数是原子的）
unsafe impl Send for NativeClient {}
unsafe impl Sync for NativeClient {}
```

`unsafe impl` 的含义不是「关闭检查」，而是**程序员向编译器立下军令状**：
「我担保跨线程使用此类型不会引发未定义行为」。担保错了，就是 UB，不是 panic。

> 这就是 JD 那句「FFI/裸指针经常破坏 Send/Sync」的完整答案：
> 不是 Rust 报错太多，而是 C 世界的线程安全契约 Rust 无法自动验证，
> 只能把验证责任交还给人。写好 `unsafe impl` 前的文档论证（Safety 注释）是 SDK 工程师的必修课。

### 4.3 反向坑：Java 调 Rust 时反过来破坏

JNI 场景中，`JNIEnv` 指向的线程局部数据只在本线程有效，把 `JNIEnv*` 存起来
跨线程使用是经典崩溃。阶段 6 实现 JNI 回调时，正确姿势是：
仅在线程内使用 `JNIEnv`，跨线程用 `JavaVM` AttachCurrentThread。

## 五、Pin：把「不许移动」写进类型

### 5.1 问题的起源：自引用结构体

```rust
struct SelfRef {
    data: String,
    // 指向自己 data 字段的指针 —— 移动整个结构体后它会指向旧地址！
    ptr: *const String,
}
```

如果这个结构体被 move（比如从栈挪进堆、从数组挪到数组），`data` 的地址变了，
`ptr` 却还是旧地址 → 悬垂指针 → UB。
Java 没这个问题：对象一旦分配在堆上就不再移动（GC 复制算法除外，但 JVM 会改引用）。

### 5.2 async fn 编译产物就是自引用结构体！

```rust
async fn echo_roundtrip(stream: TcpStream) -> io::Result<()> {
    let buf = [0u8; 4096];                    // 栈上分配在这个「状态机」的槽位里
    let n = stream.read(&mut buf).await;      // <- 挂起点：future 在这里暂停
    // 暂停期间，future 本身可能被 Tokio 在堆上挪动（任务队列迁移），
    // 而 future 内部「下一个状态要用的 &buf」就形成自引用
}
```

`async fn` 会被编译成一个状态机结构体（见 docs/03），它内部持有跨 `.await` 存活的局部变量的**指针**。所以：

> **Future 必须先被 Pin 住，才能被 poll。**
> `Pin` 的承诺：这个值在释放前不会被移动，自引用指针永远有效。

### 5.3 日常你感受不到 Pin——但必须能讲清

```rust
// 你写的：
tokio::spawn(my_async_fn());

// Tokio 内部做的（概念上）：
let mut fut = Box::pin(my_async_fn());   // 先钉到堆上，地址固定
poll(Pin::new(&mut fut), waker);         // 然后才能安全地 poll
```

分层的类型：

| 类型 | 含义 |
|------|------|
| `Pin<P>` | 包裹指针 `P`，承诺「指针指向的值不会被移动」 |
| `Unpin` | 「我不在乎被移动」的标记，绝大多数普通类型都是 |
| `!Unpin` | 自引用类型（编译出的 async 状态机），必须 Pin 才能安全使用 |
| `Pin<&mut T>` | 对 `T` 的可变借用且不许移动——`poll` 的签名就是 `fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Output>` |

面试一句话总结：**`Pin` 是 Rust 为「含自引用的类型」提供的移动禁令；async 状态机是典型代表；普通类型都是 `Unpin`，所以业务代码里几乎不感知它，但库作者（我们在阶段 2 会手写 Future）绕不开。**

## 六、动手练习

1. **亲眼看 Rc 翻车**：在 `crates/im-transport/tests/` 写：

   ```rust
   let rc = std::rc::Rc::new(1);
   std::thread::spawn(move || println!("{}", rc));
   ```

   读编译错误，把 `Rc` 换成 `Arc` 修复。理解报错信息里 `Rc<i32> cannot be sent between threads safely` 指向的是 `Send` 缺失。

2. **亲手破坏 Send**：定义 `struct Holder { p: *const u8 }`，用 `std::thread::spawn` 跨线程 move 它，观察编译器拒绝。然后加 `unsafe impl Send for Holder {}`，思考：这次承诺了什么？（答案：`p` 指向的内存跨线程访问是安全的）

3. **验证 AtomicU64 的 Sync**：写一个多线程计数测试：8 个线程每个对 `Arc<AtomicU64>` 加 1 万次，断言最终等于 8 万。再换成非原子的 `Cell<u64>` 试试（会因 `!Sync` 编译失败）。

## 七、面试题与标准回答

**Q1：Send 和 Sync 是什么？什么时候 struct 不能自动实现？**

> Send 是「值可跨线程转移所有权」的标记，Sync 是「&T 可跨线程共享」的标记，关系是 `T: Sync ⟺ &T: Send`。
> struct 的所有字段都满足时自动实现；一旦含裸指针、`Rc`、`RefCell` 等非线程安全类型，自动实现即断。
> 我在 IM SDK 里包 C 库句柄时遇到过：字段是 `*mut evp_pkey`，整个 struct 失去 Send/Sync，
> 必须在查证 C 库线程安全契约后 `unsafe impl` 并写明 Safety 论证。

**Q2：FFI 场景下 Send/Sync 有哪些坑？**

> 三个层面：① 裸指针默认不 Send/Sync，包裹 C 句柄的 struct 需要显式 unsafe impl，前提是核实 C 库文档的线程安全承诺；② 某些 C 库对象绑定创建线程（如 JNI 的 JNIEnv），跨线程要用它提供的线程 attach 机制；③ Rust 分配的内存交给 C 后，所有权已经越过类型系统，必须靠「谁分配谁释放」的 API 约定（阶段 6 的 im_sdk_free 家族）兜底，这已经不是类型能保护的领域。

**Q3：Pin 的作用？自引用结构体为什么必须 Pin？**

> move 会改变值地址，自引用指针在 move 后悬垂，构成 UB。async fn 编译出的状态机内部持有跨 await 的借用（本质是指向自身字段的指针），是典型自引用类型。Pin 把「此值不许再移动」编码进类型，Future::poll 因此要求 `self: Pin<&mut Self>`——先保证地址稳定，才允许 poll 内部访问自引用字段。

**Q4：什么是 Unpin？为什么日常业务代码很少接触 Pin？**

> Unpin 是「不在乎被移动」的标记，String、Vec、普通 struct 都是 Unpin，对它们 Pin 是零开销透传。业务代码只在两处感知 Pin：调用 `.await`（编译器自动处理）和 spawn（Tokio 内部 Box::pin）。但实现自定义 Future 或 intrusive 链表这类结构时，Pin 语义必须由作者手工维护，我在阶段 2 手写 Future 时会具体实践。

## 下一章

[03-async-tokio.md](./03-async-tokio.md)——Send/Sync 是「能不能跨线程」，
async/Tokio 是「怎么高效调度百万任务」；两者在 `tokio::spawn` 的 `'static + Send` 门槛处汇合。
