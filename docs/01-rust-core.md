# 01 - Rust 核心：所有权、借用与生命周期（Java 工程师视角）

> 本章目标：读完并做完练习后，你应当能不假思索地回答——
> 「一段内存什么时候释放？谁有资格读写它？引用会不会失效？」
> 这三个问题就是 Rust 与 Java 最根本的思维差异。

## 一、为什么 Java 工程师学 Rust 会有「世界观冲击」

Java 的内存模型一句话概括：**对象分配在堆上，谁想用就留个引用，没人用了 GC 来收**。
你从不需要回答「这块内存归谁」——因为答案是「归运行时」。

代价是：

1. **GC 停顿**：IM 服务端要扛百万长连接、微秒级消息转发，任何 STW 都是灾难
2. **别名可变性无约束**：同一个对象可以被任意线程、任意方法同时改，正确性靠纪律（`synchronized`、`volatile`）保证，编译器不帮你
3. **资源释放时机不确定**：`close()` 要靠 try-with-resources，忘了就是连接泄漏

Rust 把这三个问题的答案全部移到了**编译期**：

| 问题 | Java 的答案 | Rust 的答案 |
|------|-------------|-------------|
| 内存何时释放 | GC 决定（不确定） | 所有权离开作用域时立即释放（确定） |
| 谁能读写 | 任何拿到引用的人 | 借用检查器在编译期裁决 |
| 引用是否有效 | 运行时可能 NPE / ConcurrentModificationException | 悬垂引用**无法编译通过** |

> 面试金句：Rust 不是「安全的 C++」，而是「把 Java 运行时的 GC 和线程检查，
> 换成了编译期的所有权检查」——用编译时间换运行时的确定性。

## 二、所有权：每个值恰好有一个主人

### 2.1 move 语义：赋值 = 转让，不是复制引用

```rust
fn main() {
    let s1 = String::from("hello");
    let s2 = s1;            // 所有权从 s1 move 到 s2
    // println!("{s1}");    // 编译错误！s1 已经失效
    println!("{s2}");       // OK：s2 是唯一主人
}
```

对照 Java：

```java
String s1 = "hello";
String s2 = s1;   // 两个引用指向同一个对象，都能用
```

**关键理解**：`String` 内部是 `(指针, 长度, 容量)` 三元组，存在栈上，指向堆上字符串。
`let s2 = s1` 复制的是这个三元组——如果两个三元组都有效，函数结束时堆内存会被释放两次。
Rust 的解法：move 之后原变量**静态失效**（编译器标记为「已被移动」），你再用它直接编译错误。
这就是 Rust 不需要引用计数式 GC 的根本原因：**任何时刻每个堆内存都有唯一确定的释放者**。

### 2.2 作用域结束 = 自动 Drop

```rust
fn demo() {
    let stream = TcpStream::connect("127.0.0.1:8080").await?;
    // ... 使用 stream ...
}   // <- stream 在这里自动 drop，socket 关闭。没有 finally，没有 close()
```

这就是本项目 `echo.rs` 里 `run_echo_client` 的写法——
注释里特意写了「无论正常返回还是 Err 提前返回，Drop 自动关闭 socket」，
等价于 Java 的 try-with-resources，但是**由类型系统保证**而非语法糖约定。

### 2.3 Copy 类型：例外的小家伙

```rust
let a = 42;
let b = a;          // i32 是 Copy 类型：按位复制，a 依然有效
println!("{a} {b}"); // OK
```

规则：**栈上完全自包含、复制代价极小的类型**（整数、浮点、bool、char、
以及由它们组成的元组/定长数组）自动实现 `Copy`。
`String`、`Vec`、`TcpStream` 这类持有堆资源或系统资源的类型**绝不 Copy**，
只能 move 或借用。

对照 Java：Java 的 `int` 与 `Integer` 的装箱区分，恰好是 Copy 与堆类型的类比。

## 三、借用：不拿所有权，只拿使用权

### 3.1 借用规则（背下来，面试必考）

任意时刻，对同一块内存：

1. **要么**任意多个不可变引用 `&T`
2. **要么**恰好一个可变引用 `&mut T`
3. 两者**绝不能同时存在**
4. 引用必须始终有效（不允许悬垂引用）

为什么这么严？这不是为了折磨人，而是精确复刻了数据竞争的定义：

> 数据竞争 = 至少两个访问者 + 至少一个写者 + 无同步措施。
> 规则 1+2 直接让「两个写者」和「读写共存」在编译期不可能出现。

### 3.2 用 Java 的翻车现场对照

Java 里这段代码能编译能运行，然后在生产环境某个深夜抛 `ConcurrentModificationException`：

```java
List<String> list = new ArrayList<>(...);
for (String s : list) {
    if (s.isEmpty()) list.remove(s);   // 迭代中修改，运行时才炸
}
```

Rust 等价代码**编译不过**：

```rust
let mut list = vec!["a".to_string(), String::new()];
for s in &list {                 // 不可变借用开始（迭代器持有 &list）
    if s.is_empty() {
        list.remove(0);          // 编译错误：不能在存在 &list 时可变借用 list
    }
}                                // 不可变借用结束
```

### 3.3 借用就是「带期限的视图」

本项目的 echo 服务器里有一行注释专门讲这个：

```rust
// crates/im-transport/src/echo.rs（serve_connection 内）
stream.write_all(&buf[..n]).await?;
```

`&buf[..n]` 是**切片借用**：把 `buf` 的前 `n` 个字节「借」给 `write_all`，
用完归还，所有权始终在 `serve_connection` 手里，所以下一轮循环还能继续用。
最接近的 Java 概念是 `ByteBuffer.flip()` + `position/limit`——
但 Java 里你可以 flip 错、可以越界；Rust 里切片的长度信息在类型里，越界直接 panic（且边界检查有优化）。

### 3.4 NLL（非词法作用域生命周期）

借用什么时候结束**不再看大括号**，而是看「最后一次使用」：

```rust
let mut s = String::from("hi");
let r = &s;             // 借用开始
println!("{r}");        // r 的最后一次使用 —— 借用在此结束
s.push_str("!");        // OK：此刻已没有任何 &s 借用存活
```

这叫 NLL（Rust 2018 起默认）。如果按老规则（词法作用域到块尾），
上面代码要报错，写起来会痛苦得多。

## 四、生命周期：引用有效期写在类型里

### 4.1 为什么需要生命周期标注

生命周期不改变任何行为，**它只是给编译器的证明材料**：
「这个引用至少能活到那个引用失效为止」。

经典案例——悬垂引用，编译器直接拒绝：

```rust
fn dangle() -> &String {            // 编译错误：返回的引用指向哪？
    let s = String::from("hi");
    &s                               // s 在函数结束被 drop，引用悬垂
}
```

### 4.2 生命周期省略规则（面试高频）

函数签名上满屏 `'a` 很吓人，其实 90% 的场景编译器能自动推断，依据三条省略规则：

1. 每个引用类型的**输入**参数各自获得独立的生命周期
2. 只有一个输入引用时，所有输出引用的生命周期 = 它
3. 方法（含 `&self`/`&mut self`）的输出引用生命周期 = `self` 的

项目中的实例（阶段 1 将实现）：

```rust
// 只有 &self 一个输入引用 → 规则 3：返回值的生命周期自动绑定 self，无需标注
impl FrameCodec {
    fn decode(&mut self, buf: &mut BytesMut) -> Option<Frame> { ... }
}

// 两个输入引用、输出引用依赖第二个 → 省不掉，必须写出来
fn copy_within<'a>(src: &'a [u8], dst: &mut [u8]) -> &'a [u8] { ... }
```

**面试表述**：生命周期标注不是「声明引用能活多久」，而是**描述多个引用之间的相对关系**；
真正的存活范围由使用位置决定，标注只是满足编译器的约束求解。

### 4.3 `'static`：能活到程序结束

```rust
let s: &'static str = "hello";   // 字符串字面量内嵌在二进制里，天然 'static
```

高频误区：`'static` 不等于「全局变量」，而是「**不依赖任何局部作用域**」。
后面 async 章节会看到 `tokio::spawn` 要求 future 是 `'static`——
这是 Rust 异步最让人头疼的门槛，本质是「spawn 出去的任务可能活得比当前函数久，
所以它捕获的一切都必须归它所有（move），不能只借用局部变量」。

## 五、本项目代码走读：echo.rs 中的所有权现场

打开 `crates/im-transport/src/echo.rs`，找到这五处，对照理解：

| 位置 | 代码 | 讲的知识点 |
|------|------|-----------|
| `run_echo_client` | `let mut stream = TcpStream::connect(addr).await?;` | 所有权 + RAII（Drop 即关闭） |
| `run_echo_client` | `let mut echoed = vec![0u8; payload.len()];` | 堆分配归当前函数所有 |
| `serve_connection` | `let mut buf = [0u8; READ_BUF_SIZE];` | 栈上定长数组，零堆分配 |
| `serve_connection` | `stream.write_all(&buf[..n])` | 切片借用：借出去、还回来 |
| `spawn_echo_server_on_random_port` | `tokio::spawn(run_echo_server(listener));` | move 语义：listener 所有权交给后台 task（spawn 要求 `'static`） |

特别注意最后一行：**如果不理解 move，就理解不了 spawn**。
`listener` 是局部变量，但 `tokio::spawn` 要求任务自包含（`'static`），
所以必须把所有权交出去——Java 工程师在这里最常见的报错
`borrowed value does not live long enough` 就是这个场景。

## 六、动手练习

在 `crates/im-transport/tests/` 下新建 `ownership_drill.rs`（测试文件本身就是 Rust 的学习沙盒），
完成以下练习，每题先预测编译结果再运行验证：

1. **move 预测**：`let s1 = String::from("im"); let s2 = s1;` 之后打印 `s1`——能编译吗？把 `String::from("im")` 换成 `&str` 字面量呢？
2. **借用冲突**：写一个函数同时持有 `&v` 和 `v.push(...)`，看编译器报什么；用「提前结束借用」（NLL）修复它。
3. **悬垂引用**：仿照 4.1 的 `dangle()` 写一个返回局部 `Vec` 元素引用的函数，读编译错误信息，翻译成人话。
4. **改本项目代码**：把 `serve_connection` 里的 `stream.write_all(&buf[..n])` 改成先 `let owned = buf.to_vec()` 再写 `&owned`——功能一样，但每轮循环多一次堆分配。用 `cargo bench` 或简单计时感受差异（这也是阶段 5 性能调优的第一课：**借用不只是安全，还是零拷贝**）。

## 七、面试题与标准回答

**Q1：讲讲 Rust 的所有权机制，它解决了什么问题？**

> 每个值有唯一的所有者，值被 move 时原变量静态失效，所有者离开作用域时值自动 drop。
> 它在编译期解决了 Java 靠 GC 解决的内存安全、以及 GC 解决不了的内存泄漏（如全局缓存持有）和数据竞争问题。
> 在我做的 IM 项目里，连接（TcpStream）、缓冲区的所有权清晰划分给每个连接 task，
> 连接断开即释放，不存在「忘记 close」「引用残留」这类问题。

**Q2：`&T` 和 `&mut T` 能同时存在吗？为什么？**

> 不能。这是借用规则的核心：多个只读借用或唯一一个可写借用。
> 原因是它直接消解了数据竞争的定义——两个写者或读写共存。
> 在实际编码里我碰到过迭代中修改集合的场景，Rust 编译期就拒绝，而 Java 是运行时 ConcurrentModificationException。

**Q3：生命周期标注是什么？什么时候必须写？**

> 生命周期是引用类型的组成部分，描述引用之间的相对存活关系，不是改变运行时行为。
> 三条省略规则覆盖大多数场景；当有多个输入引用且输出引用的生命周期无法唯一确定时必须显式标注。
> 我在协议解码器里遇到的实际例子是 `decode(&mut self, buf: &mut BytesMut)`，
> 靠省略规则 3 自动绑定，无需标注。

**Q4：`'static` 是什么意思？`tokio::spawn` 为什么要求 `'static`？**

> `'static` 表示引用不依赖任何局部作用域，能活到程序结束。
> spawn 的任务生命周期不受调用方控制（可能在当前函数返回后仍在跑），
> 所以它捕获的一切必须拥有所有权而不是借用局部变量——
> 我在 echo server 里 spawn 后台任务时，把 `TcpListener` 直接 move 进任务，就是这个原因。

**Q5：Rust 的 Drop 与 Java 的 finalize / try-with-resources 区别？**

> Drop 是确定性的：离开作用域立即执行，顺序确定，且编译器保证（panic 时也通过 unwinding 执行）。
> finalize 时机不确定且已被废弃；try-with-resources 是语法约定，忘了写就没有保护。
> 我的项目里 TcpStream/File/SqliteConnection 全部依赖 Drop，业务代码里几乎没有手动 close。

## 下一章

[02-send-sync-pin.md](./02-send-sync-pin.md)——所有权解决了单线程问题；
多线程和 FFI 的安全边界，由 `Send`/`Sync`/`Pin` 把守。
