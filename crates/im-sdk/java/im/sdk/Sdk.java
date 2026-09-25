package im.sdk;

/**
 * rust-im FFI SDK 的 Java 绑定（JNI 形态）。
 *
 * <p>三层结构（与 docs/17-ffi.md 对应）：Rust 内核 → C ABI（im_sdk.dll）
 * → JNI（本类）。内存契约在 JNI 层被翻译成 Java 语义：
 *
 * <ul>
 *   <li>C 事件结构 + free 义务 → {@link Event}（数据已拷贝进 JVM 堆，
 *       生命周期交给 GC，Java 侧没有任何 free）；</li>
 *   <li>C 回调 user_data 裸指针 → {@link EventCallback}（native 侧用
 *       GlobalRef 持有，线程自动 attach）；</li>
 *   <li>C 错误码 → Java 异常（send/poll 失败时 throw）。</li>
 * </ul>
 *
 * <p>用法：回调模式（构造时传 callback）或轮询模式（传 null，用
 * {@link #poll}）。两种模式互斥——传了 callback 再 poll 会抛异常。
 *
 * <p>线程契约：本实例可跨线程共享，但 {@link #close} 不得与其他调用并发；
 * 回调在 SDK 内部事件泵线程上执行，不要在回调里阻塞或调 close。
 */
public final class Sdk implements AutoCloseable {

    static {
        // 库名 im_sdk ↔ im_sdk.dll / libim_sdk.so / libim_sdk.dylib
        //（cdylib 的 lib name 在 crates/im-sdk/Cargo.toml 定义）
        System.loadLibrary("im_sdk");
    }

    /** 事件类型码（与 im_sdk.h 的 IM_SDK_EV_* 一一对应）。 */
    public static final int EV_CONNECTED = 0;
    public static final int EV_DISCONNECTED = 1;
    public static final int EV_MESSAGE = 2;
    public static final int EV_MESSAGE_QUEUED = 3;
    public static final int EV_ACK = 4;
    public static final int EV_REJECTED = 5;
    public static final int EV_SEND_FAILED = 6;

    /** SDK 版本（Rust CARGO_PKG_VERSION）。启动时做兼容性检查的入口。 */
    public static native String nativeVersion();

    private native long nativeCreate(String addr, long userId, String token,
                                     String dataDir, EventCallback callback);

    private native void nativeClose(long handle);

    private native void nativeSend(long handle, long to, byte[] data);

    private native Event nativePoll(long handle, int timeoutMs);

    /** 事件回调（观察者模式的 JNI 形态）。在 SDK 事件泵线程上被调用。 */
    public interface EventCallback {
        /** 每条事件回调一次；data 已拷贝，回调返回后对象可自由持有。 */
        void onEvent(Event event);
    }

    /** SDK 事件（字段与 C im_sdk_event_t 对应；data 可为空数组）。 */
    public static final class Event {
        public final int type;
        public final long sessionId;
        public final long msgId;
        public final long clientMsgId;
        public final long from;
        public final long to;
        /** 变体数据：EV_MESSAGE/EV_MESSAGE_QUEUED = 内容；EV_REJECTED = 原因。 */
        public final byte[] data;

        /** 由 nativePoll/native 回调构造——Java 代码不要自己 new。 */
        /* signature: (IJJJJJ[B)V —— jni.rs 的 build_java_event 与它对齐 */
        private Event(int type, long sessionId, long msgId, long clientMsgId,
                      long from, long to, byte[] data) {
            this.type = type;
            this.sessionId = sessionId;
            this.msgId = msgId;
            this.clientMsgId = clientMsgId;
            this.from = from;
            this.to = to;
            this.data = data;
        }

        /** data 以 UTF-8 解读（拒绝原因等文本字段的口径）。 */
        public String dataAsText() {
            return new String(data, java.nio.charset.StandardCharsets.UTF_8);
        }

        @Override
        public String toString() {
            return "Event{type=" + type + ", from=" + from + ", to=" + to
                    + ", msgId=" + msgId + ", clientMsgId=" + clientMsgId
                    + ", sessionId=" + sessionId + ", data=" + dataAsText() + "}";
        }
    }

    /** native 句柄（JniHandle 裸指针）。0 = 已关闭。 */
    private long handle;

    /**
     * 创建并连接客户端（断线自动重连）。
     *
     * @param addr     服务端地址，如 "127.0.0.1:8888"
     * @param userId   登录用户 ID
     * @param token    认证令牌
     * @param dataDir  本地消息库目录；null = 临时目录（重启丢历史）
     * @param callback 事件回调；null = 轮询模式（用 poll 取事件）
     */
    public Sdk(String addr, long userId, String token, String dataDir,
               EventCallback callback) {
        this.handle = nativeCreate(addr, userId, token, dataDir, callback);
    }

    /**
     * 发消息：正常返回 = 已入队（断线排队，送达以 EV_ACK 事件为准）。
     *
     * @throws IllegalStateException 客户端已关闭
     * @throws RuntimeException      SDK 返回错误（信息来自 im_sdk_error_string）
     */
    public void send(long to, byte[] content) {
        long h = takeHandle();
        nativeSend(h, to, content);
    }

    /**
     * 轮询模式：取下一条事件。
     *
     * @return 事件；null = 超时（暂时没有事件）
     * @throws IllegalStateException 客户端已关闭或回调模式下误用
     */
    public Event poll(int timeoutMs) {
        long h = takeHandle();
        return nativePoll(h, timeoutMs);
    }

    @Override
    public void close() {
        // 幂等关闭：先 swap 0 再进 native——JNI 侧凭「handle != 0 才回收」
        // 保证不会 double free
        long h;
        synchronized (this) {
            h = handle;
            handle = 0;
        }
        if (h != 0) {
            nativeClose(h);
        }
    }

    /** 校验句柄可用（0 = 已关闭）。 */
    private long takeHandle() {
        long h;
        synchronized (this) {
            h = handle;
        }
        if (h == 0) {
            throw new IllegalStateException("client already closed");
        }
        return h;
    }
}
