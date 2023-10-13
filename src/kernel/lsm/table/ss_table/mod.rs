use crate::kernel::io::{IoFactory, IoReader, IoType};
use crate::kernel::lsm::iterator::Iter;
use crate::kernel::lsm::mem_table::KeyValue;
use crate::kernel::lsm::storage::Config;
use crate::kernel::lsm::table::ss_table::block::{
    Block, BlockBuilder, BlockCache, BlockItem, BlockOptions, BlockType, CompressType, Index,
    MetaBlock, Value,
};
use crate::kernel::lsm::table::ss_table::footer::{Footer, TABLE_FOOTER_SIZE};
use crate::kernel::lsm::table::ss_table::iter::SSTableIter;
use crate::kernel::lsm::table::Table;
use crate::kernel::KernelResult;
use crate::KernelError;
use bytes::Bytes;
use growable_bloom_filter::GrowableBloom;
use itertools::Itertools;
use parking_lot::Mutex;
use std::io::SeekFrom;
use std::sync::Arc;
use tracing::info;

pub(crate) mod block;
pub(crate) mod block_iter;
mod footer;
pub(crate) mod iter;

/// SSTable
///
/// SSTable仅加载MetaBlock与Footer，避免大量冷数据时冗余的SSTable加载的空间占用
pub(crate) struct SSTable {
    // 表索引信息
    footer: Footer,
    // 文件IO操作器
    reader: Mutex<Box<dyn IoReader>>,
    // 该SSTable的唯一编号(时间递增)
    gen: i64,
    // 统计信息存储Block
    meta: MetaBlock,
    // Block缓存(Index/Value)
    cache: Arc<BlockCache>,
}

impl SSTable {
    pub(crate) fn new(
        io_factory: &IoFactory,
        config: &Config,
        cache: Arc<BlockCache>,
        gen: i64,
        vec_data: Vec<KeyValue>,
        level: usize,
        io_type: IoType,
    ) -> KernelResult<SSTable> {
        let len = vec_data.len();
        let data_restart_interval = config.data_restart_interval;
        let index_restart_interval = config.index_restart_interval;
        let mut filter = GrowableBloom::new(config.desired_error_prob, len);

        let mut builder = BlockBuilder::new(
            BlockOptions::from(config)
                .compress_type(CompressType::LZ4)
                .data_restart_interval(data_restart_interval)
                .index_restart_interval(index_restart_interval),
        );
        for data in vec_data {
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
            size_of_disk: (data_bytes.len()
                + index_bytes.len()
                + meta_bytes.len()
                + TABLE_FOOTER_SIZE) as u32,
        };
        let mut writer = io_factory.writer(gen, io_type)?;
        writer.write_all(
            data_bytes
                .into_iter()
                .chain(index_bytes)
                .chain(meta_bytes)
                .chain(bincode::serialize(&footer)?)
                .collect_vec()
                .as_mut(),
        )?;
        writer.flush()?;
        info!("[SsTable: {}][create][MetaBlock]: {:?}", gen, meta);

        let reader = Mutex::new(io_factory.reader(gen, io_type)?);
        Ok(SSTable {
            footer,
            reader,
            gen,
            meta,
            cache,
        })
    }

    /// 通过已经存在的文件构建SSTable
    ///
    /// 使用原有的路径与分区大小恢复出一个有内容的SSTable
    pub(crate) fn load_from_file(
        mut reader: Box<dyn IoReader>,
        cache: Arc<BlockCache>,
    ) -> KernelResult<Self> {
        let gen = reader.get_gen();
        let footer = Footer::read_to_file(reader.as_mut())?;
        let Footer {
            size_of_disk,
            meta_offset,
            meta_len,
            ..
        } = &footer;
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
            footer,
            gen,
            reader,
            meta,
            cache,
        })
    }

    pub(crate) fn data_block(&self, index: Index) -> KernelResult<BlockType> {
        Ok(BlockType::Data(Self::loading_block(
            self.reader.lock().as_mut(),
            index.offset(),
            index.len(),
            CompressType::LZ4,
            self.meta.data_restart_interval,
        )?))
    }

    pub(crate) fn index_block(&self) -> KernelResult<&Block<Index>> {
        self.cache
            .get_or_insert((self.gen(), None), |_| {
                let Footer {
                    index_offset,
                    index_len,
                    ..
                } = self.footer;
                Ok(BlockType::Index(Self::loading_block(
                    self.reader.lock().as_mut(),
                    index_offset,
                    index_len as usize,
                    CompressType::None,
                    self.meta.index_restart_interval,
                )?))
            })
            .map(|block_type| match block_type {
                BlockType::Index(data_block) => Some(data_block),
                _ => None,
            })?
            .ok_or(KernelError::DataEmpty)
    }

    fn loading_block<T>(
        reader: &mut dyn IoReader,
        offset: u32,
        len: usize,
        compress_type: CompressType,
        restart_interval: usize,
    ) -> KernelResult<Block<T>>
    where
        T: BlockItem,
    {
        let mut buf = vec![0; len];
        let _ = reader.seek(SeekFrom::Start(offset as u64))?;
        reader.read_exact(&mut buf)?;

        Block::decode(buf, compress_type, restart_interval)
    }
}

impl Table for SSTable {
    fn query(&self, key: &[u8]) -> KernelResult<Option<KeyValue>> {
        if self.meta.filter.contains(key) {
            let index_block = self.index_block()?;

            if let BlockType::Data(data_block) = self.cache.get_or_insert(
                (self.gen(), Some(index_block.find_with_upper(key))),
                |(_, index)| {
                    let index = (*index).ok_or_else(|| KernelError::DataEmpty)?;
                    Ok(Self::data_block(self, index)?)
                },
            )? {
                if let (value, true) = data_block.find(key) {
                    return Ok(Some((Bytes::copy_from_slice(key), value)));
                }
            }
        }

        Ok(None)
    }

    fn len(&self) -> usize {
        self.meta.len
    }

    fn size_of_disk(&self) -> u64 {
        self.footer.size_of_disk as u64
    }

    fn gen(&self) -> i64 {
        self.gen
    }

    fn level(&self) -> usize {
        self.footer.level as usize
    }

    fn iter<'a>(&'a self) -> KernelResult<Box<dyn Iter<'a, Item = KeyValue> + 'a + Send + Sync>> {
        Ok(SSTableIter::new(self).map(Box::new)?)
    }
}

#[cfg(test)]
mod tests {
    use crate::kernel::io::{FileExtension, IoFactory, IoType};
    use crate::kernel::lsm::log::LogLoader;
    use crate::kernel::lsm::mem_table::DEFAULT_WAL_PATH;
    use crate::kernel::lsm::storage::Config;
    use crate::kernel::lsm::table::loader::TableLoader;
    use crate::kernel::lsm::table::ss_table::SSTable;
    use crate::kernel::lsm::table::{Table, TableType};
    use crate::kernel::lsm::version::DEFAULT_SS_TABLE_PATH;
    use crate::kernel::utils::lru_cache::ShardingLruCache;
    use crate::kernel::KernelResult;
    use bincode::Options;
    use bytes::Bytes;
    use std::collections::hash_map::RandomState;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_ss_table() -> KernelResult<()> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");

        let value = Bytes::copy_from_slice(
            b"If you shed tears when you miss the sun, you also miss the stars.",
        );
        let config = Config::new(temp_dir.into_path());
        let sst_factory = Arc::new(IoFactory::new(
            config.dir_path.join(DEFAULT_SS_TABLE_PATH),
            FileExtension::SSTable,
        )?);
        let (log_loader, _, _) = LogLoader::reload(
            config.path(),
            (DEFAULT_WAL_PATH, Some(1)),
            IoType::Buf,
            |_| Ok(()),
        )?;
        let sst_loader = TableLoader::new(config.clone(), sst_factory.clone(), log_loader)?;

        let mut vec_data = Vec::new();
        let times = 2333;

        for i in 0..times {
            vec_data.push((
                Bytes::from(bincode::options().with_big_endian().serialize(&i)?),
                Some(value.clone()),
            ));
        }
        // Tips: 此处Level需要为0以上，因为Level 0默认为Mem类型，容易丢失
        let _ = sst_loader.create(1, vec_data.clone(), 1, TableType::SortedString)?;
        assert!(sst_loader.is_table_file_exist(1)?);

        let ss_table = sst_loader.get(1).unwrap();

        for i in 0..times {
            assert_eq!(
                ss_table.query(&vec_data[i].0)?.unwrap().1,
                Some(value.clone())
            )
        }
        let cache = ShardingLruCache::new(config.table_cache_size, 16, RandomState::default())?;
        let ss_table =
            SSTable::load_from_file(sst_factory.reader(1, IoType::Direct)?, Arc::new(cache))?;
        for i in 0..times {
            assert_eq!(
                ss_table.query(&vec_data[i].0)?.unwrap().1,
                Some(value.clone())
            )
        }

        Ok(())
    }
}
