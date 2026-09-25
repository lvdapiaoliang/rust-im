//! JNI 绑定层（feature `jni`）：把 C ABI 包一层「Java 味」的糖衣。
//!
//! 这是 FFI 的**第二跳**：Rust → C ABI → JNI。为什么不让 Java 直接调
//! C ABI（JNA 那样）？因为 JNI 能把内存契约**翻译成 Java 语义**：
//!
//! - C 的 `im_sdk_event_t*` + `im_sdk_event_free` → Java 的 `Sdk.Event`
//!   对象（字段已拷贝进 JVM 堆，**GC 管生命周期，Java 侧没有任何 free**）；
//! - C 的回调 `user_data` 裸指针 → JNI 的 `GlobalRef`（JVM 全局引用，
//!   跨线程合法持有 Java 对象的标准形态）；
//! - C 的错误码 → Java 异常（`nativeSend`/`nativePoll` 里 throw）。
//!
//! # JNI 三件套（跨语言 SDK 的真实工程点）
//!
//! 1. **线程 attach**：SDK 事件泵是普通线程，JVM 不认识它——回调前必须
//!    `attach_current_thread()`（AttachGuard，drop 时自动 detach）。
//!    每条事件 attach/detach 一次有成本，事件量大的 SDK 应换成
//!    `attach_current_thread_permanently`（docs/17 已知取舍）。
//! 2. **GlobalRef**：局部引用出不了它的原生调用帧——把 Java 回调对象
//!    存到泵线程用，必须 `new_global_ref`，并且**最终必须显式 delete**
//!    （Java 侧 GC 不会替你管理 native 持有的全局引用）。
//! 3. **字符串编码**：`env.get_string` 处理 UTF-16 → UTF-8 转换。
//!    千万别用裸 `GetStringUTFChars`——它给的是 **modified UTF-8**
//!    （NUL 用双字节编码、增补字符非标准），和真 UTF-8 不兼容，
//!    是 JNI 最著名的陷阱（docs/20 §5.7）。
//!
//! Java 侧源码：`java/im/sdk/Sdk.java`（绑定类）与 `java/im/sdk/Demo.java`
//! （冒烟演示），冒烟步骤见 docs/17。

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;

use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jbyteArray, jint, jlong, jobject, jstring};
use jni::{JNIEnv, JavaVM};

use crate::core::{EventCallback, ImSdkEvent, SdkClient};
use crate::ffi::{
    im_sdk_client_create, im_sdk_client_destroy, im_sdk_client_poll_event, im_sdk_client_send,
    im_sdk_error_string, im_sdk_event_free, str_from_c,
};
use crate::{SDK_VERSION, error};

/// JNI 句柄：Java 侧持有的 `long`。包住两个裸指针——
/// `client`（C ABI 客户端）与 `callback_ctx`（回调模式的事件桥，可为 null）。
///
/// 拆开存的必要性：destroy 必须**先** join 事件泵（im_sdk_client_destroy
/// 内部完成），**再**回收 callback_ctx——泵线程还活着时回收它就是
/// use-after-free。句柄把两者绑在一起，Java 侧就无法弄错顺序。
struct JniHandle {
    /// C ABI 客户端句柄（poll 与 send 都走它）。
    client: *mut SdkClient,
    /// `Box<JniEventBridge>` 的裸指针；null = 轮询模式（无回调）。
    callback_ctx: *mut c_void,
}

/// 事件桥：塞给 C 事件泵的 `user_data`——JavaVM + 回调对象的 GlobalRef。
///
/// JavaVM 可以跨线程克隆传递（它就是为此设计的）；GlobalRef 是 JVM 里
/// 唯一能被 native 长期持有的引用形态（局部引用出了原生帧就失效）。
struct JniEventBridge {
    vm: JavaVM,
    callback: GlobalRef,
}

// SAFETY: 两个成员都声明了跨线程共享的安全性——JavaVM 内部线程安全；
// GlobalRef 按 JNI 规范可跨线程使用（引用本身的访问由 JVM 保证）。
unsafe impl Send for JniEventBridge {}

/// C 形态的事件回调（喂给 `im_sdk_client_create` 的泵线程）：
/// attach JVM → 把 C 事件**拷贝**成 Java Event → 回调 → 异常清走。
///
/// 注意函数形态必须精确匹配 [`EventCallback`]：`extern "C" fn`，
/// 参数是裸指针——这是唯一让 Rust 编译器放行的「C 函数指针」形状。
extern "C" fn jni_event_shim(event: *mut ImSdkEvent, user_data: *mut c_void) {
    // SAFETY: user_data 来自 nativeCreate 的 Box::into_raw(JniEventBridge)，
    // 泵线程存活期间有效（nativeDestroy 在 join 泵之后才回收它）
    let bridge = unsafe { &*(user_data as *const JniEventBridge) };

    // 1. 事件泵线程不在 JVM 里：attach（AttachGuard，drop 时自动 detach）。
    //    guard 通过 DerefMut 暴露 JNIEnv。
    let Ok(mut env_guard) = bridge.vm.attach_current_thread() else {
        eprintln!("im-sdk jni: attach_current_thread 失败，事件被丢弃");
        return;
    };
    let env = &mut *env_guard;

    // 2. C 事件 → Java Event（拷贝进 JVM 堆——free 义务到此与 Java 无关）
    let Some(event_obj) = build_java_event(env, event) else {
        eprintln!("im-sdk jni: 构造 Event 失败，事件被丢弃");
        return;
    };

    // 3. 调 Java 回调（call_method 内部把 pending 异常留给 JVM 体系）
    let _ = env.call_method(
        &bridge.callback,
        "onEvent",
        "(Lim/sdk/Sdk$Event;)V",
        &[JValue::Object(&event_obj)],
    );

    // 4. 回调里抛的异常不能带进 native（下一帧 JNI 调用会炸）：
    //    清掉并打到 stderr——native 层的 catch 兜底。
    if matches!(env.exception_check(), Ok(true)) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    // 事件本身的回收由 C 泵在 shim 返回后统一执行（作用域契约）
}

/// 把 C 事件结构拷贝成 `im.sdk.Sdk$Event`（字段 + byte[] data）。
fn build_java_event<'local>(
    env: &mut JNIEnv<'local>,
    event: *mut ImSdkEvent,
) -> Option<JObject<'local>> {
    // SAFETY: 泵在 alloc 后、free 前调用本函数（作用域契约）
    let e = unsafe { &*event };
    let data = if e.data.is_null() {
        Vec::new()
    } else {
        // SAFETY: (data, data_len) 是 alloc_event 的配对区间
        unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec()
    };

    let class = env.find_class("im/sdk/Sdk$Event").ok()?;
    let byte_array = env.byte_array_from_slice(&data).ok()?;
    let data_obj = JObject::from(byte_array);
    let args = [
        JValue::Int(e.type_),
        JValue::Long(i64::try_from(e.session_id).unwrap_or(0)),
        JValue::Long(i64::try_from(e.msg_id).unwrap_or(0)),
        JValue::Long(i64::try_from(e.client_msg_id).unwrap_or(0)),
        JValue::Long(i64::try_from(e.from).unwrap_or(0)),
        JValue::Long(i64::try_from(e.to).unwrap_or(0)),
        JValue::Object(&data_obj),
    ];
    // 构造器签名：(int type, long sessionId, long msgId, long clientMsgId,
    //             long from, long to, byte[] data)
    env.new_object(class, "(IJJJJJ[B)V", &args).ok()
}

/// `Sdk.nativeVersion() -> String`：SDK 版本（运行时兼容性检查入口）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_im_sdk_Sdk_nativeVersion(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    match env.new_string(SDK_VERSION) {
        Ok(s) => s.into_raw(),
        // new_string 失败 = OOM 级别的 JVM 故障：返回 null，让 Java 侧 NPE 提前暴露
        Err(_) => ptr::null_mut(),
    }
}

/// `Sdk.nativeCreate(addr, userId, token, dataDir, callback) -> long`。
///
/// 返回 `JniHandle` 的裸指针（Java 侧当 long 存）；失败抛异常。
#[unsafe(no_mangle)]
pub extern "system" fn Java_im_sdk_Sdk_nativeCreate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    addr: JString<'_>,
    user_id: jlong,
    token: JString<'_>,
    data_dir: JObject<'_>,
    callback: JObject<'_>,
) -> jlong {
    // JString 取内容：get_string 返回 JavaStr（内部经 cesu8 把
    // modified UTF-8 解回标准 UTF-8——避开 GetStringUTFChars 裸指针的陷阱）
    let (Ok(addr), Ok(token)) = (env.get_string(&addr), env.get_string(&token)) else {
        let _ = env.throw_new("java/lang/IllegalArgumentException", "invalid string argument");
        return 0;
    };
    let (addr, token) = (String::from(addr), String::from(token));
    if addr.is_empty() || token.is_empty() {
        let _ = env.throw_new("java/lang/IllegalArgumentException", "addr/token must not be empty");
        return 0;
    }

    // data_dir：null JObject → 走 SDK 的临时目录默认值
    let data_dir: Option<String> = if data_dir.is_null() {
        None
    } else {
        // 先绑定再借用：JString::from 产生的临时值必须活到 get_string 结束
        let dir_string = JString::from(data_dir);
        let Ok(dir) = env.get_string(&dir_string) else {
            let _ = env.throw_new("java/lang/IllegalArgumentException", "dataDir is not a String");
            return 0;
        };
        Some(String::from(dir))
    };

    // 回调：非 null → GlobalRef + JavaVM 打包成事件桥，塞给 C 泵当 user_data
    let (cb, ctx) = if callback.is_null() {
        (None, ptr::null_mut())
    } else {
        let Ok(vm) = env.get_java_vm() else {
            let _ = env.throw_new("java/lang/RuntimeException", "get_java_vm failed");
            return 0;
        };
        // 全局引用：局部引用出了这个原生帧就失效，泵线程拿着它必炸
        let Ok(global) = env.new_global_ref(&callback) else {
            let _ = env.throw_new("java/lang/RuntimeException", "new_global_ref failed");
            return 0;
        };
        let bridge = Box::new(JniEventBridge { vm, callback: global });
        (Some(jni_event_shim as EventCallback), Box::into_raw(bridge).cast::<c_void>())
    };

    let addr_c = match std::ffi::CString::new(addr) {
        Ok(c) => c,
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", "addr contains NUL");
            return 0;
        }
    };
    let token_c = match std::ffi::CString::new(token) {
        Ok(c) => c,
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalArgumentException", "token contains NUL");
            return 0;
        }
    };
    let dir_c = data_dir.and_then(|d| std::ffi::CString::new(d).ok());

    // SAFETY: 三个字符串都是刚构造的 NUL 结尾 CString；cb 形态精确匹配。
    // （create/poll/destroy 等都是「安全表面 + 调用方契约」的 extern "C" fn，
    //   安全义务在文档里，不在 unsafe 块里。）
    let client = im_sdk_client_create(
        addr_c.as_ptr(),
        u64::try_from(user_id).unwrap_or(0),
        token_c.as_ptr(),
        dir_c.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
        cb,
        ctx,
    );
    if client.is_null() {
        // 装配失败的分支：事件桥不能漏（SDK 侧 create 未接管就退出）
        if !ctx.is_null() {
            // SAFETY: ctx 是本函数 Box::into_raw(JniEventBridge) 的产物，
            // SDK 未接管（client 为 null），无并发访问
            drop(unsafe { Box::from_raw(ctx.cast::<JniEventBridge>()) });
        }
        let _ = env.throw_new("java/lang/RuntimeException", "im_sdk_client_create failed");
        return 0;
    }
    Box::into_raw(Box::new(JniHandle { client, callback_ctx: ctx })) as jlong
}

/// `Sdk.nativeSend(self, to, data) -> void`：非 0 错误码翻译成 Java 异常。
#[unsafe(no_mangle)]
pub extern "system" fn Java_im_sdk_Sdk_nativeSend(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    to: jlong,
    data: jbyteArray,
) {
    let Some(handle) = (unsafe { (handle as *mut JniHandle).as_ref() }) else {
        let _ = env.throw_new("java/lang/IllegalStateException", "client already closed");
        return;
    };
    // Java byte[] → Vec（拷贝）：JNI 数组访问必须走 GetByteArrayElements 家族，
    // jni-rs 的 convert_byte_array 封装了拷贝/释放的完整闭环
    let content = if data.is_null() {
        Vec::new()
    } else {
        // SAFETY: data 是 JVM 传入的合法 jbyteArray（方法签名由 JVM 保证类型）
        let arr = unsafe { JByteArray::from_raw(data) };
        match env.convert_byte_array(&arr) {
            Ok(v) => v,
            Err(_) => {
                let _ = env.throw_new("java/lang/RuntimeException", "convert_byte_array failed");
                return;
            }
        }
    };
    // 句柄契约：nativeCreate 产出、close 之前、无并发 close
    let rc = im_sdk_client_send(
        handle.client,
        u64::try_from(to).unwrap_or(0),
        content.as_ptr().cast::<u8>(),
        content.len(),
    );
    if rc != error::OK {
        // SAFETY: 错误串是 SDK 静态字符串（NUL 结尾、进程常驻）
        let msg =
            unsafe { str_from_c(im_sdk_error_string(rc)) }.unwrap_or_else(|| "unknown".to_string());
        let _ = env.throw_new("java/lang/RuntimeException", format!("send failed: {msg}"));
    }
}

/// `Sdk.nativePoll(self, timeoutMs) -> Event`：超时返回 null，其余错误抛异常。
#[unsafe(no_mangle)]
pub extern "system" fn Java_im_sdk_Sdk_nativePoll(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    timeout_ms: jint,
) -> jobject {
    let Some(handle) = (unsafe { (handle as *mut JniHandle).as_mut() }) else {
        let _ = env.throw_new("java/lang/IllegalStateException", "client already closed");
        return ptr::null_mut();
    };

    let mut raw: *mut ImSdkEvent = ptr::null_mut();
    // 句柄契约同 send；out 指向栈上局部变量
    let rc =
        im_sdk_client_poll_event(handle.client, &mut raw, u32::try_from(timeout_ms).unwrap_or(0));
    match rc {
        error::OK => {
            let event = build_java_event(&mut env, raw);
            // SAFETY: poll 成功即移交了所有权——拷贝完必须回收（谁分配谁释放）
            unsafe { im_sdk_event_free(raw) };
            match event {
                Some(obj) => obj.into_raw(),
                None => {
                    let _ = env.throw_new("java/lang/RuntimeException", "build Event failed");
                    ptr::null_mut()
                }
            }
        }
        // 超时不是异常：Java 侧拿到 null 表示「暂时没有事件」
        error::ERR_TIMEOUT => ptr::null_mut(),
        _ => {
            // SAFETY: 错误串是 SDK 静态字符串
            let msg = unsafe { str_from_c(im_sdk_error_string(rc)) }
                .unwrap_or_else(|| "unknown".to_string());
            let _ = env.throw_new("java/lang/RuntimeException", format!("poll failed: {msg}"));
            ptr::null_mut()
        }
    }
}

/// `Sdk.nativeClose(self)`：销毁客户端 + 回收事件桥。
///
/// 顺序即安全：`im_sdk_client_destroy` 内部**先 join 事件泵**，泵确认
/// 停了之后才回收 `callback_ctx`（GlobalRef 的 delete 也在这一步）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_im_sdk_Sdk_nativeClose(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: Java 侧 close 幂等保证（Sdk.close 置 0 后才允许再次调用），
    // 此处 take 回所有权后指针不再复用
    let handle = unsafe { Box::from_raw(handle as *mut JniHandle) };
    // 1. 销毁客户端：内部 trigger shutdown → join 事件泵 → drop 运行时
    //    （句柄契约：nativeCreate 产出且未销毁过）
    im_sdk_client_destroy(handle.client);
    // 2. 泵已 join：回收事件桥（GlobalRef 随 Box drop 一起释放）
    if !handle.callback_ctx.is_null() {
        // SAFETY: ctx 是 nativeCreate 的 Box::into_raw(JniEventBridge) 产物，
        // 事件泵已 join，无并发访问
        drop(unsafe { Box::from_raw(handle.callback_ctx.cast::<JniEventBridge>()) });
    }
}
