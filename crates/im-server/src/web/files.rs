//! 文件域仓储：multipart 落盘 + sha256 完整性元数据 + 鉴权下载。
//!
//! # 为什么内容不进数据库
//!
//! PG 存大 blob 会拖垮 buffer pool 与备份窗口；本项目的内容在磁盘
//! `data/files/{id}`，DB 只记「谁传的、叫什么、多大、指纹是什么」。
//! 代价是**元数据与内容可能不一致**（写盘成功后进程崩溃，DB 里没行）——
//! 写入顺序刻意选「先落盘、后落库」：孤儿文件无害（无主不可达），
//! 反过来（有行无文件）则会让下载 500。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::{id_i64, id_u64};
use crate::session::Sessions;

/// 文件域错误。
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// 文件不存在（或已删除）。
    #[error("文件不存在")]
    NotFound,
    /// 超过单文件上限（保护内存：multipart 全量缓存后再落盘）。
    #[error("文件超过大小上限")]
    TooLarge,
    /// 磁盘读写失败。
    #[error("磁盘 IO 错误: {0}")]
    Io(#[from] std::io::Error),
    /// 雪花发号不可用（时钟回拨）。
    #[error("ID 发号器暂不可用")]
    IdUnavailable,
    /// 数据库错误。
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),
}

/// 单文件大小上限：全量缓存进内存再落盘，上限即内存保护线。
pub const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;

/// 文件元数据。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileMeta {
    /// 文件 ID（雪花，下载 URL 的 `{id}`）。
    pub id: u64,
    /// 上传者用户 ID。
    pub owner_id: u64,
    /// 原始文件名（下载时回填 `Content-Disposition`）。
    pub filename: String,
    /// 字节数。
    pub size_bytes: u64,
    /// 内容的 SHA-256（十六进制）。
    pub sha256: String,
}

// 手写 FromRow：同 account::User，i64 → u64 的边界收敛在仓储层。
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for FileMeta {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: id_u64(row.try_get::<i64, _>("id")?),
            owner_id: id_u64(row.try_get::<i64, _>("owner_id")?),
            filename: row.try_get("filename")?,
            size_bytes: u64::try_from(row.try_get::<i64, _>("size_bytes")?)
                .expect("文件大小装得下 u64"),
            sha256: row.try_get("sha256")?,
        })
    }
}

/// 文件仓储：磁盘 + DB 双写（顺序见模块文档）。
#[derive(Debug, Clone)]
pub struct FileStore {
    pool: PgPool,
    /// 内容根目录（`data/files/`）。
    root: PathBuf,
}

impl FileStore {
    /// 用既有连接池与内容根目录构建（目录不存在则自动创建）。
    ///
    /// # Errors
    ///
    /// 创建目录失败返回 [`FileError::Io`]。
    pub async fn new(pool: PgPool, root: impl Into<PathBuf>) -> Result<Self, FileError> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self { pool, root })
    }

    /// 保存文件：落盘 → 写元数据（见模块文档的顺序取舍）。
    ///
    /// # Errors
    ///
    /// 超上限 [`FileError::TooLarge`]；发号失败 [`FileError::IdUnavailable`]；
    /// 磁盘/数据库错误见 [`FileError::Io`] / [`FileError::Db`]。
    ///
    /// # Panics
    ///
    /// 雪花 ID 超出 `i64` 范围时 panic（发号器保证不会发生）。
    pub async fn save(
        &self,
        ids: &Sessions,
        owner_id: u64,
        filename: &str,
        content: &[u8],
    ) -> Result<FileMeta, FileError> {
        if content.len() > MAX_FILE_SIZE {
            return Err(FileError::TooLarge);
        }
        let Some(file_id) = ids.next_id().await else {
            return Err(FileError::IdUnavailable);
        };

        // SHA-256：流式吸收（实现上这里是全量内存，但接口按 chunk 设计，
        // 将来换流式落盘时指纹逻辑不用动）
        let mut hasher = Sha256::new();
        hasher.update(content);
        let sha256 = hex(&hasher.finalize());

        // 先落盘（孤儿文件无害），再落库
        let path = self.path_of(file_id);
        tokio::fs::write(&path, content).await?;
        let meta = sqlx::query_as::<_, FileMeta>(
            "INSERT INTO files (id, owner_id, filename, size_bytes, sha256)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, owner_id, filename, size_bytes, sha256",
        )
        .bind(id_i64(file_id))
        .bind(id_i64(owner_id))
        .bind(filename)
        .bind(i64::try_from(content.len()).expect("大小已限 64MiB，装得下 i64"))
        .bind(&sha256)
        .fetch_one(&self.pool)
        .await
        // DB 失败时尽力删掉孤儿文件（删不掉也无害，只是占磁盘）
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;
        Ok(meta)
    }

    /// 取文件元数据。
    ///
    /// # Errors
    ///
    /// 不存在返回 [`FileError::NotFound`]；数据库错误见 [`FileError::Db`]。
    pub async fn meta(&self, file_id: u64) -> Result<FileMeta, FileError> {
        sqlx::query_as::<_, FileMeta>(
            "SELECT id, owner_id, filename, size_bytes, sha256 FROM files WHERE id = $1",
        )
        .bind(id_i64(file_id))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(FileError::NotFound)
    }

    /// 读文件内容（元数据 + 字节）。下载端点用。
    ///
    /// # Errors
    ///
    /// 元数据不存在 [`FileError::NotFound`]；磁盘文件缺失/读取失败
    /// 见 [`FileError::Io`]（有行无文件=脏数据，运维告警场景）。
    pub async fn read(&self, file_id: u64) -> Result<(FileMeta, Vec<u8>), FileError> {
        let meta = self.meta(file_id).await?;
        let content = tokio::fs::read(self.path_of(file_id)).await?;
        Ok((meta, content))
    }

    /// 文件在磁盘上的路径：`root/{id}`（不用原文件名——
    /// 用户文件名可能含非法字符/超长/重名，ID 即键）。
    fn path_of(&self, file_id: u64) -> PathBuf {
        self.root.join(file_id.to_string())
    }

    /// 内容根目录（诊断与测试）。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// 字节 → 十六进制小写（指纹展示形态，与 `sha256sum` 输出一致）。
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}
