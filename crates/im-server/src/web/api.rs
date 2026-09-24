//! REST API（`/api` 前缀）：注册 / 登录 / 好友 / 群组 / 文件。
//!
//! # 分层纪律
//!
//! 处理器只做四件事：**解析请求 → 调仓储 → 组装响应 → 映射错误**。
//! 业务语义（状态机、事务、安全取舍）全在仓储层（`account` /
//! `friends` / `groups` / `files`），处理器里看不到一行 SQL。
//!
//! # 认证
//!
//! `Authorization: Bearer <token>`（登录接口签发的不透明令牌）。
//! [`AuthUser`] 提取器把「验 token → 换用户」收敛在一处——
//! 需要鉴权的处理器第一个参数挂上它即可，重复的样板零拷贝复用。

use std::path::PathBuf;

use axum::extract::multipart::Multipart;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router, extract::FromRequestParts};
use sqlx::PgPool;

use crate::session::Sessions;

use super::account::{AccountError, AccountStore, User};
use super::files::{FileError, FileMeta, FileStore, MAX_FILE_SIZE};
use super::friends::{FriendError, FriendStore};
use super::groups::{GroupError, GroupStore};

/// 登录令牌有效期：7 天（演示值；阶段 10 工程化时做滑动续期）。
pub const TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// 应用状态：axum 的依赖注入容器（`Clone` 廉价——全是 `Arc`/池句柄）。
#[derive(Clone)]
pub struct AppState {
    /// 会话中心：雪花发号 + （WS 网关复用的）路由表。
    pub sessions: Sessions,
    /// 账号域仓储。
    pub accounts: AccountStore,
    /// 好友域仓储。
    pub friends: FriendStore,
    /// 群组域仓储。
    pub groups: GroupStore,
    /// 文件域仓储。
    pub files: FileStore,
}

impl AppState {
    /// 组装应用状态（`files_root` 不存在会自动创建）。
    ///
    /// # Errors
    ///
    /// 文件根目录创建失败返回 [`FileError::Io`]。
    pub async fn new(
        pool: PgPool,
        sessions: Sessions,
        files_root: impl Into<PathBuf>,
    ) -> Result<Self, FileError> {
        Ok(Self {
            sessions,
            accounts: AccountStore::new(pool.clone()),
            friends: FriendStore::new(pool.clone()),
            groups: GroupStore::new(pool.clone()),
            files: FileStore::new(pool, files_root).await?,
        })
    }
}

// ────────────────────────────────────────────────────────────────
// 错误映射
// ────────────────────────────────────────────────────────────────

/// API 错误：携带 HTTP 状态码与人话消息（`From<各仓储错误>` 让处理器
/// 直接 `?` 上抛——错误映射集中在这里，处理器零样板）。
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    /// 手工构造（处理器自身的参数校验用）。
    #[must_use]
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

impl From<AccountError> for ApiError {
    fn from(e: AccountError) -> Self {
        use AccountError as E;
        let status = match &e {
            E::UsernameTaken => StatusCode::CONFLICT,
            E::BadCredentials | E::InvalidToken => StatusCode::UNAUTHORIZED,
            E::IdUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            E::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

impl From<FriendError> for ApiError {
    fn from(e: FriendError) -> Self {
        use FriendError as E;
        let status = match &e {
            E::RequestNotFound | E::UserNotFound => StatusCode::NOT_FOUND,
            E::SelfRequest => StatusCode::BAD_REQUEST,
            E::IdUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            E::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

impl From<GroupError> for ApiError {
    fn from(e: GroupError) -> Self {
        use GroupError as E;
        let status = match &e {
            E::GroupNotFound => StatusCode::NOT_FOUND,
            E::NotOwner => StatusCode::FORBIDDEN,
            E::IdUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            E::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

impl From<FileError> for ApiError {
    fn from(e: FileError) -> Self {
        use FileError as E;
        let status = match &e {
            E::NotFound => StatusCode::NOT_FOUND,
            E::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            E::IdUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            E::Io(_) | E::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.to_string())
    }
}

// ────────────────────────────────────────────────────────────────
// 认证提取器
// ────────────────────────────────────────────────────────────────

/// 已认证用户：`Authorization: Bearer <token>` → 查库换用户。
///
/// 挂在处理器的**非 body 参数**里即可完成鉴权（axum 提取器即中间件）。
pub struct AuthUser {
    /// 令牌对应的用户。
    pub user: User,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers)
            .ok_or_else(|| ApiError::unauthorized("缺少 Bearer 令牌"))?;
        let user = state.accounts.user_by_token(token).await?;
        Ok(Self { user })
    }
}

/// 从 `Authorization` 头取 Bearer 令牌。
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ")
}

// ────────────────────────────────────────────────────────────────
// 请求 / 响应体
// ────────────────────────────────────────────────────────────────

/// 注册请求。
#[derive(Debug, serde::Deserialize)]
pub struct RegisterReq {
    /// 登录名。
    pub username: String,
    /// 明文口令（HTTPS 传输；服务端只存 argon2 哈希）。
    pub password: String,
    /// 昵称。
    pub display_name: String,
}

/// 登录请求。
#[derive(Debug, serde::Deserialize)]
pub struct LoginReq {
    /// 登录名。
    pub username: String,
    /// 明文口令。
    pub password: String,
}

/// 登录响应。
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LoginResp {
    /// 不透明令牌（后续 REST 与 WS 都用它）。
    pub token: String,
    /// 有效期（秒）。
    pub expires_in_secs: u64,
    /// 用户信息。
    pub user: User,
}

/// 好友请求体（按用户 ID 发起）。
#[derive(Debug, serde::Deserialize)]
pub struct FriendReqBody {
    /// 目标用户 ID。
    pub to: u64,
}

/// 建群请求。
#[derive(Debug, serde::Deserialize)]
pub struct CreateGroupReq {
    /// 群名。
    pub name: String,
}

/// 拉人入群请求。
#[derive(Debug, serde::Deserialize)]
pub struct AddMemberReq {
    /// 被拉的用户 ID。
    pub user_id: u64,
}

// ────────────────────────────────────────────────────────────────
// 路由
// ────────────────────────────────────────────────────────────────

/// 组 REST 路由（CORS 全开：开发期 Vite 5173 跨域直连；生产由反向代理同源收口）。
#[must_use]
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/me", get(me))
        .route("/api/friends/requests", post(create_friend_request).get(list_friend_requests))
        .route("/api/friends/requests/{id}/accept", post(accept_friend_request))
        .route("/api/friends/requests/{id}/reject", post(reject_friend_request))
        .route("/api/friends", get(list_friends))
        .route("/api/friends/{user_id}", delete(delete_friend))
        .route("/api/groups", post(create_group).get(my_groups))
        .route("/api/groups/{id}/members", post(add_group_member))
        .route("/api/files", post(upload_file))
        .route("/api/files/{id}", get(download_file))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

// ────────────────────────────────────────────────────────────────
// 账号
// ────────────────────────────────────────────────────────────────

/// POST /api/register：注册成功返回 201 + 用户（不自动登录，前端跳登录页）。
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Result<(StatusCode, Json<User>), ApiError> {
    validate_credentials(&req.username, &req.password)?;
    if req.display_name.trim().is_empty() {
        return Err(ApiError::bad_request("昵称不能为空"));
    }
    let user = state
        .accounts
        .register(&state.sessions, &req.username, &req.password, &req.display_name)
        .await?;
    Ok((StatusCode::CREATED, Json(user)))
}

/// POST /api/login：校验口令并签发令牌。
async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    validate_credentials(&req.username, &req.password)?;
    let user = state.accounts.verify_login(&req.username, &req.password).await?;
    let token = state.accounts.issue_token(user.id, TOKEN_TTL).await?;
    Ok(Json(LoginResp { token, expires_in_secs: TOKEN_TTL.as_secs(), user }))
}

/// GET /api/me：令牌换用户（前端刷新页面后恢复会话用）。
async fn me(auth: AuthUser) -> Json<User> {
    Json(auth.user)
}

/// 注册/登录的入参校验（长度窗口：防垃圾数据，不是安全边界）。
fn validate_credentials(username: &str, password: &str) -> Result<(), ApiError> {
    let username = username.trim();
    if !(3..=32).contains(&username.chars().count()) {
        return Err(ApiError::bad_request("用户名长度须在 3~32 个字符"));
    }
    if password.len() < 6 {
        return Err(ApiError::bad_request("密码至少 6 个字符"));
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────
// 好友
// ────────────────────────────────────────────────────────────────

/// POST /api/friends/requests：发起好友请求。
async fn create_friend_request(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<FriendReqBody>,
) -> Result<(StatusCode, Json<super::friends::FriendRequestView>), ApiError> {
    let view = state.friends.create_request(&state.sessions, auth.user.id, req.to).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// GET /api/friends/requests：我收到 / 我发出的待处理请求。
async fn list_friend_requests(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (incoming, outgoing) = state.friends.pending_requests(auth.user.id).await?;
    Ok(Json(serde_json::json!({ "incoming": incoming, "outgoing": outgoing })))
}

/// POST /api/friends/requests/{id}/accept：接受（仅收方）。
async fn accept_friend_request(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    state.friends.accept_request(id, auth.user.id).await?;
    Ok(StatusCode::OK)
}

/// POST /api/friends/requests/{id}/reject：拒绝（仅收方）。
async fn reject_friend_request(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    state.friends.reject_request(id, auth.user.id).await?;
    Ok(StatusCode::OK)
}

/// GET /api/friends：我的好友列表。
async fn list_friends(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<User>>, ApiError> {
    let friends = state.friends.list_friends(auth.user.id).await?;
    Ok(Json(friends))
}

/// DELETE /api/friends/{user_id}：删除好友（双向对称）。
async fn delete_friend(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(other): Path<u64>,
) -> Result<StatusCode, ApiError> {
    state.friends.remove_friend(auth.user.id, other).await?;
    Ok(StatusCode::OK)
}

// ────────────────────────────────────────────────────────────────
// 群组
// ────────────────────────────────────────────────────────────────

/// POST /api/groups：建群（创建者即群主）。
async fn create_group(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateGroupReq>,
) -> Result<(StatusCode, Json<super::groups::Group>), ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::bad_request("群名不能为空"));
    }
    let group = state.groups.create_group(&state.sessions, &req.name, auth.user.id).await?;
    Ok((StatusCode::CREATED, Json(group)))
}

/// GET /api/groups：我加入的群（含自建）。
async fn my_groups(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<super::groups::MyGroup>>, ApiError> {
    let groups = state.groups.my_groups(auth.user.id).await?;
    Ok(Json(groups))
}

/// POST /api/groups/{id}/members：群主拉人入群。
async fn add_group_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(group_id): Path<u64>,
    Json(req): Json<AddMemberReq>,
) -> Result<StatusCode, ApiError> {
    state.groups.add_member(group_id, auth.user.id, req.user_id).await?;
    Ok(StatusCode::OK)
}

// ────────────────────────────────────────────────────────────────
// 文件
// ────────────────────────────────────────────────────────────────

/// POST /api/files：multipart 上传（字段名 `file`）。
///
/// 全量缓存在内存中再落盘（上限 [`MAX_FILE_SIZE`]）——阶段 6 换
/// 流式落盘时接口不变。
async fn upload_file(
    State(state): State<AppState>,
    auth: AuthUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FileMeta>), ApiError> {
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("multipart 解析失败: {e}")))?
    {
        if field.name() != Some("file") {
            continue; // 忽略其他字段（前端附带的自定义元数据等）
        }
        let filename = field.file_name().map_or_else(|| "unnamed".to_string(), sanitize_filename);

        // 分块累积：超限即刻熔断（不给恶意大文件灌满内存的机会）
        let mut content = Vec::new();
        while let Some(chunk) =
            field.chunk().await.map_err(|e| ApiError::bad_request(format!("读取字段失败: {e}")))?
        {
            if content.len() + chunk.len() > MAX_FILE_SIZE {
                return Err(FileError::TooLarge.into());
            }
            content.extend_from_slice(&chunk);
        }

        let meta = state.files.save(&state.sessions, auth.user.id, &filename, &content).await?;
        return Ok((StatusCode::CREATED, Json(meta)));
    }
    Err(ApiError::bad_request("缺少名为 file 的文件字段"))
}

/// GET /api/files/{id}：鉴权下载（任意登录用户可下载——
/// 好友间转发场景下「下载者未必是上传者」，更细的授权阶段 6 再做）。
async fn download_file(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(file_id): Path<u64>,
) -> Result<Response, ApiError> {
    let (meta, content) = state.files.read(file_id).await?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    // filename 已被 sanitize 过；`filename*` 形态兼容非 ASCII 名
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        meta.filename,
        url_friendly(&meta.filename)
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }

    Ok((headers, content).into_response())
}

/// 文件名消毒：去掉路径分隔符与引号（防 `Content-Disposition` 注入
/// 与路径穿越），其余字符保留。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '/' | '\\' | '"' | '\r' | '\n'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// `filename*` 的 RFC 5987 百分号编码（非 ASCII 文件名用）。
fn url_friendly(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::db::testing::pool_or_skip;
    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    /// 组装「迁移到位」的应用（PG 不可达返回 `None` 跳过）。
    /// 文件根目录用一次性临时目录，测试互不串扰。
    async fn app_or_skip() -> Option<(Router, PgPool, PathBuf)> {
        let pool = pool_or_skip().await?;
        let root = std::env::temp_dir().join(format!("im-files-{}", uuid::Uuid::new_v4()));
        let sessions = Sessions::new(crate::session::SessionConfig::default());
        let state = AppState::new(pool.clone(), sessions, &root).await.ok()?;
        Some((router(state), pool, root))
    }

    /// 注册 + 登录，返回 (token, user)。
    async fn register_and_login(app: &Router, username: &str, password: &str) -> (String, User) {
        let register = Request::post("/api/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "username": username,
                    "password": password,
                    "display_name": username,
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(register).await.unwrap();
        if resp.status() != StatusCode::CREATED {
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            panic!("注册应返回 201，实际响应体: {}", String::from_utf8_lossy(&body));
        }

        let login = Request::post("/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "username": username, "password": password }).to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(login).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "登录应返回 200");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: LoginResp = serde_json::from_slice(&body).unwrap();
        (parsed.token, parsed.user)
    }

    /// 带 Bearer 的 GET/POST/DELETE 快捷构造。
    fn authed(
        method: &str,
        uri: &str,
        token: &str,
        json: Option<serde_json::Value>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = if let Some(json) = json {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        } else {
            Body::empty()
        };
        builder.body(body).unwrap()
    }

    /// 收响应体为 JSON。
    async fn json_body(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// 唯一且在注册校验窗口（3~32 字符）内的用户名。
    fn unique_username() -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("t_{}", &id[..16])
    }

    /// 尽力清理测试数据。
    async fn cleanup(pool: &PgPool, usernames: &[&str], root: &PathBuf) {
        for name in usernames {
            let _ =
                sqlx::query("DELETE FROM users WHERE username = $1").bind(name).execute(pool).await;
        }
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 主线：注册 → 登录 → /api/me 恢复会话；错误口令 401
    #[tokio::test]
    async fn register_login_me_roundtrip() {
        let Some((app, pool, root)) = app_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let username = unique_username();

        let (token, user) = register_and_login(&app, &username, "pass1234").await;
        assert_eq!(user.username, username.to_lowercase());

        let resp = app.clone().oneshot(authed("GET", "/api/me", &token, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let me = json_body(resp).await;
        assert_eq!(me["id"], serde_json::json!(user.id));

        // 错误口令 401；缺令牌 401
        let bad = Request::post("/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "username": username, "password": "wrong-pw" }).to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(bad).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(Request::get("/api/me").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        cleanup(&pool, &[&username], &root).await;
    }

    /// 好友全流程：请求 → 对方看到 → 接受 → 互为好友 → 删除
    #[tokio::test]
    async fn friend_request_flow() {
        let Some((app, pool, root)) = app_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let name_a = unique_username();
        let name_b = unique_username();
        let (token_a, user_a) = register_and_login(&app, &name_a, "pass1234").await;
        let (token_b, user_b) = register_and_login(&app, &name_b, "pass1234").await;

        // A 请求 B
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                "/api/friends/requests",
                &token_a,
                Some(serde_json::json!({ "to": user_b.id })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        let request_id = created["id"].as_u64().unwrap();

        // B 的收件箱里能看到
        let resp = app
            .clone()
            .oneshot(authed("GET", "/api/friends/requests", &token_b, None))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["incoming"].as_array().unwrap().len(), 1);
        assert_eq!(body["incoming"][0]["from_user"], serde_json::json!(user_a.id));

        // A 无权接受自己的请求（收方是 B）
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/api/friends/requests/{request_id}/accept"),
                &token_a,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // B 接受 → 互为好友
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/api/friends/requests/{request_id}/accept"),
                &token_b,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp =
            app.clone().oneshot(authed("GET", "/api/friends", &token_b, None)).await.unwrap();
        let friends = json_body(resp).await;
        assert_eq!(friends.as_array().unwrap().len(), 1);
        assert_eq!(friends[0]["id"], serde_json::json!(user_a.id));

        // A 删除好友
        let resp = app
            .clone()
            .oneshot(authed("DELETE", &format!("/api/friends/{}", user_b.id), &token_a, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp =
            app.clone().oneshot(authed("GET", "/api/friends", &token_a, None)).await.unwrap();
        let friends = json_body(resp).await;
        assert!(friends.as_array().unwrap().is_empty(), "删除后应为空");

        cleanup(&pool, &[&name_a, &name_b], &root).await;
    }

    /// 群流程：建群 → 群主拉人 → 双方都在 my_groups；非群主拉人 403
    #[tokio::test]
    async fn group_member_flow() {
        let Some((app, pool, root)) = app_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let name_a = unique_username();
        let name_b = unique_username();
        let (token_a, _user_a) = register_and_login(&app, &name_a, "pass1234").await;
        let (token_b, user_b) = register_and_login(&app, &name_b, "pass1234").await;

        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                "/api/groups",
                &token_a,
                Some(serde_json::json!({ "name": "压测预备群" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let group = json_body(resp).await;
        let group_id = group["id"].as_u64().unwrap();

        // 群主拉 B
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/api/groups/{group_id}/members"),
                &token_a,
                Some(serde_json::json!({ "user_id": user_b.id })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // B 的群列表里能看到，角色 member
        let resp = app.clone().oneshot(authed("GET", "/api/groups", &token_b, None)).await.unwrap();
        let groups = json_body(resp).await;
        assert_eq!(groups.as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["role"], serde_json::json!("member"));

        // B（非群主）拉人被拒
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/api/groups/{group_id}/members"),
                &token_b,
                Some(serde_json::json!({ "user_id": user_b.id })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        cleanup(&pool, &[&name_a, &name_b], &root).await;
    }

    /// 文件：multipart 上传 → 鉴权下载往返；缺令牌 401
    #[tokio::test]
    async fn file_upload_download_roundtrip() {
        let Some((app, pool, root)) = app_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let username = unique_username();
        let (token, _user) = register_and_login(&app, &username, "pass1234").await;

        // 手工拼 multipart 体（测试不引 multer 这类额外依赖）
        let boundary = "----qoder-test-boundary";
        let payload = b"hello file content".to_vec();
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\n\
             Content-Type: text/plain\r
\r
hello file content\r
--{b}--\r
",
            b = boundary
        );
        let upload = Request::post("/api/files")
            .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(upload).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let meta = json_body(resp).await;
        assert_eq!(meta["filename"], serde_json::json!("hello.txt"));
        assert_eq!(meta["size_bytes"], serde_json::json!(payload.len()));
        let file_id = meta["id"].as_u64().unwrap();

        // 下载往返：字节一致 + 文件名回填
        let resp = app
            .clone()
            .oneshot(authed("GET", &format!("/api/files/{file_id}"), &token, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("hello.txt"))
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.to_vec(), payload);

        // 缺令牌下载 401
        let resp = app
            .clone()
            .oneshot(Request::get(format!("/api/files/{file_id}")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        cleanup(&pool, &[&username], &root).await;
    }
}
