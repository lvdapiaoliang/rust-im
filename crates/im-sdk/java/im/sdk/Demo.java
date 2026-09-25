package im.sdk;

import java.nio.charset.StandardCharsets;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * JNI 冒烟演示：两个客户端经真实 TCP 服务端（demo_server）互发消息。
 *
 * <p>跑通本类即验证了整条 FFI 链路：Java → JNI（{@code jni.rs}）→ C ABI
 * （{@code ffi.rs}）→ 同步内核（{@code core.rs}）→ 异步客户端（im-client）。
 *
 * <p>前置条件：
 * <ol>
 *   <li>构建动态库：{@code cargo build -p im-sdk --release --features jni}
 *       （产出 {@code target/release/im_sdk.dll}）；</li>
 *   <li>编译 Java：{@code javac -d target/java-classes
 *       crates/im-sdk/java/im/sdk/*.java}；</li>
 *   <li>启动服务端：{@code cargo run -p im-sdk --release --example demo_server}；</li>
 *   <li>运行本类：{@code java -Djava.library.path=target/release
 *       -cp target/java-classes im.sdk.Demo}（默认连 127.0.0.1:18888，
 *       可用第一个参数覆盖地址）。</li>
 * </ol>
 *
 * <p>演示要点（每一条都踩过坑才写进来）：
 * <ul>
 *   <li>回调在 SDK 事件泵线程上执行——这里只做 countDown 与打印，绝不
 *       阻塞、绝不 close（close 会 join 泵线程，在泵里调 close = 自锁）；</li>
 *   <li>close 全部由主线程做（try-with-resources）：幂等关闭 + 泵先 join
 *       后回收 GlobalRef 的顺序契约在 native 侧兑现；</li>
 *   <li>中文消息顺带验证 modified UTF-8 陷阱的修复：Java String → JNI
 *       get_string（cesu8 解码）→ Rust UTF-8 → byte[] → dataAsText()。</li>
 * </ul>
 */
public final class Demo {

    /** 每步等待上限：本地回环上正常交互应远快于 5s。 */
    private static final int WAIT_SECS = 5;

    public static void main(String[] args) throws Exception {
        String addr = args.length > 0 ? args[0] : "127.0.0.1:18888";
        System.out.println("rust-im FFI SDK (JNI) 冒烟演示");
        System.out.println("SDK version = " + Sdk.nativeVersion());
        System.out.println("服务端      = " + addr);

        long aliceId = 1001;
        long bobId = 1002;

        // 事件流的检查点：每个 latch 对应一个「必须发生」的协议节点
        CountDownLatch aliceConnected = new CountDownLatch(1);
        CountDownLatch bobConnected = new CountDownLatch(1);
        CountDownLatch bobReceived = new CountDownLatch(1);
        CountDownLatch aliceReceived = new CountDownLatch(1);

        // try-with-resources：main 正常/异常退出都会 close（幂等，重复 close 也安全）
        try (Sdk alice = new Sdk(addr, aliceId, "demo", null, ev -> {
                 System.out.println("[alice] " + ev);
                 if (ev.type == Sdk.EV_CONNECTED) aliceConnected.countDown();
                 if (ev.type == Sdk.EV_MESSAGE) aliceReceived.countDown();
             });
             Sdk bob = new Sdk(addr, bobId, "demo", null, ev -> {
                 System.out.println("[bob]   " + ev);
                 if (ev.type == Sdk.EV_CONNECTED) bobConnected.countDown();
                 if (ev.type == Sdk.EV_MESSAGE) bobReceived.countDown();
             })) {

            // 1. 双端握手（Connected 事件 = 服务端 HandshakeAck 已确认）
            if (!aliceConnected.await(WAIT_SECS, TimeUnit.SECONDS)
                    || !bobConnected.await(WAIT_SECS, TimeUnit.SECONDS)) {
                throw new IllegalStateException(
                        WAIT_SECS + "s 内未握手成功——检查 demo_server 是否已启动");
            }

            // 2. alice -> bob（send 只保证入队；送达以对端的 Message 事件为准）
            alice.send(bobId, "你好 bob，这条消息走完了 Java→JNI→C ABI→Rust 全链路"
                    .getBytes(StandardCharsets.UTF_8));
            System.out.println("alice -> bob 已入队，等待对端收到……");
            if (!bobReceived.await(WAIT_SECS, TimeUnit.SECONDS)) {
                throw new IllegalStateException(WAIT_SECS + "s 内 bob 未收到消息");
            }

            // 3. bob -> alice（对称回一条，验证双向）
            bob.send(aliceId, "收到！这条回复证明回调线程把事件送回了 JVM"
                    .getBytes(StandardCharsets.UTF_8));
            if (!aliceReceived.await(WAIT_SECS, TimeUnit.SECONDS)) {
                throw new IllegalStateException(WAIT_SECS + "s 内 alice 未收到回复");
            }

            System.out.println("冒烟通过：回调事件流 + 消息互发 + 关闭即将验证");
        } // close 在主线程顺序执行：join 事件泵 → 回收 GlobalRef → drop runtime
        System.out.println("done（两个客户端均已干净关闭，进程正常退出）");
    }

    private Demo() {
    }
}
