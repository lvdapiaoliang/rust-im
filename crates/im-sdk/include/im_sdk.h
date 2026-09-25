/*
 * im_sdk.h —— rust-im FFI SDK 的 C 头文件（与 src/ffi.rs 手工同步维护）
 *
 * 产品形态：Rust cdylib（Windows: im_sdk.dll / Linux: libim_sdk.so /
 * macOS: libim_sdk.dylib）。
 *
 * 三条契约（docs/17-ffi.md 全文展开）：
 *   1. 内存契约（谁分配谁释放）：
 *      - 入参（字符串/字节）一律立即拷贝，指针寿命只到本次调用返回；
 *      - im_sdk_client_poll_event 拿到的事件必须 im_sdk_event_free；
 *      - 回调模式下事件生命周期止于回调返回（SDK 代为回收）；
 *      - im_sdk_version / im_sdk_error_string 返回静态字符串，绝不 free。
 *   2. 线程契约：句柄可跨线程传递；im_sdk_client_destroy 不得与其他
 *      调用并发（句柄所有权语义）；回调发生在 SDK 内部事件泵线程上，
 *      不得在回调内阻塞或调用 im_sdk_client_destroy。
 *   3. 错误契约：不抛异常、不 panic，只有下方稳定 i32 返回码。
 *
 * 字符串编码：UTF-8，入参以 '\0' 结尾且必须为合法 UTF-8。
 */

#ifndef IM_SDK_H
#define IM_SDK_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── 错误码（数值稳定：发布后不改，只追加）── */

#define IM_SDK_OK                 0  /* 成功 */
#define IM_SDK_ERR_INVALID_ARG    1  /* 入参非法（空指针/非 UTF-8/非法长度） */
#define IM_SDK_ERR_STOPPED        2  /* 客户端已退出（被拒或已销毁） */
#define IM_SDK_ERR_TIMEOUT        3  /* 等待事件超时（正常轮询路径之一） */
#define IM_SDK_ERR_POLL_WITH_CB   4  /* 回调模式下调用 poll */
#define IM_SDK_ERR_INTERNAL       5  /* 内部故障（兜底） */
#define IM_SDK_ERR_REJECTED        6  /* 握手被服务端拒绝（不再重连；阶段 12 新增，
                                         类型状态 API 的终局错误码） */

/* ── 事件类型码（数值稳定）── */

#define IM_SDK_EV_CONNECTED       0  /* session_id 有效 */
#define IM_SDK_EV_DISCONNECTED    1  /* SDK 自动重连中，无需处理 */
#define IM_SDK_EV_MESSAGE         2  /* 下行消息：from/to/msg_id/client_msg_id + data */
#define IM_SDK_EV_MESSAGE_QUEUED  3  /* 上行已入重发表：client_msg_id/to + data */
#define IM_SDK_EV_ACK             4  /* 上行被服务端确认：msg_id + client_msg_id */
#define IM_SDK_EV_REJECTED        5  /* 握手被拒（不再重连）：data = 原因（UTF-8） */
#define IM_SDK_EV_SEND_FAILED     6  /* 重传耗尽放弃：client_msg_id */

/* ── 不透明句柄 ── */

typedef struct im_sdk_client im_sdk_client_t;

/* ── 事件结构（宽结构，所有变体共用；data 语义随 type 变化）── */

typedef struct im_sdk_event {
    int32_t  type;           /* 事件类型码，见上 */
    uint64_t session_id;     /* Connected */
    uint64_t msg_id;         /* Message / Ack */
    uint64_t client_msg_id;  /* Message / MessageQueued / Ack / SendFailed */
    uint64_t from;           /* Message */
    uint64_t to;             /* Message / MessageQueued */
    uint8_t *data;           /* Message/Queued=内容；Rejected=原因；空内容为 NULL */
    size_t   data_len;       /* data 字节数 */
} im_sdk_event_t;

/* 事件回调：SDK 事件泵线程逐条调用。
 * event 及其 data 只在回调返回前有效——要保留请当场拷贝。 */
typedef void (*im_sdk_event_cb)(im_sdk_event_t *event, void *user_data);

/* ── 导出函数 ── */

/* SDK 版本（静态字符串，如 "0.1.0"）。兼容性检查入口：大版本不符就别用。 */
const char *im_sdk_version(void);

/* 错误码 → 人读说明（静态字符串，不 free）。 */
const char *im_sdk_error_string(int32_t code);

/* 创建客户端：起后台运行时与连接状态机，立刻返回。
 * server_addr: "127.0.0.1:8888" 形态；token: 认证令牌；
 * data_dir:    本地消息库目录，NULL = 临时目录（重启丢历史）；
 * callback:    NULL = 轮询模式（用 im_sdk_client_poll_event）；
 *              非 NULL = 回调模式（事件泵线程自动送达，勿再 poll）。
 * 返回句柄；失败返回 NULL。 */
im_sdk_client_t *im_sdk_client_create(const char *server_addr,
                                      uint64_t user_id,
                                      const char *token,
                                      const char *data_dir,
                                      im_sdk_event_cb callback,
                                      void *user_data);

/* 发消息：IM_SDK_OK = 已入队（断线排队、送达以 IM_SDK_EV_ACK 为准）。
 * data/len 描述消息内容（可为 NULL/0）；立即拷贝，指针寿命到返回为止。 */
int32_t im_sdk_client_send(im_sdk_client_t *client,
                           uint64_t to,
                           const uint8_t *data,
                           size_t len);

/* 轮询模式下取一条事件（超时返回 IM_SDK_ERR_TIMEOUT）。
 * 成功时 *out 为事件指针，用完必须 im_sdk_event_free。 */
int32_t im_sdk_client_poll_event(im_sdk_client_t *client,
                                 im_sdk_event_t **out,
                                 uint32_t timeout_ms);

/* 回收事件（poll 路径的调用方义务；NULL 为合法空操作）。 */
void im_sdk_event_free(im_sdk_event_t *event);

/* 销毁客户端：收回运行时/线程/句柄。调用后句柄作废；
 * 不得与其他调用并发；NULL 为合法空操作。 */
void im_sdk_client_destroy(im_sdk_client_t *client);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* IM_SDK_H */
