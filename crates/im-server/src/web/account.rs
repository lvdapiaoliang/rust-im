//! 账号域仓储：注册（argon2）/ 登录校验 / 不透明令牌。
//!
//! # 安全取舍
//!
//! - **argon2**（密码哈希竞赛冠军）：默认参数（内存 19MB / 迭代 2 次）
//!   在「注册登录一次几百毫秒」与「GPU 暴力破解成本」间取平衡；
//! - **不透明 token + 查库校验**：牺牲一点延迟换取**可撤销性**
//!   （删行即踢下线）——JWT 的无状态优势对单机 IM 不值一提；
//! - **统一错误话术**：用户不存在与密码错误都报 [`AccountError::BadCredentials`]，
//!   且不存在时也跑一遍哈希校验——防「用户名枚举」（时序侧信道）。

use std::sync::OnceLock;
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::rngs::OsRng;
use sqlx::PgPool;
use uuid::Uuid;

use super::serde_id;
use super::{id_i64, id_u64};
use crate::session::Sessions;

/// 账号域错误。
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// 用户名已被占用（注册撞唯一键）。
    #[error("用户名已被占用")]
    UsernameTaken,
    /// 用户不存在或密码错误（刻意不区分，防账号探测）。
    #[error("用户不存在或密码错误")]
    BadCredentials,
    /// 令牌无效或已过期。
    #[error("令牌无效或已过期")]
    InvalidToken,
    /// 雪花发号不可用（时钟回拨）——注册请稍后重试。
    #[error("ID 发号器暂不可用")]
    IdUnavailable,
    /// 数据库错误。
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),
}

/// 用户实体（不含敏感字段——密码哈希留在仓储内部）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct User {
    /// 用户 ID（雪花，即协议层 `user_id`；JSON 形态为字符串）。
    #[serde(with = "serde_id")]
    pub id: u64,
    /// 登录名（唯一，注册时已小写化）。
    pub username: String,
    /// 昵称（展示用）。
    pub display_name: String,
}

// 手写 FromRow：PG 的 BIGINT 映射 i64，而领域层统一 u64（与协议层
// `user_id` 同型）——转换集中在这里，调用方不见 i64。
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for User {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: id_u64(row.try_get::<i64, _>("id")?),
            username: row.try_get("username")?,
            display_name: row.try_get("display_name")?,
        })
    }
}

/// 账号仓储：用户与令牌的全部 SQL 都收敛在这里。
#[derive(Debug, Clone)]
pub struct AccountStore {
    pool: PgPool,
}

/// 「用户不存在」时用来对齐耗时的哑哈希（防时序侧信道探测用户名）。
///
/// 首次使用时对固定口令做一次真哈希（懒初始化）：参数与真实校验
/// 一致，耗时相同——攻击者无法用「响应快慢」区分用户是否存在。
fn dummy_password_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("timing-equalizer-dummy"))
}

impl AccountStore {
    /// 用既有连接池构建。
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池（诊断与测试）。
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 注册新用户：小写化用户名 → 雪花发号 → argon2 哈希 → 落库。
    ///
    /// # Errors
    ///
    /// 用户名占用返回 [`AccountError::UsernameTaken`]；发号器不可用返回
    /// [`AccountError::IdUnavailable`]；见 [`AccountError`] 其余变体。
    ///
    /// # Panics
    ///
    /// 雪花 ID 超出 `i64` 范围时 panic（发号器保证不会发生）。
    pub async fn register(
        &self,
        ids: &Sessions,
        username: &str,
        password: &str,
        display_name: &str,
    ) -> Result<User, AccountError> {
        let username = username.trim().to_lowercase();
        let Some(id) = ids.next_id().await else {
            return Err(AccountError::IdUnavailable);
        };
        let password_hash = hash_password(password);

        // 撞唯一键（23505）翻译成人话，其余原样上抛
        let result = sqlx::query_as::<_, User>(
            "INSERT INTO users (id, username, password_hash, display_name)
             VALUES ($1, $2, $3, $4)
             RETURNING id, username, display_name",
        )
        .bind(id_i64(id))
        .bind(&username)
        .bind(&password_hash)
        .bind(display_name.trim())
        .fetch_one(&self.pool)
        .await;

        match result {
            Ok(user) => Ok(user),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(AccountError::UsernameTaken)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// 按用户名查用户（不含密码哈希）。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`AccountError::Db`]）。
    pub async fn find_by_username(&self, username: &str) -> Result<Option<User>, AccountError> {
        let user = sqlx::query_as::<_, User>(
            "SELECT id, username, display_name FROM users WHERE username = $1",
        )
        .bind(username.trim().to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        Ok(user)
    }

    /// 登录校验：查用户 + argon2 验签；统一错误话术（见模块文档）。
    ///
    /// # Errors
    ///
    /// 失败一律 [`AccountError::BadCredentials`]；数据库错误见 [`AccountError::Db`]。
    ///
    /// # Panics
    ///
    /// 库里的用户 ID 超出 `u64` 范围时 panic（发号器保证不会发生）。
    pub async fn verify_login(&self, username: &str, password: &str) -> Result<User, AccountError> {
        let row = sqlx::query_as::<_, (i64, String, String, String)>(
            "SELECT id, username, display_name, password_hash FROM users WHERE username = $1",
        )
        .bind(username.trim().to_lowercase())
        .fetch_optional(&self.pool)
        .await?;

        if let Some((id, username, display_name, password_hash)) = row {
            if verify_password(password, &password_hash) {
                Ok(User { id: id_u64(id), username, display_name })
            } else {
                Err(AccountError::BadCredentials)
            }
        } else {
            // 用户不存在：也跑一遍验签（耗时对齐，防时序探测）
            let _ = verify_password(password, dummy_password_hash());
            Err(AccountError::BadCredentials)
        }
    }

    /// 签发登录令牌（UUID v4），`ttl` 后过期。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`AccountError::Db`]）。
    ///
    /// # Panics
    ///
    /// `ttl` 超出 `i64` 秒范围时 panic（调用方传常量，属开发期错误）。
    pub async fn issue_token(&self, user_id: u64, ttl: Duration) -> Result<String, AccountError> {
        let token = Uuid::new_v4().to_string();
        let ttl_secs = i64::try_from(ttl.as_secs()).expect("TTL 不应超过 i64 秒");
        sqlx::query(
            "INSERT INTO tokens (token, user_id, expires_at)
             VALUES ($1, $2, now() + make_interval(secs => $3))",
        )
        .bind(&token)
        .bind(id_i64(user_id))
        .bind(ttl_secs)
        .execute(&self.pool)
        .await?;
        Ok(token)
    }

    /// 按令牌换用户（过期/已撤销/不存在都算无效）。
    ///
    /// # Errors
    ///
    /// 无效返回 [`AccountError::InvalidToken`]；数据库错误见 [`AccountError::Db`]。
    pub async fn user_by_token(&self, token: &str) -> Result<User, AccountError> {
        let user = sqlx::query_as::<_, User>(
            "SELECT u.id, u.username, u.display_name
             FROM tokens t JOIN users u ON u.id = t.user_id
             WHERE t.token = $1 AND t.expires_at > now()",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        user.ok_or(AccountError::InvalidToken)
    }

    /// 撤销单个令牌。返回是否真的删了（`false` = 本就无效，幂等）。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`AccountError::Db`]）。
    pub async fn revoke_token(&self, token: &str) -> Result<bool, AccountError> {
        let deleted = sqlx::query("DELETE FROM tokens WHERE token = $1")
            .bind(token)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(deleted > 0)
    }

    /// 撤销某用户全部令牌（改密码/踢下线）。返回删除数。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`AccountError::Db`]）。
    ///
    /// # Panics
    ///
    /// 用户 ID 超出 `i64` 范围时 panic（发号器保证不会发生）。
    pub async fn revoke_all_tokens(&self, user_id: u64) -> Result<u64, AccountError> {
        let deleted = sqlx::query("DELETE FROM tokens WHERE user_id = $1")
            .bind(id_i64(user_id))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(deleted)
    }
}

/// argon2 哈希（随机盐，PHC 字符串自包含盐与参数）。
///
/// 固定默认参数 + UTF-8 口令不存在失败路径（参数非法才会 Err），
/// 直接 `expect` 把实现 bug 变成进程内显式 panic。
fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("默认参数下哈希不会失败")
        .to_string()
}

/// argon2 验签（PHC 字符串自解释盐与参数）。
fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false; // 库里存了非法哈希：一律拒绝（fail-closed）
    };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::db::testing::{pool_or_skip, test_sessions};

    /// 雪花发号器（测试专用实例，每次分配唯一 `machine_id`——并行测试不撞库）。
    fn ids() -> Sessions {
        test_sessions()
    }

    /// 每个测试一个全局唯一的用户名（共享开发库下互不干扰）。
    fn unique_username() -> String {
        format!("t_{}", Uuid::new_v4().simple())
    }

    /// 尽力清理测试用户（级联删令牌；失败不影响断言）。
    async fn cleanup(pool: &PgPool, username: &str) {
        let _ =
            sqlx::query("DELETE FROM users WHERE username = $1").bind(username).execute(pool).await;
    }

    /// 注册 → 登录 → 字段往返：用户名小写化、ID 非零、昵称原样
    #[tokio::test]
    async fn register_then_login_roundtrip() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let store = AccountStore::new(pool.clone());
        let username = unique_username().to_uppercase(); // 故意大写：验证小写化

        let user =
            store.register(&ids(), &username, "s3cret-pw", "测试用户").await.expect("注册应成功");
        assert_ne!(user.id, 0, "用户 ID 由雪花分配");
        assert_eq!(user.username, username.to_lowercase(), "用户名应小写化");
        assert_eq!(user.display_name, "测试用户");

        let logged = store.verify_login(&username, "s3cret-pw").await.expect("正确密码应登录成功");
        assert_eq!(logged.id, user.id, "同一用户");

        cleanup(&pool, &user.username).await;
    }

    /// 重复用户名被拒；错误是 `UsernameTaken` 而非裸数据库错误
    #[tokio::test]
    async fn duplicate_username_is_rejected() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let store = AccountStore::new(pool.clone());
        let username = unique_username();

        store.register(&ids(), &username, "pw-1", "甲").await.expect("首次注册应成功");
        let err =
            store.register(&ids(), &username, "pw-2", "乙").await.expect_err("同名二次注册应失败");
        assert!(matches!(err, AccountError::UsernameTaken));

        cleanup(&pool, &username).await;
    }

    /// 错误密码与不存在的用户报同一个错（防账号探测）
    #[tokio::test]
    async fn bad_password_and_unknown_user_share_error() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let store = AccountStore::new(pool.clone());
        let username = unique_username();
        store.register(&ids(), &username, "right-pw", "丙").await.expect("注册应成功");

        let wrong_pw = store.verify_login(&username, "wrong-pw").await;
        let unknown = store.verify_login(&unique_username(), "any-pw").await;
        assert!(matches!(wrong_pw, Err(AccountError::BadCredentials)));
        assert!(matches!(unknown, Err(AccountError::BadCredentials)));

        cleanup(&pool, &username).await;
    }

    /// 令牌全生命周期：签发 → 校验 → 撤销 → 失效；批量撤销踢全部
    #[tokio::test]
    async fn token_lifecycle_and_revocation() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let store = AccountStore::new(pool.clone());
        let username = unique_username();
        let user = store.register(&ids(), &username, "pw", "丁").await.expect("注册应成功");

        let ttl = Duration::from_secs(3600);
        let token1 = store.issue_token(user.id, ttl).await.expect("签发应成功");
        let token2 = store.issue_token(user.id, ttl).await.expect("签发应成功");
        assert_ne!(token1, token2, "两次签发应得到不同令牌");

        let by_token = store.user_by_token(&token1).await.expect("有效令牌应换到用户");
        assert_eq!(by_token.id, user.id);

        // 撤销单个：只有被撤销的失效
        assert!(store.revoke_token(&token1).await.expect("撤销应成功"));
        assert!(matches!(store.user_by_token(&token1).await, Err(AccountError::InvalidToken)));
        assert!(store.user_by_token(&token2).await.is_ok(), "另一个令牌不受影响");
        // 幂等：再撤销已失效的返回 false
        assert!(!store.revoke_token(&token1).await.expect("二次撤销应成功"));

        // 批量撤销（踢下线）：全部失效
        assert_eq!(store.revoke_all_tokens(user.id).await.expect("批量撤销应成功"), 1);
        assert!(matches!(store.user_by_token(&token2).await, Err(AccountError::InvalidToken)));

        cleanup(&pool, &username).await;
    }
}
