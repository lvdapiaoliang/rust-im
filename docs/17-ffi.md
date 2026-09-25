# 17 - FFI SDK：C ABI / JNI / 跨语言内存契约

> 阶段 11 交付物：`im-sdk` crate——workspace 里第一个**给别的语言用**的
> crate。所有权与 Send/Sync 的边界（docs/02 的理论）在这里全部变成
> 编译器逼你直面约束的实战：裸指针进闭包、跨线程回调、谁分配谁释放。
> Java 工程师读这章会有旧相识的既视感——JNI 的坑你们踩过，Rust 侧
> 只是同一批坑换了更严格的对账方式。

## 一、本章目标

把阶段 3~10 打磨过的 `im-client`（重连/重传/去重/本地库）包成三种
消费形态，**异步内核一行不改**：

| 调用方 | 形态 | 交付物 |
|---|---|---|
| C / C++ / 任意能调动态库的语言 | C ABI（`extern "C"` + 不透明句柄） | `im_sdk.dll/.so/.dylib` + `im_sdk.h` |
| Java / Kotlin（Android 预演） | JNI（feature `jni` 门控） | 同一个动态库 + `Sdk.java` |
| Rust 宿主（阶段 12 Tauri） | 原生 rlib | `im-sdk` 直接依赖 |

验证口径：`cargo test -p im-sdk` 12 项全绿 + JDK 27 真机冒烟
（`Demo.java` 连真实 TCP 服务端收发消息，§六）。

## 二、概念：C ABI 与三条跨语言契约

C ABI 是语言间的「最小公约数」：一个动态库、一堆 `extern "C"`
函数、内存就是字节。它没有类、没有异常、没有垃圾回收——所以**一切
Rust 的安全保证到 ABI 边界都归零**，必须换成写下来的契约：

1. **内存契约（谁分配谁释放）**：
   - 入参字符串：SDK 在调用期间**立即拷贝**，指针寿命到函数返回为止
     （调用方想复用想释放随意）；
   - SDK 分配的事件结构：**只能用 SDK 的 `im_sdk_event_free` 回收**——
     C 的 `free`/Java 的 GC 都不认识 Rust 的分配器；
   - SDK 返回的静态字符串（版本号/错误描述）：进程常驻，**不许 free**。
2. **线程契约**：句柄可跨线程传递；`destroy` 不得与其他调用并发
   （一次 join，join 两次是未定义行为）；回调发生在 SDK 内部的事件泵
   线程——**回调里不许调 close**（destroy 内部要 join 泵线程，泵里
   调 destroy = 让线程 join 自己，必死锁）。
3. **错误契约**：不 panic 穿越 FFI（`extern "C"` 边界 panic = 进程
   abort）；一切失败只有稳定的 `i32` 返回码，未知码兜底
   `"unknown error code"`——**向前兼容**是错误码模型的第一性原理
   （新版本加错误码，旧调用方的 `switch` 不会走到崩溃分支）。

Java 对照：这三条契约在 JNI 世界里有现成的翻译——内存契约变成
「数据拷进 JVM 堆、GC 管生命周期」，线程契约变成 AttachGuard +
GlobalRef，错误码变成异常（§4.4）。

## 三、架构：四层分明，unsafe 只住一层

```text
C/Java 宿主
    │
    ▼
ffi.rs   ← 全部 unsafe 集中地：指针判空、Box::into_raw/from_raw、事件泵
    │        （7 个 extern "C" 导出 + JNI 需要的装配件）
    ▼
core.rs  ← 纯安全代码：SdkClient 同步外观（每个实例一个专属 tokio
    │        Runtime，block_on 桥接），事件归一化，pending 队列
    ▼
jni.rs   ← feature "jni" 门控：C ABI 的「Java 糖衣」（GlobalRef/
    │        线程 attach/modified UTF-8 处理），不碰裸指针所有权
    ▼
im-client（异步内核，零改动——SDK 是它上面的一件外套）
```

关键取舍：

- **为什么不用 cbindgen**：API 只有 7 个函数、1 个结构、两组码表，
  手写 `im_sdk.h`（116 行）配 12 项回归测试，比引入生成器管线便宜；
  API 变大后该换 cbindgen——**工具选型跟规模走**；
- **宽结构而不是 tagged union**：`im_sdk_event_t` 的 8 个事件变体
  共用一套字段（`type_` 判别 + `data` 随类型变语义），换来
  「一个 free 函数管所有变体」——union 要每个变体配一套访问纪律，
  C 调用方配错的概率远大于浪费几个字段的内存；
- **`data` 用 `Box<[u8]>` 跨边界**：free 侧凭 `(ptr, len)` 无损重建
  （`Vec` 还需要 `capacity`，跨 FFI 传不了——这是「为什么不是 Vec」
  的完整答案）；空载荷事件 `data = NULL`，不是空指针坑而是契约分支；
- **轮询与回调互斥**：`create` 传回调则事件接收端被事件泵取走
  （`take_events`），此时再 `poll` 直接返回 `ERR_POLL_WITH_CALLBACK`——
  **互斥不是文档约定，是「接收端已被 move」的物化**，编译器级别的
  保证免费拿到。

## 四、代码走读

### 4.1 错误码模型（error.rs）

6 个错误码 + `error_string(i32) -> &'static str`：const 数组覆盖
`0..=ERR_INTERNAL`，越界 chain 到兜底串。Java 对照：这本质上是把
`Exception` 的类层次压平成 `int`——压平丢掉了结构，换来**跨语言
稳定**（int 是所有 ABI 都有的东西）。错误码一旦发布就是 API，只能加
不能改语义。

### 4.2 同步外观（core.rs）：block_on 桥与 timeout 三层语义

每个 `SdkClient` 自带一个专属 `tokio::Runtime`，同步入口内部
`block_on`。**这意味着 SDK 不能在 tokio 上下文里调**（"Cannot start
a runtime from within a runtime" 直接 abort 进程）——而这不是缺陷：
真实 C/Java 调用方**没有环境 runtime**，测试也照此形态写（普通
`#[test]` + 服务端挂独立 runtime 的 `TestServer` 结构）。

`poll_event` 的超时语义是最容易写反的地方，结果要剥三层：

```rust
match timeout(d, events.recv()).await {
    Ok(Some(ev))  => /* 收到事件 */,
    Ok(None)      => /* recv 完成但通道关闭 —— 客户端已停 */,
    Err(_Elapsed) => /* 超时，暂时没有事件 */,
}
```

`Ok(None)` 和 `Err` 弄反的版本编译完全通过、测试当场抓住（通道关闭
被当成超时，`ERR_STOPPED` 永远发不出来）。完整踩坑实录见 docs/20。

### 4.3 事件泵（ffi.rs）：Send 包装与闭包捕获的三连败

回调模式起一条专职线程泵事件：`blocking_recv → alloc → callback →
free`，一事件一闭环，**作用域即契约**。难点是 `user_data: *mut c_void`
要进 `spawn` 的闭包——裸指针不是 Send，三连败后才找到唯一可靠形态：

1. 包装 `struct UserData(*mut c_void)` + `unsafe impl Send`——
   **失败**：闭包体里写 `user_data.0`，RFC 2229 精确捕获只捕获**字段
   路径**（还是那个裸指针），Send 白做；
2. 改成模式解构 `let UserData(p) = user_data`——**失败**：解构同样
   归一化为字段路径捕获；
3. **成功**：泵循环体提成独立函数 `pump_loop(events, callback,
   user_data)`，闭包只写 `move || pump_loop(...)`——结构体按值
   **传参**，闭包被迫捕获完整结构体（连带 Send 资格）。

教训：**类型系统的意图推理（捕获什么）和直觉的字面推理（我用了
整个结构体）可以不一致**，唯一的解法是让「使用形态」无法被拆开。
这个案例是 docs/02（Send/Sync）从理论变实战的最佳素材。

### 4.4 JNI 层（jni.rs）：三件套与内存语义翻译

JNI 的三个真实工程点（每个都是独立的知识点）：

- **线程 attach**：SDK 事件泵是普通线程，JVM 不认识——回调前
  `attach_current_thread()`（AttachGuard，drop 自动 detach，DerefMut
  暴露 JNIEnv）。每条事件 attach/detach 一次有成本，事件量大的 SDK
  换 `attach_current_thread_permanently`（本阶段取舍见 §七）；
- **GlobalRef**：局部引用出不了原生调用帧，泵线程要长期持有 Java
  回调对象必须 `new_global_ref`，且最终**显式 delete**（GC 不替你管
  native 侧的全局引用）。回收顺序即安全：`nativeClose` 先
  destroy（内部 join 泵）再回收事件桥——泵还活着就回收它就是
  use-after-free；
- **modified UTF-8**：裸 `GetStringUTFChars` 给的是 CESU-8 变体
  （NUL 双字节、增补字符非标准），与真 UTF-8 不兼容——JNI 最著名
  的陷阱。jni-rs 的 `get_string` 内部经 cesu8 解回标准 UTF-8，取内容
  用 `String::from(JavaStr)`。

JNI 层的全部价值是**契约翻译**：C 事件 + free 义务 → Java `Event`
对象（拷进 JVM 堆，GC 管生命周期，Java 侧零 free）；C 错误码 → 异常；
C 超时 → `null`（「暂时没有」不是异常，Java 侧的语义判断）。
Java 侧 `Sdk.close()` 的幂等（synchronized 先 swap 0 再进 native）与
native 侧 handle 非空守卫，构成 double free 的双保险。

## 五、打包：xtask sdk 与 dist/sdk

`cargo xtask sdk [--target <triple>]...` 一条命令产出：

```text
dist/sdk/
├── README.md          产物说明（打包时生成，不与实际产物漂移）
├── include/im_sdk.h   C 头文件（契约写在注释里）
├── java/im/sdk/       Sdk.java + Demo.java
└── lib/
    ├── im_sdk.dll（+ .dll.lib 导入库——MSVC 给 C 调用方的链接馈赠）
    └── <triple>/      交叉编译产物（按 target 三元组分目录）
```

两个诚实的点：构建参数钉死 `--features jni`（原因见 §七的 feature
覆盖坑）；`--target` 只构建 rustup 已安装的三元组，没装的**跳过并
打印安装命令**，绝不假装产出。Android/iOS 交叉还需要 NDK 的链接器
配置，属阶段 14 CI 矩阵的活，此处只交付机制不冒充结果。

## 六、冒烟：Demo.java 连真实服务端

```powershell
cargo xtask sdk                                     # 或手动 build（--features jni！）
javac -encoding UTF-8 -d target/java-classes crates/im-sdk/java/im/sdk/*.java
cargo run -p im-sdk --release --example demo_server # 127.0.0.1:18888，token "demo"
java "-Djava.library.path=target/release" -cp target/java-classes im.sdk.Demo
```

JDK 27 真机输出（节选）：`SDK version = 0.1.0`、双端 Connected（雪花
sessionId）、`alice -> bob` 消息（MessageQueued/Ack/Message 三事件
各就位）、中文内容逐字无损（modified UTF-8 链路实测）、`done` 干净
退出（幂等 close + 泵 join + GlobalRef 回收，无挂死无崩溃）。

顺带记录 JDK 27 新行为：`System.loadLibrary` 触发 restricted native
access WARNING（未来版本会默认 block）——真实集成要加
`--enable-native-access=ALL-UNNAMED`（或把绑定装进命名模块）。
**SDK 的用户环境永远比你的开发机激进**，警告即预告。

## 七、设计模式实战（对照 roadmap 4.5）

| 模式 | 在本阶段的形态 |
|---|---|
| **句柄/门面（Handle/Facade）** | `im_sdk_client_t` 不透明指针 + 7 个函数的极简 C API——调用方「拿到的东西」越小，误用面越小 |
| **观察者（回调注册的 C 形态）** | 事件泵 + `EventCallback` typedef + `user_data` 闭包——观察者的跨语言形态就是「函数指针 + 上下文指针」 |
| **错误码模型** | panic 边界拦在 FFI 内、`i32` 稳定码 + 兜底串——Result 惯用法翻译成 ABI 惯用法 |
| **类型状态（Typestate）** | **本阶段未做**（诚实边界，见下） |

类型状态的账单：roadmap 4.5 标了「11 进阶」，docs/16 预告也提了
「编译期保证未连接的句柄不能发消息」。实做时确认：**C ABI 形态下
类型状态无法兑现**——C 调用方拿到的就是 `void*`，强转、乱传、
双 free 都在 C 的能力面内，Rust 编译期的类型区分到不了 C 那边。
它在 Rust 原生 API（rlib 形态，`SdkClient<Connected>` 泛型状态）里
可行，留给阶段 12 Tauri 集成时以 Rust API 形态实现——**模式是否
适用由消费方的类型系统决定，不由实现方的意愿决定**。

## 八、测试策略

| 层 | 用例 | 要点 |
|---|---|---|
| 纯函数单测 | 错误码→描述串（覆盖 + 越界兜底） | 向前兼容的兜底路径必须有测试 |
| FFI 单测 | 静态字符串指针稳定性 / null 入参防御（不解引用直接拒） / destroy(null) 空操作 | 防御式收尾：坏入参不许崩 |
| 端到端（普通 #[test]） | 双客户端经真实服务端互发：poll 往返 / SyncBatch 展开逐条 / Rejected 事件后通道关闭 / 空载荷 data=NULL / 回调泵端到端 / destroy 不挂死 | 服务端挂独立 runtime（TestServer，drop 即停）——**测试形态 = 真实 C 调用方形态** |
| 真机冒烟 | JDK 27 + Demo.java 连 demo_server | 中文 UTF-8 全链路、close 干净退出 |

全部 12 项在 `cargo test -p im-sdk`（**不带** jni feature 也能跑——
JNI 层由真机冒烟覆盖，而不是靠 CI 里没有的 JDK）。

## 九、已知取舍（诚实的账单）

- **每事件一次 attach/detach**：简单正确先行；事件量大时换
  `attach_current_thread_permanently` 或 jni-rs 的 Executor——机制
  留在注释里，数字没压测不吹收益；
- **宽结构的字段浪费**：8 变体共用一套字段，小事件浪费几十字节——
  换「一个 free 管所有变体」，值；
- **SDK 只包 TCP 会话核心**：好友/群组/REST 是 Web 网关的能力面
  （docs/12），SDK 事件模型里没有它们——`im-client` 的对外边界就是
  SDK 的对外边界，不越权承诺；
- **feature 覆盖坑（实测）**：`cargo run --example demo_server`（不带
  jni）会把带 JNI 导出的 dll **静默覆盖**成无导出版本，Java 侧
  `UnsatisfiedLinkError`（库加载成功、符号找不到——比「库不存在」
  迷惑得多）。cargo 的构建缓存按 feature 集**整体区分**，交替构建
  就互相覆盖。解法是 xtask 打包钉死 feature 集 + docs/20 立此存照；
- **无 cbindgen**：7 个函数手写头文件足够；API 膨胀后的迁移路径已
  在 §三写明。

## 十、下一步（阶段 12 预告）

SDK 阵面交付后回到安全与桌面主线：rustls TLS（传输层加密）→
E2EE 双棘轮（X3DH + Signal 协议，`im-crypto`）→ Tauri 桌面端
（消费本阶段的 rlib 形态，顺带兑现类型状态的 Rust 原生版）。
docs/16 预告的「类型状态进阶」在此立账：**ABI 形态做不了，Rust
原生 API 形态做**。

## 十一、面试题与标准回答

**Q1：跨语言 SDK 最重要的设计是什么？**

答：不是 API 多优雅，是**契约可执行**。三条契约里内存契约最致命：
「谁分配谁释放」靠文档约定就是靠运气——所以事件结构只在 SDK 侧
分配、只认 SDK 的 free；入参立即拷贝、指针寿命当场终结；静态字符串
契约里写明不许 free。再把「互斥」这种容易写错的约定做成物化结构
（回调模式把事件接收端 move 进泵，poll 自然失败）——**能用所有权
表达的约定就不要靠文档**，剩下表达不了的才写注释。Java 对照：JNI
层把契约翻译成 Java 语义（GC、异常、null），Java 程序员感觉不到
free 的存在，这本身就是契约设计成功的标志。

**Q2：`*mut c_void` 不是 Send，你的回调上下文怎么进线程的？**

答：踩了三连败才答得上来。第一直觉是 newtype + `unsafe impl Send`，
失败——闭包体里写 `user_data.0`，RFC 2229 精确捕获只捕获**字段路径**
（还是那个裸指针）；第二直觉模式解构，同样失败——解构会被编译器
归一化成字段捕获。最终形态：把泵循环提成独立函数，结构体**按值
传参**，闭包 `move || pump_loop(events, callback, user_data)` 被迫
捕获完整结构体。这个案例的普适教训：**unsafe impl Send 只声明资格，
捕获分析决定实际穿越的是什么**——两者要一起设计，验证手段就是
`cargo check`（编译器不认，资格声明就是废纸）。

**Q3：为什么 JNI 层不用 C 层的错误码，而是抛异常？**

答：入乡随俗是契约翻译的核心。C ABI 的消费者（C/C++/以及将来
Kotlin via JNA）检查返回值是本能；Java 的消费者检查返回值不是本能
——错误码会被忽略，异常不会。所以 `nativeSend` 把非零码翻译成
`RuntimeException`（错误描述来自 `im_sdk_error_string`），唯独
`ERR_TIMEOUT` 在 `poll` 里翻译成 `null`——「暂时没有事件」在 Java
语义里不是错误。同一个错误码在不同语言里应该有不同的**自然形态**，
机械保持一致才是错的。

**Q4：你的 SDK 怎么处理 panic？**

答：三条防线。第一，`extern "C"` 边界 panic 默认 abort——所以
SDK 内部所有可失败路径都走 `Result`/错误码，不依赖「碰巧没 panic」；
第二，防御式入参：null 句柄/null 出参返回 `ERR_INVALID_ARG` 而不是
解引用，`destroy(null)`/`free(null)` 是合法空操作——C 调用方的清理
路径经常双调、空调，崩在 free 里是最难排查的一类;第三，JNI 层
回调里 Java 侧抛的异常必须 `exception_clear`——pending 异常带进
native，下一帧 JNI 调用直接炸，这是 JNI 规范级别的坑。

**Q5：轮询和回调两种事件模式，SDK 为什么都做？**

答：因为两种宿主都需要。GUI 主循环（Tauri/Flutter 的 event loop）
天然适合轮询按帧消化；后台服务/Android Service 更自然用回调。
代价是互斥纪律——SDK 把它物化：回调模式创建时事件接收端被泵线程
`take_events` move 走，此后 `poll` 必然 `ERR_POLL_WITH_CALLBACK`，
**调用方想同时用两种模式在 API 层面就走不通**，不用文档说服。
Java 层同款：构造器传 callback 就没有 poll 入口（传 null 才有）。

---

*阶段 11 完成于：`im-sdk`（错误码模型 + 同步外观 + 7 函数 C ABI +
手写 `im_sdk.h` + JNI 绑定，12 项回归全绿，clippy 零警告）、
`Sdk.java`/`Demo.java`（JDK 27 真机冒烟：中文消息全链路无损、
干净关闭）、`xtask sdk` 打包（dist/sdk + 交叉编译诚实跳过）、
demo_server（免数据库冒烟对端）。踩坑五条收录于 docs/20：
闭包捕获三连败、timeout 语义反转、嵌套 runtime abort、
c_void 双胞胎、feature 覆盖 dll。*
