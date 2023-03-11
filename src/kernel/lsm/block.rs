use std::cmp::min;
use std::io::{Cursor, Read, Write};
use std::mem;
use bytes::{Buf, BufMut};
use itertools::Itertools;
use lz4::Decoder;
use varuint::{ReadVarint, WriteVarint};
use crate::kernel::{CommandData, Result};
use crate::kernel::lsm::lsm_kv::Config;
use crate::kernel::utils::lru_cache::ShardingLruCache;
use crate::KvsError;

/// BlockCache类型 可同时缓存两种类型
///
/// Key为SSTable的gen且Index为None时返回Index类型
///
/// Key为SSTable的gen且Index为Some时返回Data类型
#[allow(dead_code)]
pub(crate) type BlockCache = ShardingLruCache<(i64, Option<Index>), BlockType>;

pub(crate) const DEFAULT_BLOCK_SIZE: usize = 4 * 1024;

/// 不动态决定Restart是因为Restart的范围固定可以做到更简单的Entry二分查询，提高性能
pub(crate) const DEFAULT_DATA_RESTART_INTERVAL: usize = 16;

pub(crate) const DEFAULT_INDEX_RESTART_INTERVAL: usize = 2;

const CRC_SIZE: usize = 4;

pub(crate) type KeyValue<T> = (Vec<u8>, T);

pub(crate) enum BlockType {
    Data(Block<Value>),
    Index(Block<Index>),
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct Entry<T> {
    unshared_len: usize,
    shared_len: usize,
    key: Vec<u8>,
    item: T
}

impl<T> Entry<T> where T: BlockItem {
    pub(crate) fn new(
        shared_len: usize,
        unshared_len: usize,
        key: Vec<u8>,
        item: T
    ) -> Self {
        Entry {
            unshared_len,
            shared_len,
            key,
            item,
        }
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        let _ignore = buf.write_varint(self.unshared_len as u32)?;
        let _ignore1 = buf.write_varint(self.shared_len as u32)?;
        let _ignore2 = buf.write(&self.key)?;
        let _ignore3 = buf.write(&self.item.encode()?);

        Ok(buf)
    }

    fn decode_with_cursor(cursor: &mut Cursor<Vec<u8>>) -> Result<Vec<(usize, Self)>> {
        let mut vec_entry = Vec::new();
        let mut index = 0;

        while !cursor.is_empty() {
            let unshared_len = ReadVarint::<u32>::read_varint(cursor)? as usize;
            let shared_len = ReadVarint::<u32>::read_varint(cursor)? as usize;

            let mut key = vec![0u8; unshared_len];
            let _ = cursor.read(&mut key)?;

            let item = T::decode(cursor)?;

            vec_entry.push((index, Self {
                unshared_len,
                shared_len,
                key,
                item,
            }));
            index += 1;
        }

        Ok(vec_entry)
    }
}

/// 键值对对应的Value
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Value {
    value_len: usize,
    bytes: Option<Vec<u8>>
}

impl From<Option<Vec<u8>>> for Value {
    fn from(bytes: Option<Vec<u8>>) -> Self {
        let value_len = bytes.as_ref()
            .map_or(0, Vec::len);
        Value {
            value_len,
            bytes,
        }
    }
}

/// Block索引
#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub(crate) struct Index {
    offset: u32,
    len: usize,
}

impl Index {
    fn new(offset: u32, len: usize) -> Self {
        Index { offset, len, }
    }

    pub(crate) fn offset(&self) -> u32 {
        self.offset
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

pub(crate) trait BlockItem: Sized + Clone {
    /// 由于需要直接连续序列化，因此使用Read进行Bytes读取
    fn decode<T>(reader: &mut T) -> Result<Self> where T: Read;

    fn encode(&self) -> Result<Vec<u8>>;
}

impl BlockItem for Value {
    fn decode<T>(mut reader: &mut T) -> Result<Self> where T: Read {
        let value_len = ReadVarint::<u32>::read_varint(&mut reader)? as usize;

        let bytes = (value_len > 0)
            .then(|| {
                let mut value = vec![0u8; value_len];
                reader.read(&mut value).ok()
                    .map(|_| value)
            })
            .flatten();

        Ok(Value {
            value_len,
            bytes,
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let _ = buf.write_varint(self.value_len as u32)?;
        if let Some(value) = &self.bytes {
            let _ = buf.write(value)?;
        }
        Ok(buf)
    }
}

impl BlockItem for Index {
    fn decode<T>(mut reader: &mut T) -> Result<Self> where T: Read {
        let offset = ReadVarint::<u32>::read_varint(&mut reader)?;
        let len = ReadVarint::<u32>::read_varint(&mut reader)? as usize;

        Ok(Index {
            offset,
            len,
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let _ = buf.write_varint(self.offset)?;
        let _ = buf.write_varint(self.len as u32)?;

        Ok(buf)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CompressType {
    None,
    LZ4
}

/// Block SSTable最小的存储单位
///
/// 分为DataBlock和IndexBlock
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Block<T> {
    restart_interval: usize,
    vec_entry: Vec<(usize, Entry<T>)>,
}

#[derive(Clone)]
pub(crate) struct BlockOptions {
    block_size: usize,
    compress_type: CompressType,
    data_restart_interval: usize,
    index_restart_interval: usize,
}

impl From<&Config> for BlockOptions {
    fn from(config: &Config) -> Self {
        BlockOptions {
            block_size: config.block_size,
            compress_type: CompressType::None,
            data_restart_interval: config.data_restart_interval,
            index_restart_interval: config.index_restart_interval,
        }
    }
}

impl BlockOptions {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        BlockOptions {
            block_size: DEFAULT_BLOCK_SIZE,
            compress_type: CompressType::None,
            data_restart_interval: DEFAULT_DATA_RESTART_INTERVAL,
            index_restart_interval: DEFAULT_INDEX_RESTART_INTERVAL,
        }
    }
    #[allow(dead_code)]
    pub(crate) fn block_size(mut  self, block_size: usize) -> Self {
        self.block_size = block_size;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn compress_type(mut  self, compress_type: CompressType) -> Self {
        self.compress_type = compress_type;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn data_restart_interval(mut self, data_restart_interval: usize) -> Self {
        self.data_restart_interval = data_restart_interval;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn index_restart_interval(mut self, index_restart_interval: usize) -> Self {
        self.index_restart_interval = index_restart_interval;
        self
    }
}

struct BlockBuf {
    bytes_size: usize,
    vec_key_value: Vec<KeyValue<Value>>,
}

impl BlockBuf {
    fn new() -> Self {
        BlockBuf {
            bytes_size: 0,
            vec_key_value: Vec::new(),
        }
    }

    fn add(&mut self, key_value: KeyValue<Value>) {
        // 断言新插入的键值对的Key大于buf中最后的key
        if let Some(last_key) = self.last_key() {
            assert!(key_value.0.cmp(last_key).is_gt());
        }
        self.bytes_size += key_value_bytes_len(&key_value);
        self.vec_key_value.push(key_value);
    }

    /// 获取最后一个Key
    fn last_key(&self) -> Option<&Vec<u8>> {
        self.vec_key_value
            .last()
            .map(|key_value| key_value.0.as_ref())
    }

    /// 刷新且弹出其缓存的键值对与其中last_key
    fn flush(&mut self) -> (Vec<KeyValue<Value>>, Option<Vec<u8>>) {
        self.bytes_size = 0;
        let last_key = self.last_key()
            .cloned();
        (mem::take(&mut self.vec_key_value), last_key)
    }
}

/// Block构建器
///
/// 请注意add时
pub(crate) struct BlockBuilder {
    options: BlockOptions,
    len: usize,
    buf: BlockBuf,
    vec_block: Vec<(Block<Value>, Vec<u8>)>
}

impl From<CommandData> for Option<KeyValue<Value>> {
    #[inline]
    fn from(value: CommandData) -> Self {
        match value {
            CommandData::Set { key, value } => {
                Some((key,Value::from(Some(Vec::clone(&value)))))
            },
            CommandData::Remove { key } => {
                Some((key, Value::from(None)))
            },
            CommandData::Get { .. } => None,
        }
    }
}

impl From<KeyValue<Value>> for CommandData {
    #[inline]
    fn from(key_value: KeyValue<Value>) -> Self {
        let (key, value) = key_value;
        if let Some(bytes) = value.bytes {
            CommandData::set(key, bytes)
        } else {
            CommandData::remove(key)
        }
    }
}

/// 获取键值对得到其空间占用数
fn key_value_bytes_len(key_value: &KeyValue<Value>) -> usize {
    let (key, value) = key_value;
    key.len() + value.bytes.as_ref()
        .map_or(0, Vec::len)
}

impl BlockBuilder {
    pub(crate) fn new(options: BlockOptions) -> Self {
        BlockBuilder {
            options,
            len: 0,
            buf: BlockBuf::new(),
            vec_block: Vec::new(),
        }
    }

    /// 查看已参与构建的键值对数量
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// 插入需要构建为Block的键值对
    ///
    /// 请注意add的键值对需要自行保证key顺序插入,否则可能会出现问题
    pub(crate) fn add<T>(&mut self, into_key_value: T)
        where T: Into<Option<KeyValue<Value>>>
    {
        if let Some(key_value) = into_key_value.into() {
            self.buf.add(key_value);
            self.len += 1;
        }
        // 超过指定的Block大小后进行Block构建(默认为4K大小)
        if self.is_out_of_byte() {
            self.build_();
        }
    }

    fn is_out_of_byte(&self) -> bool {
        self.buf.bytes_size >= self.options.block_size
    }

    /// 封装用的构建Block方法
    ///
    /// 刷新buf获取其中的所有键值对与其中最大的key进行前缀压缩构建为Block
    fn build_(&mut self) {
        if let (vec_kv, Some(last_key)) = self.buf.flush() {
            self.vec_block.push(
                (Block::new(vec_kv, self.options.data_restart_interval), last_key)
            );
        }
    }

    /// 构建多个Block连续序列化组合成的两个Bytes 前者为多个DataBlock，后者为单个IndexBlock
    pub(crate) fn build(mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        self.build_();

        let mut offset = 0;
        let mut vec_index = Vec::with_capacity(
            self.vec_block.len()
        );

        let blocks_bytes = self.vec_block
            .into_iter()
            .flat_map(|(block, last_key)| {
                block.encode(self.options.compress_type)
                    .map(|block_bytes| {
                        let len = block_bytes.len();
                        vec_index.push(
                            (last_key, Index::new(offset, len))
                        );
                        offset += len as u32;
                        block_bytes
                    })
            })
            .flatten()
            .collect_vec();

        let indexes_bytes = Block::new(vec_index, self.options.index_restart_interval)
            .encode(CompressType::None)?;

        Ok((blocks_bytes, indexes_bytes))
    }
}

impl Block<Value> {
    /// 通过Key查询对应Value
    pub(crate) fn find(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.binary_search(key)
            .ok()
            .and_then(|index| {
                self.vec_entry[index].1.item
                    .bytes.clone()
            })
    }
}

impl<T> Block<T> where T: BlockItem {
    /// 新建Block，同时Block会进行前缀压缩
    pub(crate) fn new(vec_kv: Vec<KeyValue<T>>, restart_interval: usize) -> Block<T> {
        let vec_sharding_len = sharding_shared_len(&vec_kv, restart_interval);
        let vec_entry = vec_kv.into_iter()
            .enumerate()
            .map(|(index, (key, item))| {
                let shared_len = if index % restart_interval == 0 { 0 } else {
                    vec_sharding_len[index / restart_interval]
                };
                (index, Entry::new(
                    shared_len,
                    key.len() - shared_len,
                    key[shared_len..].into(),
                    item
                ))
            })
            .collect_vec();
        Block {
            restart_interval,
            vec_entry,
        }
    }

    pub(crate) fn all_entry(self) -> Result<Vec<KeyValue<T>>> {
        let restart_interval = self.restart_interval;
        let vec_shared_key = self.vec_entry.iter()
            .filter(|(i, _)| i % restart_interval == 0)
            .map(|(i, Entry { shared_len, .. })| {
                self.shared_key_prefix(*i, *shared_len).to_vec()
            })
            .collect_vec();
        Ok(self.vec_entry.into_iter()
            .map(|(i, Entry { key, item, .. })| {
                let full_key = if i % restart_interval == 0 { key } else {
                    vec_shared_key[i / restart_interval].iter()
                        .cloned()
                        .chain(key)
                        .collect_vec()
                };
                (full_key, item)
            })
            .collect_vec())
    }

    pub(crate) fn all_value(self) -> Vec<T> {
        self.vec_entry.into_iter()
            .map(|(_, entry)| entry.item)
            .collect_vec()
    }

    /// 查询相等或最近较大的Key
    pub(crate) fn find_with_upper(&self, key: &[u8]) -> T {
        let index = self.binary_search(key)
            .unwrap_or_else(|index| index);
        self.vec_entry[index].1
            .item.clone()
    }

    fn binary_search(&self, key: &[u8]) -> core::result::Result<usize, usize> {
        self.vec_entry
            .binary_search_by(|(index, entry)| {
                if entry.shared_len > 0 {
                    // 对有前缀压缩的Key进行前缀拼接
                    let shared_len = min(entry.shared_len, key.len());
                    key[0..shared_len]
                        .cmp(self.shared_key_prefix(*index, shared_len))
                        .then_with(|| key[shared_len..].cmp(&entry.key))
                } else {
                    key.cmp(&entry.key)
                }.reverse()
            })
    }

    /// 获取该Entry对应的shared_key前缀
    ///
    /// 具体原理是通过被固定的restart_interval进行前缀压缩的Block，
    /// 通过index获取前方最近的Restart，得到的Key通过shared_len进行截取以此得到shared_key
    fn shared_key_prefix(&self, index: usize, shared_len: usize) -> &[u8] {
        &self.vec_entry[index - index % self.restart_interval]
            .1.key[0..shared_len]
    }

    /// 序列化后进行压缩
    ///
    /// 可选LZ4与不压缩
    pub(crate) fn encode(&self, compress_type: CompressType) -> Result<Vec<u8>> {
        let buf = self.to_raw()?;
        Ok(match compress_type {
            CompressType::None => buf,
            CompressType::LZ4 => {
                let mut encoder = lz4::EncoderBuilder::new()
                    .level(4)
                    .build(Vec::with_capacity(buf.len()).writer())?;
                let _ = encoder.write(&buf[..])?;

                let (writer, result) = encoder.finish();
                result?;
                writer.into_inner()
            }
        })
    }

    /// 解压后反序列化
    ///
    /// 与encode对应，进行数据解压操作并反序列化为Block
    pub(crate) fn decode(buf: Vec<u8>, compress_type: CompressType) -> Result<Self> {
        let buf = match compress_type {
            CompressType::None => buf,
            CompressType::LZ4 => {
                let mut decoder = Decoder::new(buf.reader())?;
                let mut decoded = Vec::with_capacity(DEFAULT_BLOCK_SIZE);
                let _ = decoder.read_to_end(&mut decoded)?;
                decoded
            }
        };
        Self::from_raw(buf)
    }

    /// 读取Bytes进行Block的反序列化
    pub(crate) fn from_raw(mut buf: Vec<u8>) -> Result<Self> {
        let date_bytes_len = buf.len() - CRC_SIZE;
        if crc32fast::hash(&buf) == bincode::deserialize::<u32>(
            &buf[date_bytes_len..]
        )? {
            return Err(KvsError::CrcMisMatch)
        }
        buf.truncate(date_bytes_len);

        let mut cursor = Cursor::new(buf);
        let restart_interval = ReadVarint::<u32>::read_varint(&mut cursor)? as usize;
        let vec_entry = Entry::<T>::decode_with_cursor(&mut cursor)?;
        Ok(Self {
            restart_interval,
            vec_entry
        })
    }

    /// 序列化该Block
    ///
    /// 与from_raw对应，序列化时会生成crc_code用于反序列化时校验
    pub(crate) fn to_raw(&self) -> Result<Vec<u8>> {
        let mut bytes_block = Vec::with_capacity(DEFAULT_BLOCK_SIZE);

        let _ = bytes_block.write_varint(self.restart_interval as u32)?;
        bytes_block.append(
            &mut self.vec_entry
                .iter()
                .flat_map(|(_, entry)| entry.encode())
                .flatten()
                .collect_vec()
        );
        let check_crc = crc32fast::hash(&bytes_block);
        bytes_block.append(&mut bincode::serialize(&check_crc)?);

        Ok(bytes_block)
    }
}

/// 批量以restart_interval进行shared_len的获取
fn sharding_shared_len<T>(vec_kv: &Vec<KeyValue<T>>, restart_interval: usize) -> Vec<usize>
    where T: BlockItem
{

    let mut vec_shared_key = Vec::with_capacity(
        (vec_kv.len() + restart_interval - 1) / restart_interval
    );
    for (_, group) in &vec_kv.iter()
        .enumerate()
        .group_by(|(i, _)| i / restart_interval)
    {
        vec_shared_key.push(
            longest_shared_len(
                group.map(|(_, item)| item)
                    .collect_vec()
            )
        )
    }
    vec_shared_key
}

/// 查询一组KV的Key最长前缀计数
fn longest_shared_len<T>(sharding: Vec<&KeyValue<T>>) -> usize {
    if sharding.is_empty() {
        return 0
    }
    let mut min_len = usize::MAX;
    for kv in &sharding {
        min_len = min(min_len, kv.0.len());
    }
    let mut low = 0;
    let mut high = min_len;
    while low < high {
        let mid = (high - low + 1) / 2 + low;
        if is_common_prefix(&sharding, mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    return low;

    fn is_common_prefix<T>(sharding: &[&KeyValue<T>], len: usize) -> bool {
        let first = sharding[0];
        for kv in sharding.iter().skip(1) {
            for i in 0..len {
                if first.0[i] != kv.0[i] {
                    return false
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use bincode::Options;
    use itertools::Itertools;
    use crate::kernel::{CommandData, Result};
    use crate::kernel::lsm::block::{Block, BlockBuilder, BlockOptions, CompressType, Entry, Index, Value};
    use crate::kernel::utils::lru_cache::LruCache;

    #[test]
    fn test_entry_serialization() -> Result<()> {
        let entry1 = Entry::new(0, 1,vec![b'1'], Value::from(Some(vec![b'1'])));
        let entry2 = Entry::new(0, 1,vec![b'1'], Value::from(Some(vec![b'1'])));

        let bytes_vec_entry = entry1.encode()?
            .into_iter()
            .chain(entry2.encode()?)
            .collect_vec();

        let vec_entry = Entry::decode_with_cursor(&mut Cursor::new(bytes_vec_entry))?;

        assert_eq!(vec![(0, entry1), (1, entry2)], vec_entry);

        Ok(())
    }

    #[test]
    fn test_block() -> Result<()> {
        let value = b"Let life be beautiful like summer flowers";
        let mut vec_cmd = Vec::new();

        let times = 2333;
        let options = BlockOptions::new();
        let mut builder = BlockBuilder::new(options.clone());
        // 默认使用大端序进行序列化，保证顺序正确性
        for i in 0..times {
            let mut key = b"KipDB-".to_vec();
            key.append(
                &mut bincode::options().with_big_endian().serialize(&i)?
            );
            vec_cmd.push(
                CommandData::set(key, value.to_vec()
                )
            );
        }

        for cmd in vec_cmd.iter().cloned() {
            builder.add(cmd);
        }

        let block = builder.vec_block[0].0.clone();

        let (block_bytes, index_bytes) = builder.build()?;

        let index_block = Block::<Index>::decode(index_bytes, CompressType::None)?;

        let mut cache = LruCache::new(5)?;

        for i in 0..times {
            let key = vec_cmd[i].get_key();
            let data_block = cache.get_or_insert(
                index_block.find_with_upper(key),
                |index| {
                let &Index { offset, len } = index;
                let target_block = Block::<Value>::decode(
                    block_bytes[offset as usize..offset as usize + len].to_vec(),
                    options.compress_type
                )?;
                Ok(target_block)
            })?;
            assert_eq!(data_block.find(key), Some(value.to_vec()))
        }

        test_block_serialization_(block.clone(), CompressType::None)?;
        test_block_serialization_(block.clone(), CompressType::LZ4)?;

        Ok(())
    }

    fn test_block_serialization_(block: Block<Value>, compress_type: CompressType) -> Result<()> {
        let de_block = Block::decode(
            block.encode(compress_type)?, compress_type
        )?;
        assert_eq!(block, de_block);

        Ok(())
    }
}