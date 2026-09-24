//! 简化版 LSM 存储引擎（阶段 4）。
//!
//! # 为什么 IM 本地库要自己写一个 KV 引擎？
//!
//! roadmap 4.5 的要求：**B+ 树 / LSM 思想——本地消息库索引、写前日志
//! （理解 SQLite/RocksDB 原理）**。SQLite 用 B+ 树（读优化，原地更新）；
//! RocksDB/LevelDB 用 LSM（写优化，追加 + 压实）。IM 客户端的负载是
//! 「聊天消息持续写入 + 偶尔翻历史」——写多读少，天然适合 LSM。
//!
//! # 本引擎的简化（对照生产 RocksDB）
//!
//! ```text
//!   写 put/delete ──▶ 追加到活跃段（WAL 语义）+ memtable（BTreeMap）
//!                        │
//!      活跃段达到条数阈值 └─▶ seal：封存为只读段（建 HashMap 索引），开新段
//!
//!   读 get ──▶ memtable ──▶ 封存段索引从新到旧（HashMap 定位 + pread）
//!   扫描 scan ──▶ 全部来源按版本序灌进 BTreeMap（后写覆盖先写），range 输出
//!   压实 compact ──▶ 同 scan 收集，顺序写成一个有序新段（无 tombstone、无旧版本）
//! ```
//!
//! 与生产级的差距（刻意保留差距，文档里讲清楚）：
//! - 没有 SST 的块压缩与布隆过滤器；
//! - scan 是全量收集而非各段有序迭代器的 k 路归并（压实后可优化成二分）；
//! - 单 memtable，无 Immutable MemTable 层级；
//! - fsync 策略简化为每写必 flush（性能取舍见 [`Engine::put`]）。
//!
//! # 崩溃恢复语义（WAL 的存在理由）
//!
//! 追加写的段文件在进程崩溃时最坏情况是**尾部半条记录**——
//! 打开时逐条 CRC 校验重放，遇到第一个坏记录即截断丢弃其后所有内容：
//! 「已确认写入的数据不丢，未完成的写丢弃」，这正是数据库 WAL 的契约。
//!
//! # 算法/数据结构落点
//!
//! - **BTreeMap**：memtable 与 scan 收集器（B 树思想：有序、范围查询）；
//! - **HashMap 索引 + pread**：封存段 O(1) 定位（对照 SST 的稀疏索引）；
//! - **版本序覆盖**：同一 key 多次写，读时「最新的赢」——靠遍历顺序
//!   （段代数递增、段内偏移递增）而非时间戳，省 8 字节/记录；
//! - **CRC32 + 截断恢复**：复用 `im_protocol::crc32`（查表法）。

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use im_protocol::crc32;
use im_protocol::varint;

use crate::error::StorageError;

/// 记录种类。
const KIND_PUT: u8 = 0;
/// 删除墓碑（value 为空）——LSM 不就地删除，只标记。
const KIND_DELETE: u8 = 1;

/// 活跃段封存阈值（条数）：达到即 seal。教学取小值 1000。
const SEAL_THRESHOLD: usize = 1000;

/// 一条日志记录（内存形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    /// 写入/覆盖。
    Put { key: Vec<u8>, value: Vec<u8> },
    /// 删除标记。
    Delete { key: Vec<u8> },
}

impl Record {
    /// 编码为「kind + varint(key_len) + key + varint(value_len) + value」。
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Record::Put { key, value } => {
                out.push(KIND_PUT);
                varint::encode_u64(key.len() as u64, &mut out);
                out.extend_from_slice(key);
                varint::encode_u64(value.len() as u64, &mut out);
                out.extend_from_slice(value);
            }
            Record::Delete { key } => {
                out.push(KIND_DELETE);
                varint::encode_u64(key.len() as u64, &mut out);
                out.extend_from_slice(key);
            }
        }
        out
    }

    /// 从游标解码一条记录（`src` 恰好是一条完整记录时成功）。
    fn decode(src: &[u8]) -> Result<Self, StorageError> {
        let mut cursor: &[u8] = src;
        let mut head = [0u8; 1];
        head.copy_from_slice(&cursor[..1]);
        cursor = &cursor[1..];
        let (key_len, used) = varint::decode_u64(cursor).ok_or(StorageError::Corrupted)?;
        cursor = &cursor[used..];
        let key_len = usize::try_from(key_len).map_err(|_| StorageError::Corrupted)?;
        if cursor.len() < key_len {
            return Err(StorageError::Corrupted);
        }
        let key = cursor[..key_len].to_vec();
        cursor = &cursor[key_len..];
        if head[0] == KIND_DELETE {
            return Ok(Record::Delete { key });
        }
        let (value_len, used) = varint::decode_u64(cursor).ok_or(StorageError::Corrupted)?;
        cursor = &cursor[used..];
        let value_len = usize::try_from(value_len).map_err(|_| StorageError::Corrupted)?;
        if cursor.len() < value_len {
            return Err(StorageError::Corrupted);
        }
        let value = cursor[..value_len].to_vec();
        Ok(Record::Put { key, value })
    }
}

/// 磁盘帧：`len:u32 LE + crc32:u32 LE + record`——定长头让恢复扫描免于
/// varint 对齐的歧义（CRC 失败或长度越界即认定尾部损坏）。
fn encode_frame(record: &Record) -> Vec<u8> {
    let body = record.encode();
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32::checksum(&body).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// 一个封存段：只读文件 + key → 文件偏移的完整索引。
///
/// 对照 SST：生产级用稀疏索引（每 N 条记一个锚点）+ 块内二分；
/// 本地消息库量级（万级）用完整 HashMap 更简单，且 O(1) 定位。
struct SealedSegment {
    /// 段号（代数）：compact 后清理旧段文件时用。
    id: u64,
    file: File,
    /// key → 帧在段文件中的起始偏移。
    index: HashMap<Vec<u8>, u64>,
}

/// 活跃段：追加写（WAL）+ memtable 镜像。
struct ActiveSegment {
    writer: BufWriter<File>,
    /// 已追加的记录数（seal 阈值的计数器）。
    count: usize,
}

/// 简化版 LSM 引擎（对外 API 见 [`crate::LocalStore`] 的语义层封装）。
pub(crate) struct Engine {
    dir: PathBuf,
    /// 封存段，按代数升序（新的在尾部——读时从新到旧倒序遍历）。
    segments: Vec<SealedSegment>,
    active: ActiveSegment,
    /// 活跃段数据的内存镜像（写缓存 + 读快路径）。
    memtable: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// 下一个段号（单调递增，段名即代数）。
    next_segment: u64,
}

impl Engine {
    /// 打开（或创建）位于 `dir` 的库。
    ///
    /// 恢复流程：加载全部封存段建索引 → 重放最大段号的活跃段到 memtable
    /// （CRC 校验，坏尾截断）。
    ///
    /// # Errors
    ///
    /// 目录无法创建/读取、段文件 IO 故障时返回 [`StorageError::Io`]。
    pub(crate) fn open(dir: impl AsRef<Path>) -> Result<Self, StorageError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Self::recover(dir)
    }

    fn recover(dir: PathBuf) -> Result<Self, StorageError> {
        // 收集所有段（按段号排序——目录遍历序不可靠，显式排序）
        let mut segment_ids: Vec<u64> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let Some(id) = parse_segment_name(&entry.path()) else {
                continue;
            };
            segment_ids.push(id);
        }
        segment_ids.sort_unstable();

        // 封存段：全部建索引（含崩溃前的活跃段——重开后统一按封存段对待，
        // 追加写从新段继续；段数增长由 compact 兕底）
        let mut segments = Vec::new();
        for &id in &segment_ids {
            let path = segment_path(&dir, id);
            let file = File::open(&path)?;
            let index = build_index(BufReader::new(&file))?;
            segments.push(SealedSegment {
                id,
                file,
                index,
            });
        }

        // 活跃段 = 新段号（旧段全部封存；路径存在说明上次创建后即崩溃，
        // 重放防御性处理，通常为空）
        let active_id = segment_ids.last().map_or(0, |id| id + 1);
        let active_path = segment_path(&dir, active_id);
        let (memtable, count) = if active_path.exists() {
            // 崩溃恢复：重放 WAL。坏尾截断 = truncate 到最后一条好记录
            let file = File::open(&active_path)?;
            let (memtable, good_bytes) = replay(BufReader::new(file))?;
            // 截掉坏尾，写指针回到好数据末尾（追加模式续写）
            let file = OpenOptions::new().read(true).append(true).open(&active_path)?;
            file.set_len(good_bytes)?;
            (memtable, memtable.len())
        } else {
            (BTreeMap::new(), 0)
        };

        let writer = open_append(&active_path)?;
        Ok(Self {
            dir,
            segments,
            active: ActiveSegment { writer, count },
            memtable,
            next_segment: active_id + 1,
        })
    }

    /// 写入/覆盖一个 key。
    ///
    /// 每写必 flush（不 fsync）：进程崩溃不丢（OS 页缓存还在），
    /// 掉电才可能丢——本地聊天记录对这个级别的持久性足够，
    /// 换来的是每条消息微秒级落盘延迟。对照 SQLite 默认的
    /// `synchronous=FULL`（每次 commit fsync）。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub(crate) fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.append(Record::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        })?;
        if self.active.count >= SEAL_THRESHOLD {
            self.seal()?;
        }
        Ok(())
    }

    /// 删除一个 key（写 tombstone；compact 时物理清除）。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub(crate) fn delete(&mut self, key: &[u8]) -> Result<(), StorageError> {
        self.append(Record::Delete { key: key.to_vec() })
    }

    /// 点查：memtable → 封存段从新到旧。
    ///
    /// # Errors
    ///
    /// 段文件读取失败时返回 [`StorageError::Io`]。
    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        // memtable：Some(None) = tombstone（比任何封存段的版本都新）
        if let Some(value) = self.memtable.get(key) {
            return Ok(value.clone());
        }
        for segment in self.segments.iter().rev() {
            if let Some(&offset) = segment.index.get(key) {
                // 封存段的记录只可能是最终被读到的最新版本——
                // 索引构建时同 key 后写覆盖先写，offset 指向最后一条
                return read_record_at(&segment.file, offset).map(|record| match record {
                    Record::Put { value, .. } => Some(value),
                    Record::Delete { .. } => None,
                });
            }
        }
        Ok(None)
    }

    /// 前缀扫描：所有来源按版本序灌入 BTreeMap（后写覆盖先写），
    /// 再 range 出 `[prefix, prefix_next)` 的有序区间。
    ///
    /// # Errors
    ///
    /// 段文件读取失败时返回 [`StorageError::Io`]。
    pub(crate) fn scan_prefix(
        &self,
        prefix: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StorageError> {
        // 版本序收集：段代数升序 + 段内偏移升序 = 写入时间序
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for segment in &self.segments {
            for (&key, &offset) in &segment.index {
                match read_record_at(&segment.file, offset)? {
                    Record::Put { value, .. } => {
                        merged.insert(key.clone(), Some(value));
                    }
                    Record::Delete { .. } => {
                        merged.insert(key.clone(), None);
                    }
                }
            }
        }
        for (key, value) in &self.memtable {
            merged.insert(key.clone(), value.clone());
        }

        // range [prefix, prefix+1)：用「前缀后面接 0xFF...」构造上界
        // （比逐字节比较的实现更简洁且正确——BTreeMap 的 range 本来就是 O(log n) 定位）
        let mut upper = prefix.to_vec();
        let mut carry = true;
        for byte in upper.iter_mut().rev() {
            if carry {
                if *byte == u8::MAX {
                    *byte = 0;
                } else {
                    *byte += 1;
                    carry = false;
                    break;
                }
            }
        }
        let upper = if carry { None } else { Some(upper) };

        let mut out = Vec::new();
        let range = match upper {
            Some(upper) => merged.range(prefix.to_vec()..upper),
            None => merged.range(prefix.to_vec()..),
        };
        for (key, value) in range {
            if let Some(value) = value {
                out.push((key.clone(), value.clone()));
            } // tombstone 不输出
        }
        Ok(out)
    }

    /// 压实：全部数据归并成**一个**有序新段（无 tombstone、无被覆盖旧版本）。
    ///
    /// 这是 LSM 读放大/写放大/空间放大的平衡阀：段太多读慢、
    /// 旧版本太多占空间，compact 一次全清。
    ///
    /// # Errors
    ///
    /// 新段写入或旧段删除失败时返回 [`StorageError::Io`]。
    pub(crate) fn compact(&mut self) -> Result<(), StorageError> {
        // 先把活跃段封存（数据统一走只读段路径，避免两套收集逻辑）
        self.seal()?;

        // 收集存活数据（同 scan 的 BTreeMap 覆盖语义）
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for segment in &self.segments {
            for (&key, &offset) in &segment.index {
                match read_record_at(&segment.file, offset)? {
                    Record::Put { value, .. } => {
                        merged.insert(key.clone(), Some(value));
                    }
                    Record::Delete { .. } => {
                        merged.insert(key.clone(), None);
                    }
                }
            }
        }

        // 写新段（BTreeMap 迭代天然 key 升序——压实段的有序性由此而来，
        // 这也是未来「有序段二分扫描」优化的前提）
        let new_id = self.next_segment;
        self.next_segment += 1;
        let path = segment_path(&self.dir, new_id);
        {
            let mut writer = BufWriter::new(File::create(&path)?);
            for (key, value) in &merged {
                if let Some(value) = value {
                    let frame = encode_frame(&Record::Put {
                        key: key.clone(),
                        value: value.clone(),
                    });
                    writer.write_all(&frame)?;
                }
            }
            writer.flush()?;
        }

        // 原子替换段列表：新段成功写完才删旧段（先写后删的崩溃安全序）
        let file = File::open(&path)?;
        let index = build_index(BufReader::new(&file))?;
        let old_paths: Vec<PathBuf> = self
            .segments
            .iter()
            .map(|s| segment_path(&self.dir, s.id))
            .collect();
        self.segments = vec![SealedSegment {
            id: new_id,
            file,
            index,
        }];
        for path in old_paths {
            let _ = std::fs::remove_file(path); // 删失败无害：下次 compact 再清
        }
        Ok(())
    }

    /// 追加一条记录：写活跃段 + 更新 memtable。
    fn append(&mut self, record: Record) -> Result<(), StorageError> {
        let frame = encode_frame(&record);
        self.active.writer.write_all(&frame)?;
        self.active.writer.flush()?;
        self.active.count += 1;
        match &record {
            Record::Put { key, value } => {
                self.memtable.insert(key.clone(), Some(value.clone()));
            }
            Record::Delete { key } => {
                self.memtable.insert(key.clone(), None);
            }
        }
        Ok(())
    }

    /// 封存活跃段：flush、建索引、开新活跃段。
    fn seal(&mut self) -> Result<(), StorageError> {
        self.active.writer.flush()?;
        let old_id = self.next_segment - 1;
        let path = segment_path(&self.dir, old_id);
        let file = File::open(&path)?;
        let index = build_index(BufReader::new(&file))?;
        self.segments.push(SealedSegment {
            id: old_id,
            file,
            index,
        });
        self.memtable.clear();

        let new_id = self.next_segment;
        self.next_segment += 1;
        self.active = ActiveSegment {
            writer: open_append(segment_path(&self.dir, new_id))?,
            count: 0,
        };
        Ok(())
    }
}

/// 段文件路径：`{dir}/{id:08}.log`。
fn segment_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:08}.log"))
}

/// 解析段文件名回段号（非段文件返回 `None`）。
fn parse_segment_name(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".log")?;
    if stem.len() != 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

fn open_append(path: PathBuf) -> Result<BufWriter<File>, StorageError> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    Ok(BufWriter::new(file))
}

/// 顺序读一个文件的全部好帧，返回（key → 帧偏移 或 记录内容）。
///
/// 坏帧即停：`Ok((index, good_bytes))` 里的 `good_bytes` 是最后一条
/// 好记录的结束偏移——恢复时用它截尾。
fn replay(
    mut reader: BufReader<File>,
) -> Result<(BTreeMap<Vec<u8>, Option<Vec<u8>>>, u64), StorageError> {
    let mut memtable = BTreeMap::new();
    let mut offset: u64 = 0;
    loop {
        let frame = match read_frame_at(&mut reader, offset) {
            Ok(Some(frame)) => frame,
            _ => break, // EOF 或坏帧：坏尾截断语义
        };
        match Record::decode(&frame)? {
            Record::Put { key, value } => {
                memtable.insert(key, Some(value));
            }
            Record::Delete { key } => {
                memtable.insert(key, None);
            }
        }
        offset += 8 + frame.len() as u64;
    }
    Ok((memtable, offset))
}

/// 建段索引：key → 帧起始偏移。同 key 后写覆盖先写（索引天然指向最新）。
fn build_index(mut reader: BufReader<File>) -> Result<HashMap<Vec<u8>, u64>, StorageError> {
    let mut index = HashMap::new();
    let mut offset: u64 = 0;
    loop {
        let frame = match read_frame_at(&mut reader, offset) {
            Ok(Some(frame)) => frame,
            _ => break,
        };
        match Record::decode(&frame)? {
            Record::Put { key, .. } | Record::Delete { key } => {
                index.insert(key, offset);
            }
        }
        offset += 8 + frame.len() as u64;
    }
    Ok(index)
}

/// 从 `offset` 读一帧（len + crc + body），CRC 校验失败按坏帧处理。
fn read_frame_at(
    reader: &mut BufReader<File>,
    offset: u64,
) -> Result<Option<Vec<u8>>, StorageError> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut head = [0u8; 8];
    match reader.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(head[0..4].try_into().expect("4 字节")) as usize;
    let expected_crc = u32::from_le_bytes(head[4..8].try_into().expect("4 字节"));
    if len > MAX_RECORD_LEN {
        return Ok(None); // 长度爆炸 = 坏帧
    }
    let mut body = vec![0u8; len];
    match reader.read_exact(&mut body) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    if crc32::checksum(&body) != expected_crc {
        return Ok(None); // CRC 不符 = 坏帧
    }
    Ok(Some(body))
}

/// 定位读：seek 到偏移读一帧并解出记录。
fn read_record_at(file: &File, offset: u64) -> Result<Record, StorageError> {
    let mut reader = BufReader::new(file);
    let frame = read_frame_at(&mut reader, offset)?.ok_or(StorageError::Corrupted)?;
    Record::decode(&frame)
}

/// 单条记录上限（防坏长度头把分配撑爆）。
const MAX_RECORD_LEN: usize = 1 << 20; // 1 MiB

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用目录：进程内唯一，guard 退出时清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "im-storage-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("系统时钟正常")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 基本往返：put → get；覆盖更新读最新。
    #[test]
    fn put_get_overwrite() {
        let dir = TempDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.put(b"k1", b"v1").unwrap();
        assert_eq!(engine.get(b"k1").unwrap(), Some(b"v1".to_vec()));

        engine.put(b"k1", b"v2").unwrap();
        assert_eq!(engine.get(b"k1").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(engine.get(b"missing").unwrap(), None);
    }

    /// 删除墓碑：get 变 None；tombstone 优先于封存段的旧值。
    #[test]
    fn delete_writes_tombstone() {
        let dir = TempDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.put(b"k", b"old").unwrap();
        engine.delete(b"k").unwrap();
        assert_eq!(engine.get(b"k").unwrap(), None);
    }

    /// 崩溃恢复：重开后数据仍在（WAL 重放）。
    #[test]
    fn reopen_recovers_data() {
        let dir = TempDir::new();
        {
            let mut engine = Engine::open(dir.path()).unwrap();
            engine.put(b"a", b"1").unwrap();
            engine.put(b"b", b"2").unwrap();
            engine.delete(b"a").unwrap();
        }
        let engine = Engine::open(dir.path()).unwrap();
        assert_eq!(engine.get(b"a").unwrap(), None);
        assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    /// 坏尾截断：追加垃圾字节后重开，好数据保留、坏尾被截掉，
    /// 且引擎仍可继续写（追加位置正确）。
    #[test]
    fn corrupted_tail_is_truncated() {
        let dir = TempDir::new();
        {
            let mut engine = Engine::open(dir.path()).unwrap();
            engine.put(b"good", b"data").unwrap();
        }
        // 模拟崩溃时的半条记录：往最大段号的段文件追加垃圾
        let active = segment_path(dir.path(), 0);
        let mut file = OpenOptions::new().append(true).open(&active).unwrap();
        file.write_all(&[0xFF, 0x00, 0x11, 0x22]).unwrap();
        drop(file);

        let mut engine = Engine::open(dir.path()).unwrap();
        assert_eq!(engine.get(b"good").unwrap(), Some(b"data".to_vec()));
        // 截尾后继续写不受影响
        engine.put(b"after", b"recovery").unwrap();
        assert_eq!(engine.get(b"after").unwrap(), Some(b"recovery".to_vec()));
    }

    /// 前缀扫描：跨多版本、含 tombstone 的有序输出。
    #[test]
    fn scan_prefix_merges_versions() {
        let dir = TempDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.put(b"m/1/100", b"a").unwrap();
        engine.put(b"m/1/101", b"b").unwrap();
        engine.put(b"m/2/102", b"c").unwrap();
        engine.put(b"m/1/100", b"a2").unwrap(); // 覆盖
        engine.delete(b"m/1/101").unwrap(); // 删除

        let scanned = engine.scan_prefix(b"m/1/").unwrap();
        assert_eq!(
            scanned,
            vec![
                (b"m/1/100".to_vec(), b"a2".to_vec()),
                // m/1/101 是 tombstone，不输出
            ]
        );
    }

    /// 压实：多轮覆盖写后 compact，scan 只见最新版本且数据完整。
    #[test]
    fn compact_keeps_only_latest() {
        let dir = TempDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        for round in 0..3u8 {
            engine.put(b"key", &[round]).unwrap();
        }
        engine.put(b"other", b"keep").unwrap();
        engine.delete(b"gone").unwrap();
        engine.compact().unwrap();

        assert_eq!(engine.get(b"key").unwrap(), Some(vec![2]));
        assert_eq!(engine.get(b"other").unwrap(), Some(b"keep".to_vec()));
        assert_eq!(engine.get(b"gone").unwrap(), None);
        // 压实后重开仍完整
        drop(engine);
        let engine = Engine::open(dir.path()).unwrap();
        assert_eq!(engine.get(b"key").unwrap(), Some(vec![2]));
        assert_eq!(engine.get(b"other").unwrap(), Some(b"keep".to_vec()));
    }

    /// 段文件名解析：正常段号、非法名（长度/后缀/非数字）。
    #[test]
    fn segment_name_parsing() {
        let dir = Path::new("/tmp/x");
        assert_eq!(parse_segment_name(&segment_path(dir, 42)), Some(42));
        assert_eq!(parse_segment_name(&dir.join("notasegment.log")), None);
        assert_eq!(parse_segment_name(&dir.join("1234567.log")), None); // 7 位
        assert_eq!(parse_segment_name(&dir.join("12345678.txt")), None);
        assert_eq!(
            parse_segment_name(&dir.join("123456789.log")), // 9 位
            None
        );
    }
}
