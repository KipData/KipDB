use std::sync::Arc;
use std::time::Instant;
use futures::future;
use itertools::Itertools;
use tokio::sync::oneshot;
use tracing::{error, info};
use crate::KvsError;
use crate::kernel::io::IoFactory;
use crate::kernel::{CommandData, Result};
use crate::kernel::lsm::lsm_kv::Config;
use crate::kernel::lsm::{data_sharding, MemTable};
use crate::kernel::lsm::log::LogLoader;
use crate::kernel::lsm::ss_table::{Scope, SSTable};
use crate::kernel::lsm::version::{VersionEdit, VersionStatus};

pub(crate) const LEVEL_0: usize = 0;

/// 数据分片集
/// 包含对应分片的Gen与数据
pub(crate) type MergeShardingVec = Vec<(i64, Vec<CommandData>)>;

/// Major压缩时的待删除Gen封装(N为此次Major所压缩的Level)，第一个为Level N级，第二个为Level N+1级
pub(crate) type DelGenVec = (Vec<i64>, Vec<i64>);

/// Store与Compactor的交互信息
#[derive(Debug)]
pub(crate) enum CompactTask {
    Flush(oneshot::Sender<()>, bool),
    Drop
}

/// 压缩器
///
/// 负责Minor和Major压缩
pub(crate) struct Compactor {
    ver_status: Arc<VersionStatus>,
    config: Arc<Config>,
    sst_factory: Arc<IoFactory>,

    // mem_table与wal都是用于管理LSMStore的写入控制
    // XXX: 感觉共享状态比较多，可以进行统一封装？
    mem_table: Arc<MemTable>,
    wal: Arc<LogLoader>,
}

impl Compactor {

    pub(crate) fn new(
        ver_status: Arc<VersionStatus>,
        config: Arc<Config>,
        sst_factory: Arc<IoFactory>,
        mem_table: Arc<MemTable>,
        wal: Arc<LogLoader>
    ) -> Self {
        Compactor {
            ver_status,
            config,
            sst_factory,
            mem_table,
            wal,
        }
    }

    /// 检查并进行压缩 （默认为 异步、被动 的Lazy压缩）
    ///
    /// 默认为try检测是否超出阈值，主要思路为以被动定时检测的机制使
    /// 多事务的commit脱离Compactor的耦合，
    /// 同时减少高并发事务或写入时的频繁Compaction，优先写入后统一压缩，
    /// 减少Level 0热数据的SSTable的冗余数据
    #[allow(clippy::expect_used)]
    pub(crate) async fn check_then_compaction(
        &mut self,
        enable_caching: bool,
        option_tx: Option<oneshot::Sender<()>>
    ) {
        let exceeded_len = self.config.minor_threshold_with_len;

        if let Some((values, last_seq_id)) =
            // 当存在tx时则说明为阻塞压缩，因此不能使用try
            // 并且不会判断阈值强制压缩
            if option_tx.is_some() {
                self.mem_table.swap()
            } else {
                self.mem_table.try_exceeded_then_swap(exceeded_len)
            }
        {
            if !values.is_empty() {
                let gen = self.create_gen()
                    .expect("Log switch error!");
                let start = Instant::now();
                // 目前minor触发major时是同步进行的，所以此处对live_tag是在此方法体保持存活
                if let Err(err) = self.minor_compaction(
                    gen, last_seq_id, values, enable_caching
                ).await {
                    error!("[Compactor][minor_compaction][error happen]: {:?}", err);
                }
                info!("[Compactor][Compaction Drop][Time: {:?}]", start.elapsed());
            }
        }

        // 压缩请求响应
        let _ignore = option_tx.map(|tx| {
                tx.send(()).expect("compactor response error!")
            });
    }

    /// 创建gen
    ///
    /// 需要保证获取到了MemTable的写锁以保证wal在switch时MemTable的数据和Wal不一致(多出几条)
    /// 当wal配置启用时，使用预先记录的gen作为结果
    fn create_gen(&self) -> Result<i64> {
        let next_gen = self.config.create_gen_lazy();

        Ok(if self.config.wal_enable {
            self.wal.switch(next_gen)?
        } else {
            next_gen
        })
    }

    /// 持久化immutable_table为SSTable
    ///
    /// 请注意：vec_values必须是依照key值有序的
    pub(crate) async fn minor_compaction(
        &self,
        gen: i64,
        last_seq_id: i64,
        values: Vec<CommandData>,
        enable_caching: bool
    ) -> Result<()> {
        if !values.is_empty() {
            // 从内存表中将数据持久化为ss_table
            let ss_table = SSTable::create_for_mem_table(
                &self.config,
                gen,
                &self.sst_factory,
                values,
                LEVEL_0
            )?;

            self.ver_status
                .insert_vec_ss_table(vec![ss_table], enable_caching).await?;

            // `Compactor::data_loading_with_level`中会检测是否达到压缩阈值，因此此处直接调用Major压缩
            if let Err(err) = self.major_compaction(
                LEVEL_0,
                vec![
                    VersionEdit::NewFile((vec![gen], 0), 0),
                    VersionEdit::LastSequenceId(last_seq_id)
                ]
            ).await {
                error!("[LSMStore][major_compaction][error happen]: {:?}", err);
            }
        }
        Ok(())
    }

    /// Major压缩，负责将不同Level之间的数据向下层压缩转移
    /// 目前Major压缩的大体步骤是
    ///
    /// 1、获取当前Version，读取当前Level的指定数量SSTable，命名为vec_ss_table_l
    ///
    /// 2、vec_ss_table_l的每个SSTable中的scope属性进行融合，并以此获取下一Level与该scope相交的SSTable，命名为vec_ss_table_l_1
    ///
    /// 3、获取的vec_ss_table_l_1向上一Level进行类似第2步骤的措施，获取两级之间压缩范围内最恰当的数据
    ///
    /// 4、vec_ss_table_l与vec_ss_table_l_1之间的数据并行取出排序归并去重等处理后，分片成多个Vec<CommandData>
    ///
    /// 6、并行将每个分片各自生成SSTable
    ///
    /// 7、生成的SSTables插入到vec_ss_table_l的第一个SSTable位置，并将vec_ss_table_l和vec_ss_table_l_1的SSTable删除
    ///
    /// 8、将变更的SSTable插入至vec_ver_edit以持久化
    ///
    /// Final: 将vec_ver_edit中的数据进行log_and_apply生成新的Version作为最新状态
    ///
    /// 经过压缩测试，Level 1的SSTable总是较多，根据原理推断：
    /// Level0的Key基本是无序的，容易生成大量的SSTable至Level1
    /// 而Level1-7的Key排布有序，故转移至下一层的SSTable数量较小
    /// 因此大量数据压缩的情况下Level 1的SSTable数量会较多
    pub(crate) async fn major_compaction(&self, mut level: usize, mut vec_ver_edit: Vec<VersionEdit>) -> Result<()> {
        if level > 6 {
            return Err(KvsError::LevelOver);
        }
        let config = &self.config;

        while level < 7 {
            if let Some((index, (del_gens_l, del_gens_ll), vec_sharding)) =
                self.data_loading_with_level(level)
                    .await?
            {

                let start = Instant::now();
                // 并行创建SSTable
                let ss_table_futures = vec_sharding.into_iter()
                    .map(|(gen, sharding)| {
                        async move {
                            SSTable::create_for_mem_table(
                                config,
                                gen,
                                &self.sst_factory,
                                sharding,
                                level + 1
                            )
                        }
                    });
                let vec_new_ss_table: Vec<SSTable> = future::try_join_all(ss_table_futures).await?;

                let vec_new_sst_gen = vec_new_ss_table.iter()
                    .map(SSTable::get_gen)
                    .collect_vec();
                self.ver_status
                    .insert_vec_ss_table(vec_new_ss_table, true).await?;

                vec_ver_edit.push(VersionEdit::NewFile((vec_new_sst_gen, level + 1), index));
                vec_ver_edit.push(VersionEdit::DeleteFile((del_gens_l, level)));
                vec_ver_edit.push(VersionEdit::DeleteFile((del_gens_ll, level)));
                info!("[LsmStore][Major Compaction][recreate_sst][Level: {}][Time: {:?}]", level, start.elapsed());
                level += 1;
            } else { break }
        }
        self.ver_status
            .log_and_apply(vec_ver_edit).await?;
        Ok(())
    }

    /// 通过Level进行归并数据加载
    async fn data_loading_with_level(&self, level: usize) -> Result<Option<(usize, DelGenVec, MergeShardingVec)>> {
        let version = self.ver_status
            .current()
            .await;
        let config = &self.config;
        let major_select_file_size = config.major_select_file_size;

        // 如果该Level的SSTables数量尚未越出阈值则提取返回空
        if level > 5 || !version.is_threshold_exceeded_major(config, level)
        {
            return Ok(None);
        }

        // 此处vec_ss_table_l指此level的Vec<SSTable>, vec_ss_table_ll则是下一级的Vec<SSTable>
        // 类似罗马数字
        if let Some(mut vec_ss_table_l) = version
            .get_first_vec_ss_table_with_size(level, major_select_file_size).await
        {
            let start = Instant::now();

            let scope_l = Scope::fusion_from_vec_ss_table(&vec_ss_table_l)?;

            // 获取下一级中有重复键值范围的SSTable
            let vec_ss_table_ll =
                version.get_meet_scope_ss_tables(level + 1, &scope_l).await;

            let index = SSTable::find_index_with_level(
                vec_ss_table_ll.first().map(SSTable::get_gen),
                &version,
                level + 1
            );

            // 若为Level 0则与获取同级下是否存在有键值范围冲突数据并插入至del_gen_l中
            if level == LEVEL_0 {
                vec_ss_table_l.append(
                    &mut version.get_meet_scope_ss_tables(level, &scope_l).await
                )
            }

            // 收集需要清除的SSTable
            let del_gen_l = SSTable::collect_gen(&vec_ss_table_l)?;
            let del_gen_ll = SSTable::collect_gen(&vec_ss_table_ll)?;

            // 此处没有chain vec_ss_table_l是因为在vec_ss_table_ll是由vec_ss_table_l检测冲突而获取到的
            // 因此使用vec_ss_table_ll向上检测冲突时获取的集合应当含有vec_ss_table_l的元素
            let vec_ss_table_final = match Scope::fusion_from_vec_ss_table(&vec_ss_table_ll) {
                Ok(scope_ll) => version.get_meet_scope_ss_tables(level, &scope_ll).await,
                Err(_) => vec_ss_table_l
            }.into_iter()
                .chain(vec_ss_table_ll)
                .unique_by(SSTable::get_gen)
                .collect_vec();

            // 数据合并并切片
            let vec_merge_sharding =
                Self::data_merge_and_sharding(&vec_ss_table_final, &self.config).await?;

            info!("[LsmStore][Major Compaction][data_loading_with_level][Time: {:?}]", start.elapsed());

            Ok(Some((index, (del_gen_l, del_gen_ll), vec_merge_sharding)))
        } else {
            Ok(None)
        }
    }

    /// 以SSTables的数据归并再排序后切片，获取以Command的Key值由小到大的切片排序
    /// 收集所有SSTable的get_all_data的future，并行执行并对数据进行去重以及排序
    /// 真他妈完美
    async fn data_merge_and_sharding(
        vec_ss_table: &[SSTable],
        config: &Config
    ) -> Result<MergeShardingVec> {
        // 需要对SSTable进行排序，可能并发创建的SSTable可能确实名字会重复，但是目前SSTable的判断新鲜度的依据目前为Gen
        // SSTable使用雪花算法进行生成，所以并行创建也不会导致名字重复(极小概率除外)
        let map_futures = vec_ss_table.iter()
            .sorted_unstable_by_key(|ss_table| ss_table.get_gen())
            .map(SSTable::all);
        let vec_cmd_data = future::try_join_all(map_futures)
            .await?
            .into_iter()
            .flatten()
            .rev()
            .unique_by(CommandData::get_key_clone)
            .sorted_unstable_by_key(CommandData::get_key_clone)
            .collect();
        Ok(data_sharding(vec_cmd_data, config.sst_file_size, config))
    }
}
