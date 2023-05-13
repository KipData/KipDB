use std::collections::hash_map::RandomState;
use std::collections::HashSet;
use std::sync::Arc;
use bytes::Bytes;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;
use tracing::{error, info};
use crate::kernel::Result;
use crate::kernel::io::{FileExtension, IoFactory, IoType, IoWriter};
use crate::kernel::lsm::SSTableLoader;
use crate::kernel::lsm::block::BlockCache;
use crate::kernel::lsm::compactor::LEVEL_0;
use crate::kernel::lsm::log::{LogLoader, LogWriter};
use crate::kernel::lsm::lsm_kv::Config;
use crate::kernel::lsm::ss_table::{Scope, SSTable};
use crate::kernel::utils::lru_cache::ShardingLruCache;
use crate::KernelError::SSTableLost;

pub(crate) const DEFAULT_SS_TABLE_PATH: &str = "ss_table";

pub(crate) const DEFAULT_VERSION_PATH: (&str, Option<i64>) = ("version", Some(0));

pub(crate) type LevelSlice = [Vec<Scope>; 7];

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum VersionEdit {
    DeleteFile((Vec<i64>, usize)),
    // 确保新File的Gen都是比旧Version更大(新鲜)
    // Level 0则请忽略第二位的index参数，默认会放至最尾
    NewFile((Vec<Scope>, usize), usize),
    // // Level and SSTable Gen List
    // CompactPoint(usize, Vec<i64>),
}

#[derive(Debug)]
enum CleanTag {
    Clean(u64),
    Add(u64, Vec<i64>)
}

/// SSTable的文件删除器
///
/// 整体的设计思路是由`Version::drop`进行删除驱动
/// 考虑过在Compactor中进行文件删除，但这样会需要进行额外的阈值判断以触发压缩(Compactor的阈值判断是通过传入的KV进行累计)
struct Cleaner {
    ss_table_loader: Arc<RwLock<SSTableLoader>>,
    tag_rx: UnboundedReceiver<CleanTag>,
    del_gens: Vec<(u64, Vec<i64>)>,
}

impl Cleaner {
    fn new(
        ss_table_loader: &Arc<RwLock<SSTableLoader>>,
        tag_rx: UnboundedReceiver<CleanTag>
    ) -> Self {
        Self {
            ss_table_loader: Arc::clone(ss_table_loader),
            tag_rx,
            del_gens: vec![],
        }
    }

    /// 监听tag_rev传递的信号
    ///
    /// 当tag_tx drop后自动关闭
    async fn listen(&mut self) {
        loop {
            match self.tag_rx.recv().await {
                Some(CleanTag::Clean(ver_num)) => self.clean(ver_num).await,
                Some(CleanTag::Add(ver_num,  vec_gen)) => {
                    self.del_gens.push((ver_num, vec_gen));
                },
                // 关闭时对此次运行中的暂存Version全部进行删除
                None => {
                    let all_ver_num = self.del_gens.iter()
                        .map(|(ver_num, _)| ver_num)
                        .cloned()
                        .collect_vec();
                    for ver_num in all_ver_num {
                        self.clean(ver_num).await
                    }
                    return
                }
            }
        }
    }

    /// 传入ver_num进行冗余SSTable的删除
    ///
    /// 整体删除逻辑: 当某个Version Drop时，以它的version_num作为基准，
    /// 检测该version_num在del_gens(应以version_num为顺序)的位置
    /// 当为第一位时说明无前置Version在使用，因此可以直接将此version_num的vec_gens全部删除
    /// 否则将对应位置的vec_gens添加至前一位的vec_gens中，使前一个Version开始clean时能将转移过来的vec_gens一起删除
    async fn clean(&mut self, ver_num: u64) {
        if let Some(index) = Self::find_index_with_ver_num(&self.del_gens, ver_num) {
            let (_, mut vec_gen) = self.del_gens.remove(index);
            if index == 0 {
                let mut ss_table_loader = self.ss_table_loader.write().await;
                // 当此Version处于第一位时，直接将其删除
                for gen in vec_gen {
                    let _ignore = ss_table_loader.remove(&gen);
                    if let Err(err) = ss_table_loader.clean(gen) {
                        error!("[Cleaner][clean][SSTable: {}]: Remove Error!: {:?}", gen, err);
                    };
                }
            } else {
                // 若非Version并非第一位，为了不影响前面Version对SSTable的读取处理，将待删除的SSTable的gen转移至前一位
                if let Some((_, pre_vec_gen)) = self.del_gens.get_mut(index - 1) {
                    pre_vec_gen.append(&mut vec_gen);
                }
            }
        }
    }

    fn find_index_with_ver_num(del_gen: &[(u64, Vec<i64>)], ver_num: u64) -> Option<usize> {
        del_gen.iter()
            .enumerate()
            .find(|(_, (vn, _))| {
                vn == &ver_num
            })
            .map(|(index, _)| index)
    }
}

/// 用于切换Version的封装Inner
struct VersionInner {
    version: Arc<Version>,
    /// TODO: 日志快照
    ver_log_writer: LogWriter<Box<dyn IoWriter>>
}

pub(crate) struct VersionStatus {
    inner: RwLock<VersionInner>,
    ss_table_loader: Arc<RwLock<SSTableLoader>>,
    sst_factory: Arc<IoFactory>,
    /// 用于Drop时通知Cleaner drop
    _cleaner_tx: UnboundedSender<CleanTag>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub(crate) struct VersionMeta {
    /// SSTable集合占有磁盘大小
    size_of_disk: u64,
    /// SSTable集合中指令数量
    len: usize,
}

#[derive(Clone)]
pub(crate) struct Version {
    version_num: u64,
    /// SSTable存储Map
    /// 全局共享
    ss_tables_loader: Arc<RwLock<SSTableLoader>>,
    /// Level层级Vec
    /// 以索引0为level-0这样的递推，存储文件的gen值
    /// 每个Version各持有各自的Gen矩阵
    level_slice: LevelSlice,
    /// 统计数据
    meta_data: VersionMeta,
    /// 稀疏区间数据Block缓存
    pub(crate) block_cache: Arc<BlockCache>,
    /// 清除信号发送器
    /// Drop时通知Cleaner进行删除
    clean_sender: UnboundedSender<CleanTag>
}

impl VersionStatus {
    pub(crate) fn get_sst_factory_ref(&self) -> &IoFactory {
        &self.sst_factory
    }

    pub(crate) async fn load_with_path(
        config: Config,
        wal: LogLoader,
    ) -> Result<Self> {
        let sst_path = config.path().join(DEFAULT_SS_TABLE_PATH);

        let block_cache = Arc::new(ShardingLruCache::new(
            config.block_cache_size,
            16,
            RandomState::default()
        )?);
        let sst_factory = Arc::new(
            IoFactory::new(
                sst_path.clone(),
                FileExtension::SSTable
            )?
        );

        let ss_table_loader = Arc::new(RwLock::new(
            SSTableLoader::new(
                config.clone(),
                Arc::clone(&sst_factory),
                wal
            )?
        ));

        let (ver_log_loader, vec_log, log_gen) = LogLoader::reload(
            config.path(),
            DEFAULT_VERSION_PATH,
            IoType::Direct,
            |bytes| Ok(bincode::deserialize::<VersionEdit>(bytes)?)
        )?;

        let (tag_sender, tag_rev) = unbounded_channel();
        let version = Arc::new(
            Version::load_from_log(
                vec_log,
                &ss_table_loader,
                &block_cache,
                tag_sender.clone()
            ).await?
        );

        let mut cleaner = Cleaner::new(
            &ss_table_loader,
            tag_rev
        );

        let _ignore = tokio::spawn(async move {
            cleaner.listen().await;
        });

        let ver_log_writer = ver_log_loader.writer(log_gen)?;

        Ok(Self {
            inner: RwLock::new(VersionInner { version, ver_log_writer }),
            ss_table_loader,
            sst_factory,
            _cleaner_tx: tag_sender,
        })
    }

    fn ss_table_insert(
        ss_table_loader: &mut SSTableLoader,
        ss_table: SSTable,
    ) -> Option<SSTable> {
        // 初始化成功时直接传入SSTable的索引中
        ss_table_loader.insert(ss_table)
    }

    pub(crate) async fn current(&self) -> Arc<Version> {
        Arc::clone(
            &self.inner.read().await.version
        )
    }

    pub(crate) async fn insert_vec_ss_table(&self, vec_ss_table: Vec<SSTable>) -> Result<()> {
        let mut ss_table_loader = self.ss_table_loader.write().await;

        for ss_table in vec_ss_table {
            let _ignore = Self::ss_table_insert(&mut ss_table_loader, ss_table);
        }

        Ok(())
    }

    /// 对一组VersionEdit持久化并应用
    pub(crate) async fn log_and_apply(
        &self,
        vec_version_edit: Vec<VersionEdit>,
    ) -> Result<()> {
        let mut new_version = Version::clone(
            self.current().await
                .as_ref()
        );
        let mut inner = self.inner.write().await;
        version_display(&new_version, "log_and_apply");

        for bytes in vec_version_edit.iter()
            .filter_map(|edit| bincode::serialize(&edit).ok())
        {
            let _ = inner.ver_log_writer.add_record(&bytes)?;
        }
        new_version.apply(vec_version_edit, false).await?;
        inner.version = Arc::new(new_version);

        Ok(())
    }
}

impl Version {
    pub(crate) fn get_len(&self) -> usize {
        self.meta_data.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.get_len() == 0
    }

    pub(crate) fn level_len(&self, level: usize) -> usize {
        self.level_slice[level].len()
    }

    pub(crate) fn get_size_of_disk(&self) -> u64 {
        self.meta_data.size_of_disk
    }

    /// 创建一个空的Version
    fn new(
        ss_table_loader: &Arc<RwLock<SSTableLoader>>,
        block_cache: &Arc<BlockCache>,
        clean_sender: UnboundedSender<CleanTag>,
    ) -> Self {
        Self {
            version_num: 0,
            ss_tables_loader: Arc::clone(ss_table_loader),
            level_slice: Self::level_slice_new(),
            block_cache: Arc::clone(block_cache),
            meta_data: VersionMeta { size_of_disk: 0, len: 0 },
            clean_sender,
        }
    }

    /// 通过一组VersionEdit载入Version
    async fn load_from_log(
        vec_log: Vec<VersionEdit>,
        ss_table_loader: &Arc<RwLock<SSTableLoader>>,
        block_cache: &Arc<BlockCache>,
        sender: UnboundedSender<CleanTag>
    ) -> Result<Self>{
        let mut version = Self::new(
            ss_table_loader,
            block_cache,
            sender,
        );

        version.apply(vec_log, true).await?;
        version_display(&version, "load_from_log");

        Ok(version)
    }

    /// Version对VersionEdit的应用处理
    ///
    /// Tips: 当此处像Cleaner发送Tag::Add时，此时的version中不需要的gens
    /// 因此每次删除都是删除此Version的前一位所需要删除的Version
    /// 也就是可能存在一次Version的冗余SSTable
    /// 可能是个确定，但是Minor Compactor比较起来更加频繁，也就是大多数情况不会冗余，因此我觉得影响较小
    /// 也可以算作是一种Major Compaction异常时的备份？
    async fn apply(&mut self, vec_version_edit: Vec<VersionEdit>, is_init: bool) -> Result<()> {
        let mut del_gens = vec![];
        let loader = self.ss_tables_loader.read().await;
        // 初始化时使用gen_set确定最终SSTable的持有状态再进行数据统计处理
        // 避免日志重溯时对最终状态不存在的SSTable进行数据统计处理
        // 导致SSTableMap不存在此SSTable而抛出`KvsError::SSTableLostError`
        let mut gen_set = HashSet::new();

        for version_edit in vec_version_edit {
            match version_edit {
                VersionEdit::DeleteFile((mut vec_gen, level)) => {
                    if !is_init {
                        Self::apply_del_on_running(
                            &mut self.meta_data,
                            &loader,
                            &vec_gen
                        ).await?;
                    }

                    for gen in vec_gen.iter() {
                        let _ignore = gen_set.remove(gen);
                    }
                    self.level_slice[level]
                        .retain(|scope| !vec_gen.contains(&scope.get_gen()));
                    del_gens.append(&mut vec_gen);
                }
                VersionEdit::NewFile((vec_scope, level), index) => {
                    let vec_gen = Self::map_gen(&vec_scope);

                    if !is_init {
                        Self::apply_add(
                            &mut self.meta_data,
                            &loader,
                            &vec_gen
                        ).await?;
                    }
                    for gen in vec_gen.iter() {
                        let _ignore = gen_set.insert(*gen);
                    }
                    // Level 0中的SSTable绝对是以gen为优先级
                    // Level N中则不以gen为顺序，此处对gen排序是因为单次NewFile中的gen肯定是有序的
                    if level == LEVEL_0 {
                        for scope in vec_scope.into_iter().sorted_by_key(Scope::get_gen) {
                            self.level_slice[level].push(scope);
                        }
                    } else {
                        for scope in vec_scope.into_iter().sorted_by_key(Scope::get_gen).rev() {
                            self.level_slice[level].insert(index, scope);
                        }
                    }
                }
            }
        }
        // 在初始化时进行统计数据累加
        // 注意与运行时统计数据处理互斥
        if is_init {
            Self::apply_add(
                &mut self.meta_data,
                &loader,
                &Vec::from_iter(gen_set)
            ).await?;
        }

        self.version_num += 1;
        self.clean_sender.send(CleanTag::Add(self.version_num, del_gens))?;
        Ok(())
    }

    fn map_gen(vec_gen: &[Scope]) -> Vec<i64> {
        vec_gen.iter()
            .map(Scope::get_gen)
            .collect_vec()
    }

    async fn apply_add(meta_data: &mut VersionMeta, ss_table_loader: &SSTableLoader, vec_gen: &[i64]) -> Result<()>  {
        meta_data.statistical_process(
            ss_table_loader,
            vec_gen,
            |meta_data, ss_table| {
                meta_data.size_of_disk += ss_table.get_size_of_disk();
                meta_data.len += ss_table.len();
            }
        ).await?;
        Ok(())
    }

    async fn apply_del_on_running(meta_data: &mut VersionMeta, ss_table_loader: &SSTableLoader, vec_gen: &[i64]) -> Result<()> {
        meta_data.statistical_process(
            ss_table_loader,
            vec_gen,
            |meta_data, ss_table| {
                meta_data.size_of_disk -= ss_table.get_size_of_disk();
                meta_data.len -= ss_table.len();
            }
        ).await?;
        Ok(())
    }

    fn level_slice_new() -> [Vec<Scope>; 7] {
        [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()]
    }

    pub(crate) async fn get_ss_table(&self, level: usize, offset: usize) -> Option<SSTable> {
        if let Some(scope) = self.level_slice[level].get(offset) {
            self.ss_tables_loader.read().await
                .get(scope.get_gen())
        } else { None }
    }

    pub(crate) fn get_index(&self, level: usize, source_gen: i64) -> Option<usize> {
        self.level_slice[level].iter()
            .enumerate()
            .find(|(_ , scope)| source_gen.eq(&scope.get_gen()))
            .map(|(index, _)| index)
    }

    pub(crate) async fn first_ss_tables(&self, level: usize, size: usize) -> Option<(Vec<SSTable>, Vec<Scope>)> {
        let ss_table_loader = self.ss_tables_loader.read().await;

        if self.level_slice[level].is_empty() {
            return None
        }

        Some(self.level_slice[level]
            .iter()
            .take(size)
            .filter_map(|scope| {
                ss_table_loader.get(scope.get_gen())
                    .map(|ss_table| (ss_table, scope.clone()))
            })
            .unzip())
    }

    /// 获取指定level中与scope冲突的SSTables和Scopes
    pub(crate) async fn get_meet_scope_ss_tables_with_scopes(&self, level: usize, target_scope: &Scope) -> (Vec<SSTable>, Vec<Scope>) {
        let ss_table_loader = self.ss_tables_loader.read().await;

        self.level_slice[level].iter()
            .filter(|scope| scope.meet(target_scope))
            .filter_map(|scope| {
                ss_table_loader.get(scope.get_gen())
                    .map(|ss_table| (ss_table, scope.clone()))
            })
            .unzip()
    }

    /// 获取指定level中与scope冲突的SSTables
    pub(crate) async fn get_meet_scope_ss_tables(&self, level: usize, target_scope: &Scope) -> Vec<SSTable> {
        let ss_table_loader = self.ss_tables_loader.read().await;

        self.level_slice[level].iter()
            .filter(|scope| scope.meet(target_scope))
            .filter_map(|scope| ss_table_loader.get(scope.get_gen()))
            .collect_vec()
    }

    /// 使用Key从现有SSTables中获取对应的数据
    pub(crate) async fn find_data_for_ss_tables(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let ss_table_loader = self.ss_tables_loader.read().await;
        let block_cache = &self.block_cache;

        // Level 0的SSTable是无序且SSTable间的数据是可能重复的,因此需要遍历
        for scope in self.level_slice[LEVEL_0]
            .iter()
            .rev()
        {
            if scope.meet_with_key(key) {
                if let Some(ss_table) = ss_table_loader.get(scope.get_gen()) {
                    if let Some(value) = ss_table.query_with_key(key, block_cache)? {
                        return Ok(Some(value))
                    }
                }
            }
        }
        // Level 1-7的数据排布有序且唯一，因此在每一个等级可以直接找到唯一一个Key可能在范围内的SSTable
        for level in 1..7 {
            let offset = self.query_meet_index(key, level);

            if let Some(scope) = self.level_slice[level].get(offset) {
                return if let Some(ss_table) = ss_table_loader.get(scope.get_gen()) {
                    ss_table.query_with_key(key, block_cache)
                } else { Ok(None) };
            }
        }

        Ok(None)
    }

    pub(crate) fn query_meet_index(&self, key: &[u8], level: usize) -> usize {
        self.level_slice[level]
            .binary_search_by(|scope| scope.start.as_ref().cmp(key))
            .unwrap_or_else(|index| index.saturating_sub(1))
    }

    /// 判断是否溢出指定的SSTable数量
    pub(crate) fn is_threshold_exceeded_major(&self, config: &Config, level: usize) -> bool {
        self.level_slice[level].len() >=
            (config.major_threshold_with_sst_size * config.level_sst_magnification.pow(level as u32))
    }
}

impl VersionMeta {
    // MetaData对SSTable统计数据处理
    async fn statistical_process<F>(
        &mut self,
        ss_table_loader: &SSTableLoader,
        vec_gen: &[i64],
        fn_process: F
    ) -> Result<()>
        where F: Fn(&mut VersionMeta, &SSTable)
    {
        for gen in vec_gen.iter() {
            let ss_table = ss_table_loader.get(*gen)
                .ok_or_else(|| SSTableLost)?;
            fn_process(self, &ss_table);
        }

        Ok(())
    }
}

impl Drop for Version {
    /// 将此Version可删除的版本号发送
    fn drop(&mut self) {
        if self.clean_sender.send(CleanTag::Clean(self.version_num)).is_err() {
            error!("[Cleaner][clean][SSTable: {}]: Channel Close!", self.version_num);
        }
    }
}

/// 使用特定格式进行display
fn version_display(new_version: &Version, method: &str) {
    info!(
            "[Version: {}]: version_num: {}, len: {}, size_of_disk: {}",
            method,
            new_version.version_num,
            new_version.get_len(),
            new_version.get_size_of_disk(),
        );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use bytes::Bytes;
    use tempfile::TempDir;
    use tokio::time;
    use crate::kernel::io::{FileExtension, IoFactory, IoType};
    use crate::kernel::lsm::log::LogLoader;
    use crate::kernel::lsm::lsm_kv::Config;
    use crate::kernel::lsm::mem_table::DEFAULT_WAL_PATH;
    use crate::kernel::lsm::ss_table::SSTable;
    use crate::kernel::lsm::version::{DEFAULT_SS_TABLE_PATH, Version, VersionEdit, VersionStatus};
    use crate::kernel::Result;

    #[test]
    fn test_version_clean() -> Result<()> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");

        tokio_test::block_on(async move {

            let config = Config::new(temp_dir.into_path());

            let (wal, _, _) = LogLoader::reload(
                config.path(),
                (DEFAULT_WAL_PATH, Some(1)),
                IoType::Direct,
                |_| Ok(())
            )?;

            // 注意：将ss_table的创建防止VersionStatus的创建前
            // 因为VersionStatus检测无Log时会扫描当前文件夹下的SSTable进行重组以进行容灾
            let ver_status =
                VersionStatus::load_with_path(config.clone(), wal.clone()).await?;


            let sst_factory = IoFactory::new(
                config.dir_path.join(DEFAULT_SS_TABLE_PATH),
                FileExtension::SSTable
            )?;

            let (ss_table_1, scope_1) = SSTable::create_for_mem_table(
                &config,
                1,
                &sst_factory,
                vec![(Bytes::from_static(b"test"), None)],
                0,
                IoType::Direct
            )?;

            let (ss_table_2, scope_2) = SSTable::create_for_mem_table(
                &config,
                2,
                &sst_factory,
                vec![(Bytes::from_static(b"test"), None)],
                0,
                IoType::Direct
            )?;

            ver_status.insert_vec_ss_table(vec![ss_table_1]).await?;
            ver_status.insert_vec_ss_table(vec![ss_table_2]).await?;

            let vec_edit_1 = vec![
                VersionEdit::NewFile((vec![scope_1], 0),0),
            ];

            ver_status.log_and_apply(vec_edit_1).await?;

            let version_1 = Arc::clone(&ver_status.current().await);

            let vec_edit_2 = vec![
                VersionEdit::NewFile((vec![scope_2], 0),0),
                VersionEdit::DeleteFile((vec![1], 0)),
            ];

            ver_status.log_and_apply(vec_edit_2).await?;

            let version_2 = Arc::clone(&ver_status.current().await);

            let vec_edit_3 = vec![
                VersionEdit::DeleteFile((vec![2], 0)),
            ];

            // 用于去除version2的引用计数
            ver_status.log_and_apply(vec_edit_3).await?;

            assert!(sst_factory.exists(1)?);
            assert!(sst_factory.exists(2)?);

            drop(version_2);

            assert!(sst_factory.exists(1)?);
            assert!(sst_factory.exists(2)?);

            drop(version_1);
            time::sleep(Duration::from_secs(1)).await;

            assert!(!sst_factory.exists(1)?);
            assert!(sst_factory.exists(2)?);

            drop(ver_status);
            time::sleep(Duration::from_secs(1)).await;

            assert!(!sst_factory.exists(1)?);
            assert!(!sst_factory.exists(2)?);

            Ok(())
        })
    }

    #[test]
    fn test_version_apply_and_log() -> Result<()> {
        let temp_dir = TempDir::new().expect("unable to create temporary working directory");

        tokio_test::block_on(async move {

            let config = Config::new(temp_dir.into_path());

            let (wal, _, _) = LogLoader::reload(
                config.path(),
                (DEFAULT_WAL_PATH, Some(1)),
                IoType::Direct,
                |_| Ok(())
            )?;

            // 注意：将ss_table的创建防止VersionStatus的创建前
            // 因为VersionStatus检测无Log时会扫描当前文件夹下的SSTable进行重组以进行容灾
            let ver_status_1 =
                VersionStatus::load_with_path(config.clone(), wal.clone()).await?;


            let sst_factory = IoFactory::new(
                config.dir_path.join(DEFAULT_SS_TABLE_PATH),
                FileExtension::SSTable
            )?;

            let (ss_table_1, scope_1) = SSTable::create_for_mem_table(
                &config,
                1,
                &sst_factory,
                vec![(Bytes::from_static(b"test"), None)],
                0,
                IoType::Direct
            )?;

            let (ss_table_2, scope_2) = SSTable::create_for_mem_table(
                &config,
                2,
                &sst_factory,
                vec![(Bytes::from_static(b"test"), None)],
                0,
                IoType::Direct
            )?;

            let vec_edit = vec![
                VersionEdit::NewFile((vec![scope_1], 0),0),
                VersionEdit::NewFile((vec![scope_2], 0),0),
                VersionEdit::DeleteFile((vec![2], 0)),
            ];

            ver_status_1.insert_vec_ss_table(vec![ss_table_1]).await?;
            ver_status_1.insert_vec_ss_table(vec![ss_table_2]).await?;
            ver_status_1.log_and_apply(vec_edit).await?;

            let version_1 = Version::clone(ver_status_1.current().await.as_ref());

            drop(ver_status_1);

            let ver_status_2 =
                VersionStatus::load_with_path(config, wal.clone()).await?;
            let version_2 = ver_status_2.current().await;

            assert_eq!(version_1.level_slice, version_2.level_slice);
            assert_eq!(version_1.meta_data, version_2.meta_data);

            Ok(())
        })
    }
}



