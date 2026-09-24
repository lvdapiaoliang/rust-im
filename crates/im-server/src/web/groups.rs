//! 群组域仓储（阶段 5 建 CRUD，阶段 7 上扇出 actor 与成员快照缓存）。

use sqlx::PgPool;

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
    /// 群 ID（雪花，即群消息的 `to`）。
    pub id: u64,
    /// 群名。
    pub name: String,
    /// 群主用户 ID。
    pub owner_id: u64,
}

/// 「我的群」视图（带我在群内的角色）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MyGroup {
    /// 群 ID。
    pub id: u64,
    /// 群名。
    pub name: String,
    /// 群主用户 ID。
    pub owner_id: u64,
    /// 我的角色：`owner` / `member`。
    pub role: String,
}

// 手写 FromRow：同 account::User，i64 → u64 的边界收敛在仓储层。
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Group {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: u64::try_from(row.try_get::<i64, _>("id")?).expect("雪花 ID 装得下 u64"),
            name: row.try_get("name")?,
            owner_id: u64::try_from(row.try_get::<i64, _>("owner_id")?)
                .expect("雪花 ID 装得下 u64"),
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
        let owner = i64::try_from(owner_id).expect("雪花 ID 装得下 i64");

        let mut tx = self.pool.begin().await?;
        let group = sqlx::query_as::<_, Group>(
            "INSERT INTO groups (id, name, owner_id) VALUES ($1, $2, $3)
             RETURNING id, name, owner_id",
        )
        .bind(i64::try_from(group_id).expect("雪花 ID 装得下 i64"))
        .bind(name.trim())
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query("INSERT INTO group_members (group_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(i64::try_from(group.id).expect("雪花 ID 装得下 i64"))
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
        let group = i64::try_from(group_id).expect("雪花 ID 装得下 i64");
        let operator = i64::try_from(operator_id).expect("雪花 ID 装得下 i64");

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
        .bind(i64::try_from(user_id).expect("雪花 ID 装得下 i64"))
        .execute(&self.pool)
        .await?;
        Ok(())
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
        .bind(i64::try_from(user_id).expect("雪花 ID 装得下 i64"))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, name, owner_id, role)| MyGroup {
                id: u64::try_from(id).expect("雪花 ID 装得下 u64"),
                name,
                owner_id: u64::try_from(owner_id).expect("雪花 ID 装得下 u64"),
                role,
            })
            .collect())
    }
}
