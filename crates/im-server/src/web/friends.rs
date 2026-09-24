//! 好友域仓储：请求状态机 + 无向边关系（阶段 5 建 CRUD，阶段 6 补 WS 推送）。
//!
//! # 状态机
//!
//! ```text
//! pending ──accept（收方）──▶ accepted（插 friends 边）
//!    │
//!    └──reject（收方）──────▶ rejected（可重新发起：upsert 回 pending）
//! ```
//!
//! # 模式落点
//!
//! - **事务边界**：接受好友 = 「改状态 + 插边」两步，必须同生共死
//!   （`BEGIN` ... `COMMIT`，中途任何失败整体回滚）；
//! - **幂等设计**：重新请求用 `ON CONFLICT DO UPDATE` 回 `pending`，
//!   重复接受用 `ON CONFLICT DO NOTHING` 吞掉已存在的边。

use sqlx::PgPool;

use crate::session::Sessions;

use super::account::User;
use super::serde_id;
use super::{id_i64, id_u64};

/// 好友域错误。
#[derive(Debug, thiserror::Error)]
pub enum FriendError {
    /// 请求不存在、不是发向自己、或已处理过（状态机不允许的迁移）。
    #[error("请求不存在或无法处理")]
    RequestNotFound,
    /// 不能加自己为好友（自环边没有业务意义）。
    #[error("不能添加自己为好友")]
    SelfRequest,
    /// 目标用户不存在。
    #[error("目标用户不存在")]
    UserNotFound,
    /// 雪花发号不可用（时钟回拨）。
    #[error("ID 发号器暂不可用")]
    IdUnavailable,
    /// 数据库错误。
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),
}

/// 好友请求（列表视图：带双方用户名，前端直接渲染）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FriendRequestView {
    /// 请求 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub id: u64,
    /// 发起方用户 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub from_user: u64,
    /// 发起方用户名。
    pub from_username: String,
    /// 接收方用户 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub to_user: u64,
    /// 接收方用户名。
    pub to_username: String,
    /// 状态：`pending` / `accepted` / `rejected`。
    pub status: String,
}

/// 好友仓储。
#[derive(Debug, Clone)]
pub struct FriendStore {
    pool: PgPool,
}

impl FriendStore {
    /// 用既有连接池构建。
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 发起好友请求：`from → to`。
    ///
    /// 已存在（含被拒过的）请求会被**重置回 pending**（重新发起语义）；
    /// 已是好友时返回 [`FriendError::RequestNotFound`] 之外的语义错误——
    /// 直接返回 `Ok`（视为请求成功，接受时幂等吞边）。
    ///
    /// # Errors
    ///
    /// 自环请求 [`FriendError::SelfRequest`]；目标不存在
    /// [`FriendError::UserNotFound`]；见 [`FriendError`] 其余变体。
    pub async fn create_request(
        &self,
        ids: &Sessions,
        from_user: u64,
        to_user: u64,
    ) -> Result<FriendRequestView, FriendError> {
        if from_user == to_user {
            return Err(FriendError::SelfRequest);
        }
        // 目标必须存在：外键其实兜底，但先查能给出更准确的业务错误
        let target: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
            .bind(id_i64(to_user))
            .fetch_optional(&self.pool)
            .await?;
        if target.is_none() {
            return Err(FriendError::UserNotFound);
        }

        let Some(request_id) = ids.next_id().await else {
            return Err(FriendError::IdUnavailable);
        };

        // upsert：被拒后重新发起 = 状态回到 pending（created_at 不动，
        // 保留首次发起时间——「认识多久」比「最近一次纠缠」更有信息量）
        let row = sqlx::query_as::<_, (i64, i64, i64, String)>(
            "INSERT INTO friend_requests (id, from_user, to_user)
             VALUES ($1, $2, $3)
             ON CONFLICT (from_user, to_user)
             DO UPDATE SET status = 'pending'
             RETURNING id, from_user, to_user, status",
        )
        .bind(id_i64(request_id))
        .bind(id_i64(from_user))
        .bind(id_i64(to_user))
        .fetch_one(&self.pool)
        .await?;

        // RETURNING 没带双方用户名：补一次轻量查询（两个主键点查）
        let (from_username, to_username) =
            self.fetch_usernames(row.1, row.2).await.ok_or(FriendError::UserNotFound)?;
        Ok(FriendRequestView {
            id: id_u64(row.0),
            from_user: id_u64(row.1),
            from_username,
            to_user: id_u64(row.2),
            to_username,
            status: row.3,
        })
    }

    /// 我收到 / 我发出的待处理请求。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`FriendError::Db`]）。
    pub async fn pending_requests(
        &self,
        user_id: u64,
    ) -> Result<(Vec<FriendRequestView>, Vec<FriendRequestView>), FriendError> {
        let incoming = sqlx::query_as::<_, (i64, i64, String, i64, String)>(
            "SELECT r.id, r.from_user, fu.username, r.to_user, tu.username
             FROM friend_requests r
             JOIN users fu ON fu.id = r.from_user
             JOIN users tu ON tu.id = r.to_user
             WHERE r.to_user = $1 AND r.status = 'pending'
             ORDER BY r.id",
        )
        .bind(id_i64(user_id))
        .fetch_all(&self.pool)
        .await?;

        let outgoing = sqlx::query_as::<_, (i64, i64, String, i64, String)>(
            "SELECT r.id, r.from_user, fu.username, r.to_user, tu.username
             FROM friend_requests r
             JOIN users fu ON fu.id = r.from_user
             JOIN users tu ON tu.id = r.to_user
             WHERE r.from_user = $1 AND r.status = 'pending'
             ORDER BY r.id",
        )
        .bind(id_i64(user_id))
        .fetch_all(&self.pool)
        .await?;

        Ok((
            incoming.into_iter().map(to_view).collect(),
            outgoing.into_iter().map(to_view).collect(),
        ))
    }

    /// 接受请求（仅收方有权）：改状态 + 插无向边，一个事务内完成。
    ///
    /// # Errors
    ///
    /// 请求不存在 / 不是发向自己 / 已处理过 → [`FriendError::RequestNotFound`]。
    pub async fn accept_request(&self, request_id: u64, by_user: u64) -> Result<(), FriendError> {
        let mut tx = self.pool.begin().await?;

        // 带状态条件的 UPDATE：天然实现「只有收方能处理 pending 请求」
        let from_user: Option<i64> = sqlx::query_scalar(
            "UPDATE friend_requests SET status = 'accepted'
             WHERE id = $1 AND to_user = $2 AND status = 'pending'
             RETURNING from_user",
        )
        .bind(id_i64(request_id))
        .bind(id_i64(by_user))
        .fetch_optional(&mut *tx)
        .await?;

        let Some(from_user) = from_user else {
            // 不回滚也无妨（什么都没改），但显式回滚语义更清晰
            tx.rollback().await?;
            return Err(FriendError::RequestNotFound);
        };

        // 无向边：小 ID 恒在前（表结构 CHECK 兜底）
        let a = from_user.min(id_i64(by_user));
        let b = from_user.max(id_i64(by_user));
        // 已是好友（互相发起等场景）：边幂等吞掉，请求状态照常推进
        sqlx::query(
            "INSERT INTO friends (user_a, user_b) VALUES ($1, $2)
             ON CONFLICT (user_a, user_b) DO NOTHING",
        )
        .bind(a)
        .bind(b)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// 拒绝请求（仅收方有权）。
    ///
    /// # Errors
    ///
    /// 请求不存在 / 不是发向自己 / 已处理过 → [`FriendError::RequestNotFound`]。
    pub async fn reject_request(&self, request_id: u64, by_user: u64) -> Result<(), FriendError> {
        let updated = sqlx::query(
            "UPDATE friend_requests SET status = 'rejected'
             WHERE id = $1 AND to_user = $2 AND status = 'pending'",
        )
        .bind(id_i64(request_id))
        .bind(id_i64(by_user))
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(FriendError::RequestNotFound);
        }
        Ok(())
    }

    /// 我的好友列表（无向边两侧点查的另一端）。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`FriendError::Db`]）。
    pub async fn list_friends(&self, user_id: u64) -> Result<Vec<User>, FriendError> {
        let users = sqlx::query_as::<_, User>(
            "SELECT u.id, u.username, u.display_name
             FROM friends f
             JOIN users u ON u.id = CASE WHEN f.user_a = $1 THEN f.user_b ELSE f.user_a END
             WHERE f.user_a = $1 OR f.user_b = $1
             ORDER BY u.id",
        )
        .bind(id_i64(user_id))
        .fetch_all(&self.pool)
        .await?;
        Ok(users)
    }

    /// 删除好友（任一方可删，双向对称）。
    ///
    /// # Errors
    ///
    /// 本就不是好友时返回 [`FriendError::RequestNotFound`]（复用「关系不存在」语义）。
    pub async fn remove_friend(&self, me: u64, other: u64) -> Result<(), FriendError> {
        let a = id_i64(me.min(other));
        let b = id_i64(me.max(other));
        let deleted = sqlx::query("DELETE FROM friends WHERE user_a = $1 AND user_b = $2")
            .bind(a)
            .bind(b)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(FriendError::RequestNotFound);
        }
        Ok(())
    }

    /// 拉两个用户名（请求视图组装用；任一不存在返回 `None`）。
    async fn fetch_usernames(&self, a: i64, b: i64) -> Option<(String, String)> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, username FROM users WHERE id IN ($1, $2)",
        )
        .bind(a)
        .bind(b)
        .fetch_all(&self.pool)
        .await
        .ok()?;
        let a_name = rows.iter().find(|(id, _)| *id == a).map(|(_, name)| name.clone())?;
        let b_name = rows.iter().find(|(id, _)| *id == b).map(|(_, name)| name.clone())?;
        Some((a_name, b_name))
    }
}

/// 行元组 → 视图（`pending_requests` 的辅助）。
fn to_view(row: (i64, i64, String, i64, String)) -> FriendRequestView {
    FriendRequestView {
        id: id_u64(row.0),
        from_user: id_u64(row.1),
        from_username: row.2,
        to_user: id_u64(row.3),
        to_username: row.4,
        status: "pending".to_string(),
    }
}
