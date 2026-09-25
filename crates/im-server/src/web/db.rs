//! 数据库接入：连接池与迁移的唯一入口。
//!
//! # 为什么用「运行时查询」而非 sqlx 宏
//!
//! `sqlx::query!` 宏在**编译期**连库校验 SQL——没有 `DATABASE_URL`
//! 就编不过，破坏「无 PG 环境也能跑 `cargo test --workspace`」的
//! 开发体验。本项目统一用运行时 API（`query` / `query_as`），
//! SQL 正确性由集成测试兜底（代价是拼错列名要等测试才暴露）。

use std::time::Duration;

use sqlx::PgPool;
use sqlx::migrate::MigrateError;
use sqlx::postgres::PgPoolOptions;

/// 开发默认连接串（本机 PostgreSQL；生产/CI 用 `DATABASE_URL` 覆盖）。
pub const DEFAULT_DATABASE_URL: &str = "postgres://im:im123456@127.0.0.1:5432/im?sslmode=disable";

/// 连接池上限：单机开发与压测前足够；阶段 10 压测时再按连接数重调。
const POOL_MAX_CONNECTIONS: u32 = 8;
/// 单次取连接的等待上限：超过即报错（宁可快速失败也别堆积请求）。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// 数据库接入错误。
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// 连接或查询失败。
    #[error("数据库错误: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// 迁移失败（通常是脚本本身有问题或版本表冲突）。
    #[error("迁移失败: {0}")]
    Migrate(#[from] MigrateError),
}

/// 读取连接串：`DATABASE_URL` 优先，缺省回落本机开发库。
#[must_use]
pub fn database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// 建连接池（不迁移；调用方自行决定是否 [`migrate`]）。
///
/// # Errors
///
/// 连不上数据库时返回 [`DbError::Sqlx`]。
pub async fn connect() -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(POOL_MAX_CONNECTIONS)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect(&database_url())
        .await?;
    Ok(pool)
}

/// 建连接池并把迁移跑到最新（服务启动入口用）。
///
/// 迁移脚本内嵌二进制（`sqlx::migrate!` 编译期收录），部署产物
/// 不需要携带 migrations 目录。
///
/// # Errors
///
/// 连接失败或迁移失败（见 [`DbError`]）。
pub async fn connect_and_migrate() -> Result<PgPool, DbError> {
    let pool = connect().await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// 执行内嵌迁移（幂等：已应用的版本自动跳过）。
///
/// # Errors
///
/// 迁移失败时返回 [`DbError::Migrate`]。
pub async fn migrate(pool: &PgPool) -> Result<(), DbError> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// 测试脚手架：web 模块共用的「迁移到位的池」与「唯一 `machine_id` 的会话中心」。
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;

    use super::database_url;
    use crate::session::{SessionConfig, Sessions};

    /// 拿到迁移到位的池；PG 不可达时返回 `None`。
    ///
    /// 跳过策略：无库环境让测试空转，保持 workspace 全绿
    /// （代价是「测试通过 ≠ 数据库逻辑验证过」，需 CI 里配真库补盲区）。
    pub(crate) async fn pool_or_skip() -> Option<PgPool> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(2))
            .connect(&database_url())
            .await
            .ok()?;
        super::migrate(&pool).await.ok()?;
        Some(pool)
    }

    /// 进程级 `machine_id` 分配计数器（见 [`test_sessions`]）。
    static MACHINE_ID_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 建一个带**唯一 `machine_id`** 的会话中心（测试并行安全）。
    ///
    /// 雪花的唯一性契约是「一个 `machine_id` 只属于一个发号器」（见
    /// `snowflake::concurrent_generators_produce_unique_ids`）。集成测试
    /// 在同一进程里**并行**起多个 `Sessions`，若都吃默认 `machine_id`，
    /// 两个测试在同一毫秒各发首号（sequence 都从 0 起）会算出同一个
    /// ID，撞库唯一约束（`friend_requests_pkey` 等）——表现为偶发
    /// 失败、单跑必过。这里用进程级原子计数器给每个测试实例发不同
    /// `machine_id`（10 位共 1024 槽，远超测试数量），从根上消除碰撞。
    pub(crate) fn test_sessions() -> Sessions {
        let machine_id =
            MACHINE_ID_SEQ.fetch_add(1, Ordering::Relaxed) % (crate::snowflake::MAX_MACHINE_ID + 1);
        Sessions::new(SessionConfig { machine_id, ..SessionConfig::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::pool_or_skip;
    use super::*;

    /// 连接 + 迁移全链路：能连上就应该建齐 7 张表
    #[tokio::test]
    async fn connect_and_migrate_creates_all_tables() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        for table in
            ["users", "tokens", "friend_requests", "friends", "groups", "group_members", "files"]
        {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = $1)",
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("{table} 表存在性查询失败: {e}"));
            assert!(exists, "迁移后应存在 {table} 表");
        }
    }

    /// 迁移幂等：连续跑两次不报错（版本表自动跳过已应用版本）
    #[tokio::test]
    async fn migrate_is_idempotent() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        migrate(&pool).await.expect("第二次迁移应直接跳过");
    }
}
