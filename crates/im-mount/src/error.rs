//! 挂载盘语义层的错误类型（阶段 13）。
//!
//! 错误按 **POSIX 的 errno 分类**命名（`NotFound` ≈ `ENOENT`、
//! `NotADirectory` ≈ `ENOTDIR`……）——挂载盘的错误最终要穿过
//! 驱动层报给操作系统，对齐内核的错误分类，驱动胶水就是纯翻译；
//! 自己发明一套分类，胶水层就得写「错误翻译的错误处理」。

use thiserror::Error;

/// 挂载盘语义层错误（POSIX errno 的语义层对齐版）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FsError {
    /// 路径不存在（`ENOENT`）
    #[error("路径不存在：{path}")]
    NotFound {
        /// 不存在的路径（报错带上现场，FUSE 侧要把它包进内核应答）
        path: String,
    },

    /// 路径的某段不是目录（`ENOTDIR`）
    #[error("路径组件不是目录：{path}")]
    NotADirectory {
        /// 出错路径
        path: String,
    },

    /// 对目录做了只有文件能做的操作（`EISDIR`）
    #[error("目标是目录：{path}")]
    IsADirectory {
        /// 出错路径
        path: String,
    },

    /// 创建时已有同名子项（`EEXIST`）
    #[error("已存在：{path}")]
    AlreadyExists {
        /// 出错路径
        path: String,
    },

    /// 非法路径（对根做 mkdir 之类没有父目录的操作）
    #[error("非法路径：{path}")]
    InvalidPath {
        /// 出错路径
        path: String,
    },

    /// inode 失踪：内部一致性被破坏时才会出现（正常 API 路径
    /// 不可达——出现即 bug，不该静默掩盖）
    #[error("inode 失踪（内部错误）：{ino}")]
    InodeGone {
        /// 失踪的 inode 号
        ino: u64,
    },
}
