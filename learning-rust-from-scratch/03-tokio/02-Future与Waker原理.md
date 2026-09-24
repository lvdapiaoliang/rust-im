# 02 - Future 与 Waker 原理：手写一个 Future

> 面试核心篇。能讲清本篇内容，async 面试题就赢了一半。

## 本章目标

理解 Future trait 的完整机制，亲手实现一个「sleep」Future，
彻底搞懂 Pending → Waker → 再 poll 的循环。

## 一、Future 的定义（只有 30 行，但内涵极深）

```rust
pub trait Future {
    type Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

pub enum Poll<T> {
    Ready(T),
    Pending,
}
```

三个要点：

1. **poll 是被动的**：Future 自己不推进，由执行器（调度器）反复调用
2. **Pending 是契约**：返回 Pending 意味着「我保证：就绪时会调用 `cx.waker().wake()`」
   ——这个承诺是整个异步生态的基石，破坏它 = 任务永远卡死
3. `self: Pin<&mut Self>`：自引用保护（见 [→ docs/02](../../../docs/02-send-sync-pin.md) 第五节的推导）

> 【Java】没有直接对应物。最接近的类比：一个 `Supplier<Poll<T>>`，
> 但 poll 的「可重入 + 不可自我修改状态以外的任何东西 + 必须保存 Waker」契约是 Rust 特有的。

## 二、Waker：异步世界的电话号码

```rust
pub struct Waker { ... }        // 克隆便宜的「唤醒凭证」

impl Waker {
    pub fn wake(self);           // 把关联任务标记为就绪（放回调度队列）
    pub fn wake_by_ref(&self);   // 同上但不消费 self
}
```

关键规则：**poll 返回 Pending 之前，必须已经把 Waker 交给某个事件源**
（IO driver、定时器、channel……），否则没人知道该何时唤醒你。

```
时间线：
t0  任务 poll → 未就绪 → 把 Waker 存入定时器堆 → 返回 Pending
t1  （线程去跑别的任务了……）
t2  定时器到期 → 取出 Waker → waker.wake() → 任务入队
t3  调度器再次 poll 任务 → 就绪 → 返回 Ready(输出)
```

## 三、手写 SleepFuture（完整可运行）

```rust
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// 手写 sleep：不依赖任何异步库，只依赖一个独立线程当「事件源」
pub struct Sleep {
    deadline: Instant,
}

pub fn sleep(dur: Duration) -> Sleep {
    Sleep { deadline: Instant::now() + dur }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            Poll::Ready(())
        } else {
            // 正确版：把 waker 借给定时器线程
            let waker = cx.waker().clone();
            let deadline = self.deadline;
            std::thread::spawn(move || {
                let now = Instant::now();
                if now < deadline {
                    std::thread::sleep(deadline - now);
                }
                waker.wake();            // 就绪了！通知调度器
            });
            Poll::Pending
        }
    }
}

fn main() {
    // 用最简执行器跑它（也可以换 tokio）
    let fut = sleep(Duration::from_secs(1));
    futures::executor::block_on(fut);    // cargo add futures
    println!("1 秒后");
}
```

逐行理解：

1. 第一次 poll：未到期 → 起线程睡觉，**克隆了 Waker 交出去** → Pending
2. 执行器看到 Pending，去跑别的任务（这里没有别的任务，就挂起线程）
3. 睡够的线程调用 `waker.wake()` → 唤醒执行器
4. 执行器再次 poll → 到期 → Ready(())

**反面教材**（务必理解为什么错）：

```rust
// ❌ 错误一：不保存 Waker
fn poll(...) -> Poll<()> {
    Poll::Pending     // 没人会在就绪时唤醒我 → 永久卡死
}

// ❌ 错误二：立即自唤醒
fn poll(...) -> Poll<()> {
    cx.waker().wake_by_ref();   // 马上叫醒自己
    Poll::Pending               // 忙轮询：空转烧 CPU
}
```

> 面试高频：「poll 返回 Pending 后任务为什么不会被饿死/遗忘？」
> 答：Pending 隐含契约是已完成唤醒登记，事件源持有 Waker；
> 这是编译器无法强制的口头协议，所以实现 Future 是 unsafe 之外
> 最容易写出 bug 的地方——也是 tokio 生态替你写好了绝大多数 Future 的原因。

## 四、async/await 是 Future 的语法糖：编译器状态机

```rust
async fn demo() -> u32 {
    let a = step_one().await;      // 挂起点 1
    let b = step_two(a).await;     // 挂起点 2
    a + b
}
```

编译器（概念上）生成：

```rust
enum Demo {
    Start,
    WaitStepOne { fut1: StepOneFut },
    WaitStepTwo { a: u32, fut2: StepTwoFut },
    Done,
}

impl Future for Demo {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<u32> {
        loop {
            match /* 当前状态 */ {
                Start => {
                    let fut1 = step_one();
                    /* 转移到 WaitStepOne 状态 */
                }
                WaitStepOne { fut1 } => match fut1.poll(cx) {
                    Poll::Ready(a) => { /* 存 a，转到 WaitStepTwo */ }
                    Poll::Pending => return Poll::Pending,   // 整个 demo 也 Pending
                },
                WaitStepTwo { a, fut2 } => match fut2.poll(cx) {
                    Poll::Ready(b) => return Poll::Ready(a + b),
                    Poll::Pending => return Poll::Pending,
                },
                Done => panic!("poll after completion"),
            }
        }
    }
}
```

四个推论：

1. **每个 .await 是一个状态**：跨 await 存活的局部变量成为状态机字段
2. **嵌套唤醒**：内层 future Pending → 外层也返回 Pending；内层 Ready → 外层继续。
   Waker 从最外层一路传进来（`cx` 就是干这个的）
3. **Future 只能被 poll 到完成一次**：poll after completion 是逻辑错误
   （async fn 的状态机 Done 后再 poll 直接 panic）
4. 状态机字段里有局部变量的**指针**（如 `&buf` 跨 await 存活）→ 自引用 → 需要 Pin

## 五、不依赖运行时的裸执行器（看懂调度原理的最短路径）

```rust
/// 极简执行器：一个任务 + 忙等 waker 通道
/// （仅教学；真实调度器见下一篇）
fn block_on_simple<F: Future>(mut fut: F) -> F::Output {
    use std::sync::{Arc, Mutex};
    use std::task::{Wake, Waker, RawWaker, RawWakerVTable};
    // ... 简化：实际要手写 RawWaker vtable，很啰嗦。
    // 结论：执行器 = 「任务队列 + Waker 把任务塞回队列 + 循环 poll」
}
```

执行器公式（记住这个，Tokio 那篇会看到同构实现）：

```
执行器 = 待跑任务队列
loop {
    task = queue.pop()        // 没任务就挂起线程（epoll_wait）
    poll(task, waker=任务入队的凭证)
    // Pending → 等事件源 wake；Ready → 任务完成，回收
}
```

## 练习

1. 跑通上面的 SleepFuture，改成 2 秒验证。
2. 故意删掉 `waker.wake()`（线程只睡觉不唤醒），观察程序永久挂起——亲手制造一次死任务。
3. 改成「忙等待版」：poll 里直接 `wake_by_ref()`，用任务管理器观察 CPU 占用飙升。
4. 给 Sleep 增加 `Output = Instant`，返回完成时刻，练习关联类型。

## 自测

1. poll 返回 Pending 的隐含契约是什么？违反后果？
2. 为什么 async 状态机需要 Pin？
3. Waker 是怎么从最外层执行器传到最内层叶子 Future 的？（cx 参数）

下一篇：[03-Tokio运行时与调度器.md](03-Tokio运行时与调度器.md)——从零执行器到工业级调度器。
