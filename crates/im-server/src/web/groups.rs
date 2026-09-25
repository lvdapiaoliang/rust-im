//! 群组域仓储（阶段 5 建 CRUD，阶段 7 上扇出 actor 与成员快照缓存）。

use sqlx::PgPool;

use super::serde_id;
use super::{id_i64, id_u64};
use crate::session::Sessions;

/// 群组域错误。
#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    /// 群不存在。
    #[error("群不存在")]
    GroupNotFound,
    /// 只有群主能拉人（阶段 6 再评估邀请制/审批制）。
    #[error("只有群主可以添加成员")]
    NotOwner,
    /// 雪花发号不可用（时钟回拨）。
    #[error("ID 发号器暂不可用")]
    IdUnavailable,
    /// 数据库错误。
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),
}

/// 群组实体。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Group {
    /// 群 ID（雪花，即群消息的 `to`；JSON 形态为字符串）。
    #[serde(serialize_with = "serde_id::serialize")]
    pub id: u64,
    /// 群名。
    pub name: String,
    /// 群主用户 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub owner_id: u64,
}

/// 「我的群」视图（带我在群内的角色）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MyGroup {
    /// 群 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub id: u64,
    /// 群名。
    pub name: String,
    /// 群主用户 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub owner_id: u64,
    /// 我的角色：`owner` / `member`。
    pub role: String,
}

/// 群成员视图（阶段 7 前端群聊界面用：ID + 名字 + 角色）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupMember {
    /// 成员用户 ID。
    #[serde(serialize_with = "serde_id::serialize")]
    pub id: u64,
    /// 登录名。
    pub username: String,
    /// 展示名。
    pub display_name: String,
    /// 群内角色：`owner` / `member`。
    pub role: String,
}

// 手写 FromRow：同 account::User，i64 → u64 的边界收敛在仓储层。
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Group {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: id_u64(row.try_get::<i64, _>("id")?),
            name: row.try_get("name")?,
            owner_id: id_u64(row.try_get::<i64, _>("owner_id")?),
        })
    }
}

/// 群组仓储。
#[derive(Debug, Clone)]
pub struct GroupStore {
    pool: PgPool,
}

impl GroupStore {
    /// 用既有连接池构建。
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 建群：插 groups 行 + 群主入 members 表（同一事务：没有群主的群
    /// 不应存在半毫秒）。
    ///
    /// # Errors
    ///
    /// 发号失败 [`GroupError::IdUnavailable`]；数据库错误见 [`GroupError::Db`]。
    ///
    /// # Panics
    ///
    /// 雪花 ID 超出 `i64` 范围时 panic（发号器保证不会发生）。
    pub async fn create_group(
        &self,
        ids: &Sessions,
        name: &str,
        owner_id: u64,
    ) -> Result<Group, GroupError> {
        let Some(group_id) = ids.next_id().await else {
            return Err(GroupError::IdUnavailable);
        };
        let owner = id_i64(owner_id);

        let mut tx = self.pool.begin().await?;
        let group = sqlx::query_as::<_, Group>(
            "INSERT INTO groups (id, name, owner_id) VALUES ($1, $2, $3)
             RETURNING id, name, owner_id",
        )
        .bind(id_i64(group_id))
        .bind(name.trim())
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query("INSERT INTO group_members (group_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(id_i64(group.id))
            .bind(owner)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(group)
    }

    /// 群主拉人入群（幂等：已在群内直接返回成功）。
    ///
    /// # Errors
    ///
    /// 群不存在 [`GroupError::GroupNotFound`]；非群主 [`GroupError::NotOwner`]。
    ///
    /// # Panics
    ///
    /// 雪花 ID 超出 `i64` 范围时 panic（发号器保证不会发生）。
    pub async fn add_member(
        &self,
        group_id: u64,
        operator_id: u64,
        user_id: u64,
    ) -> Result<(), GroupError> {
        let group = id_i64(group_id);
        let operator = id_i64(operator_id);

        // 权限校验与插入之间没有锁——单机开发可接受；并发拉人竞争
        // 由主键约束兜底（重复插入幂等吞掉）
        let owner: Option<i64> = sqlx::query_scalar("SELECT owner_id FROM groups WHERE id = $1")
            .bind(group)
            .fetch_optional(&self.pool)
            .await?;
        match owner {
            None => return Err(GroupError::GroupNotFound),
            Some(owner) if owner != operator => return Err(GroupError::NotOwner),
            Some(_) => {}
        }

        sqlx::query(
            "INSERT INTO group_members (group_id, user_id) VALUES ($1, $2)
             ON CONFLICT (group_id, user_id) DO NOTHING",
        )
        .bind(group)
        .bind(id_i64(user_id))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 群是否存在（WS 消息权限分流：单聊要好友关系，群聊只要群存在）。
    ///
    /// 阶段 6 每条消息一次点查；阶段 7 群 actor 上线后，扇出路径已
    /// 换成员快照缓存，但**发送门槛**仍是每消息一次点查（见
    /// [`GroupStore::is_member`]）——门槛要防「知道群 ID 就能发言」，
    /// 快照在 actor 肚子里，门槛查不到它。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`GroupError::Db`]）。
    pub async fn is_group(&self, group_id: u64) -> Result<bool, GroupError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT id FROM groups WHERE id = $1")
            .bind(id_i64(group_id))
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }

    /// 全量成员 ID（阶段 7 扇出 actor 的快照装载，按 `user_id` 升序）。
    ///
    /// 一次查全表而不逐个 `is_member`：扇出是「一封信抄给所有人」，
    /// 2 万人点查 = 2 万次往返；快照只装载一次，之后全部扇出吃缓存。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`GroupError::Db`]）。
    pub async fn list_members(&self, group_id: u64) -> Result<Vec<u64>, GroupError> {
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT user_id FROM group_members WHERE group_id = $1 ORDER BY user_id",
        )
        .bind(id_i64(group_id))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(id_u64).collect())
    }

    /// 是否群成员（阶段 7 群消息门槛：非成员不能往群里发）。
    ///
    /// 与 [`GroupStore::is_group`] 同为每消息一次主键点查（本地回环 PG
    /// 微秒级）；门槛不能用 actor 快照——快照是扇出的私产，且未孵化
    /// actor 的群没有快照可查。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`GroupError::Db`]）。
    pub async fn is_member(&self, group_id: u64, user_id: u64) -> Result<bool, GroupError> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT user_id FROM group_members WHERE group_id = $1 AND user_id = $2",
        )
        .bind(id_i64(group_id))
        .bind(id_i64(user_id))
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    /// 成员视图列表（阶段 7 前端群成员展示；联 users 表取名字）。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`GroupError::Db`]）。
    pub async fn list_member_users(&self, group_id: u64) -> Result<Vec<GroupMember>, GroupError> {
        let rows = sqlx::query_as::<_, (i64, String, String, String)>(
            "SELECT u.id, u.username, u.display_name, m.role
             FROM group_members m
             JOIN users u ON u.id = m.user_id
             WHERE m.group_id = $1
             ORDER BY u.id",
        )
        .bind(id_i64(group_id))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, username, display_name, role)| GroupMember {
                id: id_u64(id),
                username,
                display_name,
                role,
            })
            .collect())
    }

    /// 我加入的群（含自建）。
    ///
    /// # Errors
    ///
    /// 数据库错误（见 [`GroupError::Db`]）。
    pub async fn my_groups(&self, user_id: u64) -> Result<Vec<MyGroup>, GroupError> {
        let rows = sqlx::query_as::<_, (i64, String, i64, String)>(
            "SELECT g.id, g.name, g.owner_id, m.role
             FROM group_members m
             JOIN groups g ON g.id = m.group_id
             WHERE m.user_id = $1
             ORDER BY g.id",
        )
        .bind(id_i64(user_id))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, name, owner_id, role)| MyGroup {
                id: id_u64(id),
                name,
                owner_id: id_u64(owner_id),
                role,
            })
            .collect())
    }
}
