//! 优雅关闭信号：一次触发、多方监听。
//!
//! 基于 `tokio::sync::watch`：`bool` 值只会从 `false` 单调地变到 `true` 一次。
//! 对照 Java 的 `Thread.interrupt()`——没有异常冒泡，也没有到处检查
//! 中断位的样板代码：每个 task 在自己的 `select!` 里订阅关停信号，
//! 收到就让出、清理、退出。
//!
//! 为什么不用 tokio-util 的 `CancellationToken`？
//! 目前工作区依赖只有 tokio——用 30 行手写实现换掉一个新依赖，
//! 顺便把 watch channel 的「值变更通知」语义吃透（它是 Rust 里
//! 「广播一个新状态」与「广播一条新消息」的分界线：watch 是前者，
//! broadcast 是后者）。
//!
//! # 语义约定
//!
//! - `trigger()` 幂等，可被任意持有者重复调用；
//! - 所有 sender 被 drop 时，等待方同样视为「已关停」——
//!   信号源消失了，继续等没有意义。

use tokio::sync::watch;

/// 关停信号的发送端（可克隆，任意一方都能触发关停）。
#[derive(Clone, Debug)]
pub struct ShutdownTx {
    tx: watch::Sender<bool>,
}

impl ShutdownTx {
    /// 触发关停。幂等：重复调用没有额外效果。
    pub fn trigger(&self) {
        // 用 `let _ =` 吞掉「所有 receiver 已 drop」的 Err——
        // 没人监听时触发关停同样是合法操作。
        let _ = self.tx.send(true);
    }

    /// 当前是否已触发（非阻塞检查）。
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }
}

/// 关停信号的接收端（可克隆，每个 task 持有一份独立订阅）。
#[derive(Clone, Debug)]
pub struct ShutdownRx {
    rx: watch::Receiver<bool>,
}

impl ShutdownRx {
    /// 当前是否已触发（非阻塞检查）。
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self.rx.borrow()
    }

    /// 异步等待关停信号。
    ///
    /// 已触发则立即返回；所有 sender 消失也立即返回（视为已关停）。
    pub async fn wait(&mut self) {
        loop {
            // borrow_and_update 顺便把当前值标记为「已见」，
            // changed() 只会被「之后」的变更唤醒。
            if *self.rx.borrow_and_update() {
                return;
            }
            if self.rx.changed().await.is_err() {
                return; // sender 全部 drop：视为已关停
            }
        }
    }
}

/// 创建一对关停信号（`watch::channel` 的薄封装，语义见模块文档）。
#[must_use]
pub fn shutdown_channel() -> (ShutdownTx, ShutdownRx) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTx { tx }, ShutdownRx { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 触发一次，所有订阅者（含触发后才克隆的）都能醒来
    #[tokio::test]
    async fn trigger_wakes_all_subscribers() {
        let (tx, mut rx1) = shutdown_channel();
        let mut rx2 = rx1.clone();

        tx.trigger();

        rx1.wait().await;
        rx2.wait().await;
        assert!(rx1.is_triggered());
        assert!(rx2.is_triggered());
    }

    /// 触发是幂等的：重复 trigger 不破坏任何等待方
    #[tokio::test]
    async fn trigger_is_idempotent() {
        let (tx, mut rx) = shutdown_channel();
        tx.trigger();
        tx.trigger();
        rx.wait().await;
        assert!(tx.is_triggered());
    }

    /// sender 全部消失时，等待方视为已关停（不会永远挂起）
    #[tokio::test]
    async fn dropped_sender_counts_as_shutdown() {
        let (tx, mut rx) = shutdown_channel();
        drop(tx);
        rx.wait().await;
    }

    /// 未触发时 wait 挂起——用一个并发触发验证异步唤醒路径
    #[tokio::test]
    async fn wait_parks_until_triggered() {
        let (tx, mut rx) = shutdown_channel();

        let setter = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.trigger();
        });

        rx.wait().await;
        setter.await.unwrap();
    }
}
