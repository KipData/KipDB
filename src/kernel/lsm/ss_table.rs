use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use bytes::Bytes;
use growable_bloom_filter::GrowableBloom;
use itertools::Itertools;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::info;
use crate::kernel::io::{IoFactory, IoReader, IoType};
use crate::kernel::lsm::{MetaBlock, Footer, TABLE_FOOTER_SIZE};
use crate::kernel::lsm::block::{Block, BlockBuilder, BlockCache, BlockItem, BlockOptions, BlockType, CompressType, Index, Value};
use crate::kernel::lsm::lsm_kv::Config;
use crate::kernel::lsm::mem_table::KeyValue;
use crate::kernel::lsm::version::Version;
use crate::kernel::Result;
use crate::KernelError;

pub(crate) struct SSTable {
    inner: Arc<SSTableInner>
}

impl Clone for SSTable {
    fn clone(&self) -> Self {
        SSTable {
            inner: Arc::clone(&self.inner)
        }
    }
}

unsafe impl Send for SSTable {}

/// SSTable
///
/// SSTable仅加载MetaBlock与Footer，避免大量冷数据时冗余的SSTable加载的空间占用
pub(crate) struct SSTableInner {
    // 表索引信息
    footer: Footer,
    // 文件IO操作器
    reader: Mutex<Box<dyn IoReader>>,
    // 该SSTable的唯一编号(时间递增)
    gen: i64,
    // 统计信息存储Block
    meta: MetaBlock,
}

/// 数据范围索引
/// 用于缓存SSTable中所有数据的第一个和最后一个数据的Key
/// 标明数据的范围以做到快速区域定位
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    pub(crate) start: Bytes,
    pub(crate) end: Bytes,
    gen: i64,
}

impl Scope {

    pub(crate) fn get_gen(&self) -> i64 {
        self.gen
    }

    /// 由KeyValue组成的Key构成scope
    pub(crate) fn from_data(gen: i64, first: &KeyValue, last: &KeyValue) -> Self {
        Scope {
            start: first.0.clone(),
            end: last.0.clone(),
            gen,
        }
    }

    /// 将多个scope重组融合成一个scope
    pub(crate) fn fusion(scopes: &[Scope]) -> Result<Self> {
        if !scopes.is_empty() {
            let start = scopes.iter()
                .map(|scope| &scope.start)
                .min()
                .ok_or(KernelError::DataEmpty)?
                .clone();
            let end = scopes.iter()
                .map(|scope| &scope.end)
                .max()
                .ok_or(KernelError::DataEmpty)?
                .clone();

            Ok(Scope { start, end, gen: 0 })
        } else {
            Err(KernelError::DataEmpty)
        }
    }

    /// 判断scope之间是否相交
    pub(crate) fn meet(&self, target: &Scope) -> bool {
        (self.start.le(&target.start) && self.end.ge(&target.start))
            || (self.start.le(&target.end) && self.end.ge(&target.end))
    }

    /// 判断key与Scope是否相交
    pub(crate) fn meet_with_key(&self, key: &[u8]) -> bool {
        self.start.as_ref().le(key)
            && self.end.as_ref().ge(key)
    }

    /// 由一组KeyValue组成一个scope
    #[allow(clippy::pattern_type_mismatch)]
    pub(crate) fn from_vec_data(gen: i64, vec_mem_data: &Vec<KeyValue>) -> Result<Self> {
        match vec_mem_data.as_slice() {
            [first, .., last] => {
                Ok(Self::from_data(gen, first, last))
            },
            [one] => {
                Ok(Self::from_data(gen, one, one))
            },
            _ => {
                Err(KernelError::DataEmpty)
            },
        }
    }
}

impl SSTable {
    #[allow(dead_code)]
    pub(crate) fn get_level(&self) -> usize {
        self.inner.footer.level as usize
    }

    pub(crate) fn get_gen(&self) -> i64 {
        self.inner.gen
    }

    pub(crate) fn get_size_of_disk(&self) -> u64 {
        self.inner.footer.size_of_disk as u64
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.meta.len
    }

    /// 通过已经存在的文件构建SSTable
    ///
    /// 使用原有的路径与分区大小恢复出一个有内容的SSTable
    pub(crate) fn load_from_file(mut reader: Box<dyn IoReader>) -> Result<Self>{
        let gen = reader.get_gen();
        let footer = Footer::read_to_file(reader.as_mut())?;
        let Footer { size_of_disk, meta_offset, meta_len ,.. } = &footer;
        info!(
            "[SsTable: {gen}][load_from_file][MetaBlock]: {footer:?}, Size of Disk: {}, IO Type: {:?}",
            size_of_disk ,
            reader.get_type()
        );

        let mut buf = vec![0; *meta_len as usize];
        let _ = reader.seek(SeekFrom::Start(*meta_offset as u64))?;
        let _ = reader.read(&mut buf)?;

        let meta = bincode::deserialize(&buf)?;
        let reader = Mutex::new(reader);
        Ok(SSTable {
            inner : Arc::new(
                SSTableInner { footer, gen, reader, meta, }
            )
        })
    }

    /// 查询Key对应的Value
    pub(crate) fn query_with_key(
        &self,
        key: &[u8],
        block_cache: &BlockCache
    ) -> Result<Option<Bytes>> {
        let inner = &self.inner;
        if inner.meta.filter.contains(key) {
            let index_block = self.get_index_block(block_cache)?;

            if let BlockType::Data(data_block) =  block_cache.get_or_insert(
                (self.get_gen(), Some(index_block.find_with_upper(key))),
                |(_, index)| {
                    let index = (*index).ok_or_else(|| KernelError::DataEmpty)?;
                    Ok(Self::get_data_block_(inner, index)?)
                }
            )? { return Ok(data_block.find(key)); }
        }

        Ok(None)
    }

    pub(crate) fn get_data_block<'a>(&'a self, index: Index, block_cache: &'a BlockCache) -> Result<Option<&Block<Value>>> {
        let inner = &self.inner;
        Ok(block_cache.get_or_insert(
            (self.get_gen(), Some(index)),
            |(_, index)| {
                let index = (*index).ok_or_else(|| KernelError::DataEmpty)?;
                Ok(Self::get_data_block_(inner, index)?)
            }
        ).map(|block_type| {
            match block_type {
                BlockType::Data(data_block) => Some(data_block),
                _ => None
            }
        })?)
    }

    fn get_data_block_(inner: &SSTableInner, index: Index) -> Result<BlockType> {
        Ok(BlockType::Data(
            Self::loading_block(
                inner.reader.lock().as_mut(),
                index.offset(),
                index.len(),
                CompressType::LZ4,
                inner.meta.data_restart_interval
            )?
        ))
    }

    pub(crate) fn get_index_block<'a>(&'a self, block_cache: &'a BlockCache) -> Result<&Block<Index>> {
        let inner = &self.inner;
        block_cache.get_or_insert(
            (self.get_gen(), None),
            |_| Ok(Self::get_index_block_(inner)?)
        ).map(|block_type| {
            match block_type {
                BlockType::Index(data_block) => Some(data_block),
                _ => None
            }
        })?.ok_or(KernelError::DataEmpty)
    }

    fn get_index_block_(inner: &Arc<SSTableInner>) -> Result<BlockType> {
        let Footer { index_offset, index_len, .. } = inner.footer;
        Ok(BlockType::Index(
            Self::loading_block(
                inner.reader.lock().as_mut(),
                index_offset,
                index_len as usize,
                CompressType::None,
                inner.meta.index_restart_interval
            )?
        ))
    }

    fn loading_block<T>(
        reader: &mut dyn IoReader,
        offset: u32,
        len: usize,
        compress_type: CompressType,
        restart_interval: usize,
    ) -> Result<Block<T>>
        where T: BlockItem
    {
        let mut buf = vec![0; len];
        let _ = reader.seek(SeekFrom::Start(offset as u64))?;
        reader.read_exact(&mut buf)?;

        Block::decode(buf, compress_type, restart_interval)
    }

    /// 通过一组SSTable收集对应的Gen
    pub(crate) fn collect_gen(vec_ss_table: &[SSTable]) -> Result<Vec<i64>> {
        Ok(vec_ss_table.iter()
            .map(SSTable::get_gen)
            .unique()
            .collect())
    }

    /// 获取指定SSTable索引位置
    pub(crate) fn find_index_with_level(option_first: Option<i64>, version: &Version, level: usize) -> usize {
        option_first.and_then(|gen| version.get_index(level, gen))
            .unwrap_or(0)
    }

    /// 通过内存表构建持久化并构建SSTable
    /// 使用目标路径与文件大小，分块大小构建一个有内容的SSTable
    pub(crate) fn create_for_mem_table(
        config: &Config,
        gen: i64,
        io_factory: &IoFactory,
        vec_mem_data: Vec<KeyValue>,
        level: usize,
        io_type: IoType
    ) -> Result<(SSTable, Scope)>{
        // 获取数据的Key涵盖范围
        let scope = Scope::from_vec_data(gen, &vec_mem_data)?;
        let len = vec_mem_data.len();
        let data_restart_interval = config.data_restart_interval;
        let index_restart_interval = config.index_restart_interval;
        let mut filter = GrowableBloom::new(config.desired_error_prob, len);

        let mut builder = BlockBuilder::new(
            BlockOptions::from(config)
                .compress_type(CompressType::LZ4)
                .data_restart_interval(data_restart_interval)
                .index_restart_interval(index_restart_interval)
        );
        for data in vec_mem_data {
            let (key, value) = data;
            let _ = filter.insert(&key);
            builder.add((key, Value::from(value)));
        }
        let meta = MetaBlock {
            filter,
            len,
            index_restart_interval,
            data_restart_interval,
        };

        let (data_bytes, index_bytes) = builder.build()?;
        let meta_bytes = bincode::serialize(&meta)?;
        let footer = Footer {
            level: level as u8,
            index_offset: data_bytes.len() as u32,
            index_len: index_bytes.len() as u32,
            meta_offset: (data_bytes.len() + index_bytes.len()) as u32,
            meta_len: meta_bytes.len() as u32,
            size_of_disk: (data_bytes.len() + index_bytes.len() + meta_bytes.len() + TABLE_FOOTER_SIZE) as u32,
        };
        let mut writer = io_factory.writer(gen, io_type)?;
        let _ = writer.write(
            data_bytes.into_iter()
                .chain(index_bytes)
                .chain(meta_bytes)
                .chain(bincode::serialize(&footer)?)
                .collect_vec()
                .as_mut()
        )?;
        writer.flush()?;
        info!("[SsTable: {}][create_form_index][MetaBlock]: {:?}", gen, meta);

        let reader = Mutex::new(io_factory.reader(gen, io_type)?);
        Ok((SSTable {
            inner: Arc::new(
                SSTableInner {
                    footer,
                    reader,
                    gen,
                    meta,
                }
            )
        }, scope))

    }
}

#[cfg(test)]
mod tests {

    use std::collections::hash_map::RandomState;
    use bincode::Options;
    use bytes::Bytes;
    use tempfile::TempDir;
    use crate::kernel::io::{FileExtension, IoFactory, IoType};
    use crate::kernel::lsm::lsm_kv::Config;
    use crate::kernel::lsm::ss_table::SSTable;
    use crate::kernel::lsm::version::DEFAULT_SS_TABLE_PATH;
    use crate::kernel::Result;
    use crate::kernel::utils::lru_cache::ShardingLruCache;

    #[test]
    fn test_ss_table() -> Result<()> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");

        let value = Bytes::copy_from_slice(b"If you shed tears when you miss the sun, you also miss the stars.");
        let config = Config::new(temp_dir.into_path());
        let sst_factory = IoFactory::new(
            config.dir_path.join(DEFAULT_SS_TABLE_PATH),
            FileExtension::SSTable
        )?;
        let cache = ShardingLruCache::new(
            config.block_cache_size,
            16,
            RandomState::default()
        )?;
        let mut vec_data = Vec::new();
        let times = 2333;

        for i in 0..times {
            vec_data.push(
                (Bytes::from(bincode::options().with_big_endian().serialize(&i)?), Some(value.clone()))
            );
        }
        let (ss_table, _) = SSTable::create_for_mem_table(
            &config,
            1,
            &sst_factory,
            vec_data.clone(),
            0,
            IoType::Direct
        )?;
        for i in 0..times {
            assert_eq!(ss_table.query_with_key(&vec_data[i].0, &cache)?, Some(value.clone()))
        }
        drop(ss_table);
        let ss_table = SSTable::load_from_file(
            sst_factory.reader(1, IoType::Direct)?
        )?;
        for i in 0..times {
            assert_eq!(ss_table.query_with_key(&vec_data[i].0, &cache)?, Some(value.clone()))
        }

        Ok(())
    }
}