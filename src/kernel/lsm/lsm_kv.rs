use std::collections::{BTreeMap};
use std::path::PathBuf;
use std::sync::{Arc};
use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{error, warn};
use crate::{HashStore, KvsError};
use crate::kernel::{CommandData, CommandPackage, KVStore, sorted_gen_list};
use crate::kernel::io_handler::IOHandlerFactory;
use crate::kernel::lsm::{Manifest, MemMap, MemTable};
use crate::kernel::lsm::compactor::Compactor;
use crate::kernel::lsm::ss_table::SsTable;
use crate::kernel::Result;

pub(crate) type LevelSlice = [Vec<u64>; 7];

pub(crate) type SsTableMap = BTreeMap<u64, SsTable>;

pub(crate) const DEFAULT_WAL_PATH: &str = "wal";

pub(crate) const DEFAULT_MINOR_THRESHOLD_WITH_DATA_SIZE: u64 = 1024;

pub(crate) const DEFAULT_PART_SIZE: u64 = 64;

pub(crate) const DEFAULT_SST_FILE_SIZE: usize = 2 * 1024 * 1024;

pub(crate) const DEFAULT_MAJOR_THRESHOLD_WITH_SST_SIZE: usize = 10;

pub(crate) const DEFAULT_WAL_COMPACTION_THRESHOLD: u64 = crate::kernel::hash_kv::DEFAULT_COMPACTION_THRESHOLD;

pub struct LsmStore {
    mem_table: MemTable,
    manifest: Arc<RwLock<Manifest>>,
    config: Arc<Config>,
    io_handler_factory: Arc<IOHandlerFactory>,
    /// WAL存储器
    ///
    /// SSTable持久化前会将gen写入
    /// 持久化成功后则会删除gen，以此作为是否成功的依据
    ///
    /// 使用HashStore作为wal的原因：
    /// 1、操作简易，不需要重新写一个WAL
    /// 2、作Key-Value分离的准备，当作vLog
    /// 3、HashStore会丢弃超出大小的数据，保证最新数据不会丢失
    wal: Arc<HashStore>,
}

#[async_trait]
impl KVStore for LsmStore {
    fn name() -> &'static str where Self: Sized {
        "LsmStore made in Kould"
    }

    async fn open(path: impl Into<PathBuf> + Send) -> Result<Self> {
        LsmStore::open_with_config(Config::new().dir_path(path.into())).await
    }

    async fn flush(&self) -> Result<()> {
        self.wal.flush().await?;

        Ok(())
    }

    async fn set(&self, key: &Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.append_cmd_data(CommandData::Set { key: key.clone(), value }).await
    }

    async fn get(&self, key: &Vec<u8>) -> Result<Option<Vec<u8>>> {
        let manifest = self.manifest.read().await;

        if let Some(cmd_data) = self.mem_table.get_cmd_data(key).await {
            return Ok(LsmStore::value_unpack_with_owner(cmd_data));
        }
        for (_, ss_table) in manifest.get_ss_table_map() {
            if let Some(cmd_data) = ss_table.query(key).await? {
                return Ok(LsmStore::value_unpack_with_owner(cmd_data));
            }
        }

        Ok(None)
    }

    async fn remove(&self, key: &Vec<u8>) -> Result<()> {
        match self.get(key).await? {
            Some(_) => { self.append_cmd_data(CommandData::Remove { key: key.clone() }).await }
            None => { Err(KvsError::KeyNotFound) }
        }
    }

    async fn shut_down(&self) -> Result<()> {
        self.wal.flush().await?;
        // 注意此处不使用let保存读锁
        // Compactor进行minor_compaction时需要使用到写锁
        if !self.mem_table.mem_table_is_empty().await {
            self.minor_compaction_sync().await?;
        }

        Ok(())
    }
}

impl LsmStore {

    /// 追加数据
    async fn append_cmd_data(&self, cmd: CommandData) -> Result<()> {
        let mem_table = &self.mem_table;
        let threshold_size = self.config.minor_threshold_with_data_size as usize;

        // Wal与MemTable双写
        let key = cmd.get_key();
        wal_put(&self.wal, key.clone(), CommandPackage::encode(&cmd)?);
        mem_table.insert_data(key.clone(), cmd).await;

        if mem_table.is_threshold_exceeded_minor(threshold_size).await {
            self.minor_compaction_sync().await?;
        }

        Ok(())
    }

    /// 使用Config进行LsmStore初始化
    pub async fn open_with_config(config: Config) -> Result<Self> where Self: Sized {
        let path = config.dir_path.clone();
        let wal_compaction_threshold = config.wal_compaction_threshold;

        let mut mem_map = MemMap::new();
        let mut ss_tables = BTreeMap::new();

        let mut wal_path = path.clone();
        wal_path.push(DEFAULT_WAL_PATH);

        // 初始化wal日志
        let wal = Arc::new(HashStore::open_with_compaction_threshold(&wal_path, wal_compaction_threshold).await?);
        let io_handler_factory = Arc::new(IOHandlerFactory::new(path.clone()));
        // 持久化数据恢复
        // 倒叙遍历，从最新的数据开始恢复
        for gen in sorted_gen_list(&path).await?.iter().rev() {
            let io_handler = io_handler_factory.create(*gen)?;
            // 尝试初始化Table
            match SsTable::restore_from_file(io_handler).await {
                Ok(ss_table) => {
                    // 初始化成功时直接传入SSTable的索引中
                    ss_tables.insert(*gen, ss_table);
                }
                Err(err) => {
                    error!("[LsmKVStore][Load SSTable][Error]: {:?}", err);
                    //是否删除可能还是得根据用户选择
                    // io_handler_factory.clean(*gen)?;
                    // 从wal将有问题的ss_table恢复到mem_table中
                    Self::reload_for_wal(&mut mem_map, &wal, *gen).await?;
                }
            }
        }
        // 构建SSTable信息集
        let manifest = Manifest::new(ss_tables, Arc::new(path.clone()));

        Ok(LsmStore {
            mem_table: MemTable::new(mem_map),
            manifest: Arc::new(RwLock::new(manifest)),
            config: Arc::new(config),
            io_handler_factory,
            wal,
        })
    }

    /// 从Wal恢复SSTable数据
    /// 初始化失败时遍历wal的key并检测key是否为gen
    async fn reload_for_wal(mem_table: &mut MemMap, wal: &HashStore, gen: u64) -> Result<()>{
        // 将SSTable持久化失败前预存入的指令键集合从wal中获取
        // 随后将每一条指令键对应的指令恢复到mem_table中
        warn!("[SsTable: {}][reload_from_wal]", gen);
        let key_gen = CommandCodec::encode_gen(gen)?;
        if let Some(key_cmd_u8) = wal.get(&key_gen).await? {
            for key in CommandCodec::decode_keys(&key_cmd_u8)? {
                if let Some(cmd_data_u8) = wal.get(&key).await? {
                    let cmd_data = CommandPackage::decode(&cmd_data_u8)?;

                    mem_table.insert(cmd_data.get_key_clone(), cmd_data);
                } else {
                    return Err(KvsError::WalLoadError);
                }
            };
        } else {
            return Err(KvsError::WalLoadError);
        }
        Ok(())
    }

    /// 异步持久化immutable_table为SSTable
    pub async fn minor_compaction(&self) -> Result<()> {
        let (keys, values) = self.mem_table.table_swap().await;
        let compactor = Compactor::from_lsm_kv(self);

        tokio::spawn(async move {
            if let Err(err) = compactor.minor_compaction(keys, values).await {
                error!("[LsmStore][minor_compaction][error happen]: {:?}", err);
            }
        });
        Ok(())
    }

    /// 同步持久化immutable_table为SSTable
    pub async fn minor_compaction_sync(&self) -> Result<()> {
        let (keys, values) = self.mem_table.table_swap().await;
        Ok(Compactor::from_lsm_kv(self).minor_compaction(keys, values).await?)
    }

    /// 同步进行SSTable基于Level的层级压缩
    pub async fn major_compaction_sync(&self, level: usize) -> Result<()> {
        Ok(Compactor::from_lsm_kv(self).major_compaction(level).await?)
    }

    /// 通过CommandData的引用解包并克隆出value值
    fn value_unpack(cmd_data: &CommandData) -> Option<Vec<u8>> {
        cmd_data.get_value_clone()
    }

    /// 通过CommandData的所有权直接返回value值的所有权
    fn value_unpack_with_owner(cmd_data: CommandData) -> Option<Vec<u8>> {
        cmd_data.get_value_owner()
    }
    pub(crate) fn manifest(&self) -> &Arc<RwLock<Manifest>> {
        &self.manifest
    }
    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }
    pub(crate) fn io_handler_factory(&self) -> &Arc<IOHandlerFactory> {
        &self.io_handler_factory
    }
    pub(crate) fn wal(&self) -> &Arc<HashStore> {
        &self.wal
    }
}

pub(crate) struct CommandCodec;

impl CommandCodec {
    pub(crate) fn encode_gen(gen: u64) -> Result<Vec<u8>> {
        Ok(bincode::serialize(&gen)?)
    }

    pub(crate) fn decode_gen(key: &Vec<u8>) -> Result<u64> {
        Ok(bincode::deserialize(key)?)
    }

    pub(crate) fn encode_keys(value: &Vec<Vec<u8>>) -> Result<Vec<u8>> {
        Ok(bincode::serialize(value)?)
    }

    pub(crate) fn decode_keys(vec_u8: &Vec<u8>) -> Result<Vec<Vec<u8>>> {
        Ok(bincode::deserialize(vec_u8)?)
    }
}

pub struct Config {
    // 数据目录地址
    pub(crate) dir_path: PathBuf,
    // WAL持久化阈值
    pub(crate) wal_compaction_threshold: u64,
    // 数据分块大小
    pub(crate) part_size: u64,
    // SSTable文件大小
    pub(crate) sst_file_size: usize,
    // 持久化阈值
    pub(crate) minor_threshold_with_data_size: u64,
    // Major压缩触发阈值
    pub(crate) major_threshold_with_sst_size: usize,
}

impl Config {

    pub fn dir_path(mut self, dir_path: PathBuf) -> Self {
        self.dir_path = dir_path;
        self
    }

    pub fn minor_threshold_with_data_size(mut self, minor_threshold_with_data_size: u64) -> Self {
        self.minor_threshold_with_data_size = minor_threshold_with_data_size;
        self
    }

    pub fn wal_compaction_threshold(mut self, wal_compaction_threshold: u64) -> Self {
        self.wal_compaction_threshold = wal_compaction_threshold;
        self
    }

    pub fn part_size(mut self, part_size: u64) -> Self {
        self.part_size = part_size;
        self
    }

    pub fn sst_file_size(mut self, sst_file_size: usize) -> Self {
        self.sst_file_size = sst_file_size;
        self
    }

    pub fn major_threshold_with_sst_size(mut self, major_threshold_with_sst_size: usize) -> Self {
        self.major_threshold_with_sst_size = major_threshold_with_sst_size;
        self
    }

    pub fn new() -> Self {
        Self {
            dir_path: DEFAULT_WAL_PATH.into(),
            minor_threshold_with_data_size: DEFAULT_MINOR_THRESHOLD_WITH_DATA_SIZE,
            wal_compaction_threshold: DEFAULT_WAL_COMPACTION_THRESHOLD,
            part_size: DEFAULT_PART_SIZE,
            sst_file_size: DEFAULT_SST_FILE_SIZE,
            major_threshold_with_sst_size: DEFAULT_MAJOR_THRESHOLD_WITH_SST_SIZE
        }
    }
}

/// 以Task类似的异步写数据，避免影响数据写入性能
/// 当然，LevelDB的话虽然wal写入会提供是否同步的选项，此处先简化优先使用异步
pub(crate) fn wal_put(wal: &Arc<HashStore>, key: Vec<u8>, value: Vec<u8>) {
    let wal = Arc::clone(wal);
    tokio::spawn(async move {
        if let Err(err) = wal.set(&key, value).await {
            error!("[LsmStore][wal_put][error happen]: {:?}", err);
        }
    });
}

#[test]
fn test_lsm_major_compactor() -> Result<()> {
    use tempfile::TempDir;
    use rand::Rng;

    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ
                            abcdefghijklmnopqrstuvwxyz
                            0123456789)(*&^%$#@!~";

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");

    tokio_test::block_on(async move {
        let config = Config::new().dir_path(temp_dir.path().into())
            .major_threshold_with_sst_size(5)
            .sst_file_size(10)
            .minor_threshold_with_data_size(5);

        let mut rng = rand::thread_rng();
        let kv_store = LsmStore::open_with_config(config).await?;
        let mut vec_key = Vec::new();

        for _ in 0..25 {
            let password: String = (0..rng.gen::<u16>())
                .map(|_| {
                    let idx = rng.gen_range(0..CHARSET.len());
                    CHARSET[idx] as char
                })
                .collect();
            let vec_u8 = rmp_serde::to_vec(&password).unwrap();
            kv_store.set(&vec_u8, vec_u8.clone()).await?;
            vec_key.push(password);
        }
        kv_store.flush().await?;
        for key in vec_key {
            let vec_u8 = rmp_serde::to_vec(&key).unwrap();
            assert_eq!(kv_store.get(&vec_u8).await?.unwrap(), vec_u8);
        }
        Ok(())
    })
}