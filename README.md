# KipDB - Keep it Public DB

<p align="left">
  <a href="https://github.com/KKould/KipDB" target="_blank">
    <img src="https://img.shields.io/github/stars/KKould/KipDB.svg?style=social" alt="github star"/>
    <img src="https://img.shields.io/github/forks/KKould/KipDB.svg?style=social" alt="github fork"/>
  </a>
</p>

[![Crates.io](https://img.shields.io/crates/v/kip_db.svg)](https://crates.io/crates/kip_db/)
[![LICENSE](https://img.shields.io/github/license/kkould/kipdb.svg)](https://github.com/kkould/kipdb/blob/master/LICENSE)
[![Rust Community](https://img.shields.io/badge/Rust_Community%20-Join_us-brightgreen?style=plastic&logo=rust)](https://www.rust-lang.org/community)

**KipDB** 轻量级键值存储引擎

整体设计参考LevelDB，旨在作为NewSQL分布式数据库的存储引擎 
- 支持嵌入式/单机存储/远程调用等多应用场景
- 以[Kiss](https://zh.m.wikipedia.org/zh/KISS%E5%8E%9F%E5%88%99)作为开发理念，设计以简单而高效为主
- 实现MVCC以支持ACID
- 高性能，BenchMark写入吞吐量约为Sled的两倍，且大数据量下的顺序读取平均延迟为1μs左右
- 远程连接使用ProtoBuf实现，支持多语言通信
- 极小的内存占用(待机/大量冷数据)
- 并发安全，读读、读写并行

 **组件原理Wiki** : https://github.com/KKould/KipDB/wiki

## 快速上手 🤞
#### 组件引入
``` toml
kip_db = "0.1.2-alpha.0"
```
### 代码编译
#### 基本编译
``` shell
# 代码编译
cargo build

# 代码编译(正式环境)
cargo build --release

# 单元测试
cargo test

# 性能基准测试
cargo bench
```

#### Docker镜像编译
``` shell
# 编译镜像
docker build -t kould/kip-db:v1 .

# 运行镜像
docker run kould/kip-db:v1
```

### 直接调用(基本使用)
```rust
/// 指定文件夹以开启一个KvStore
let kip_db = LsmStore::open("/welcome/kip_db").await?;

// 插入数据
kip_db.set(&b"https://github.com/KKould/KipDB", Bytes::from(&b"your star plz"[..])).await?;
// 获取数据
let six_pence = kip_db.get(&b"my deposit").await?;
// 已占有硬盘大小
let just_lot = kip_db.size_of_disk().await?
// 已有数据数量
let how_many_times_you_inserted = kip_db.len().await?;
// 删除数据
kip_db.remove(&b"ex girlfriend").await?;

// 创建事务
let mut transaction = kip_db.new_transaction().await?;
// 插入数据至事务中
transaction.set(&b"this moment", Bytes::from(&b"hope u like it"[..]));
// 删除该事务中key对应的value
transaction.remove(&b"trouble")?;
// 获取此事务中key对应的value
let ping_cap = transaction.get(&b"dream job")?;
// 提交事务
transaction.commit().await?;

// 创建持久化数据迭代器
let guard = kip_db.iter().await?;
let mut iterator = guard.iter()?;

// 获取下一个元素
let hello = iterator.next_err()?;
// 移动至第一个元素
let world = iterator.seek(Seek::Last)?;

// 强制数据刷入硬盘
kip_db.flush().await?;
```
### 远程应用
#### 服务启动
```rust
/// 服务端启动！
let listener = TcpListener::bind("127.0.0.1:8080").await?;

kip_db::net::server::run(listener, tokio::signal::ctrl_c()).await;
```
#### 远程调用
```rust
/// 客户端调用！
let mut client = Client::connect("127.0.0.1:8080").await?;

// 插入数据
client.set(&vec![b'k'], vec![b'v']).await?
// 获取数据
client.get(&vec![b'k']).await?
// 已占有硬盘大小
client.size_of_disk().await?
// 存入指令数
client.len().await?
// 数据刷入硬盘
client.flush().await?
// 删除数据
client.remove(&vec![b'k']).await?;
// 批量指令执行(可选 并行/同步 执行)
let vec_batch_cmd = vec![CommandData::get(b"k1".to_vec()), CommandData::get(b"k2".to_vec())];
client.batch(vec_batch_cmd, true).await?
```

## 内置多种持久化内核👍
- LsmStore: LSM存储，使用Leveled Compaction策略(默认内核)
- HashStore: 类Bitcask
- SledStore: 基于Sled数据库进行封装

## 操作示例⌨️
### 服务端
``` shell
PS D:\Workspace\kould\KipDB\target\release> ./server -h
KipDB-Server 0.1.0
Kould <2435992353@qq.com>
A KV-Store server

USAGE:
server.exe [OPTIONS]

OPTIONS:
-h, --help           Print help information
--ip <IP>
--port <PORT>
-V, --version        Print version information

PS D:\Workspace\kould\KipDB\target\release> ./server   
2022-10-13T06:50:06.528875Z  INFO kip_db::kernel::lsm::ss_table: [SsTable: 6985961041465315323][restore_from_file][TableMetaInfo]: MetaInfo { level: 0, version: 0, data_len: 118, index_len: 97, part_size: 64, crc_code: 43553795 }, Size of Disk: 263
2022-10-13T06:50:06.529614Z  INFO kip_db::net::server: [Listener][Inbound Connections]
2022-10-13T06:50:13.437586Z  INFO kip_db::net::server: [Listener][Shutting Down]

```
### 客户端
``` shell
PS D:\Workspace\kould\KipDB\target\release> ./cli --help
KipDB-Cli 0.1.0
Kould <2435992353@qq.com>
Issue KipDB Commands

USAGE:
    cli.exe [OPTIONS] <SUBCOMMAND>

OPTIONS:
    -h, --help                   Print help information
        --hostname <hostname>    [default: 127.0.0.1]
        --port <PORT>            [default: 6333]
    -V, --version                Print version information

SUBCOMMANDS:
    batch-get
    batch-remove
    batch-set
    flush
    get
    help                     Print this message or the help of the given subcommand(s)
    len
    remove
    set
    size-of-disk
    
PS D:\Workspace\kould\KipDB\target\release> ./cli batch-set kould kipdb welcome !
2022-09-27T09:50:11.768931Z  INFO cli: ["Done!", "Done!"]

PS D:\Workspace\kould\KipDB\target\release> ./cli batch-get kould kipdb          
2022-09-27T09:50:32.753919Z  INFO cli: ["welcome", "!"]
```

## Features🌠
- Marjor Compation 
  - 多级递增循环压缩 ✅
  - SSTable压缩状态互斥
    - 避免并行压缩时数据范围重复 ✅
- KVStore
  - 参考Sled增加api
    - size_of_disk ✅
    - clear
    - contains_key
    - iter ✅
    - len ✅
    - is_empty ✅
    - ...
  - 多进程锁 ✅
    - 防止多进程对文件进行读写造成数据异常
- SSTable
  - 布隆过滤器 ✅
    - 加快获取键值的速度
  - MetaBlock ✅
    - 用于存储统计数据布隆过滤器的存放
- Block
  - DataBlock、IndexBlock复用实现并共享缓存 ✅
  - 实现前缀压缩并使用varint编码以及LZ4减小空间占用 ✅
  - 基于前缀进行二分查询 ✅
- Cache
  - TableCache: SSTableLoader懒加载 ✅
  - BlockCache: 稀疏索引数据块缓存 ✅
  - 类LevelDB的并行LruCache: ShardingLruCache ✅
-  Iterator 迭代器
   - BlockIterator ✅
   - SSTableIterator ✅
   - LevelIterator ✅
   - VersionIterator ✅
- WAL 防灾日志
  - 落盘时异常后重启数据回复 ✅
  - 读取数据不存在时尝试读取 ✅
- MVCC单机事务 ✅
  - Manifest多版本持久化 ✅
  - SSTable多版本持久化 ✅
- 网络通信
  - 使用ProtoBuf进行多语言序列化 ✅
  - Ruby of KipDB
  - Java of KipDB
  - Rust of KipDB ✅
- 分布式
  - 使用Raft复制协议保持状态一致
## Perf火焰图监测
- 为了方便性能调优等监测，提供了两个Dockerfile作为支持
  - Dockerfile: KipDB的Server与Cli
  - Dockerfile-perf: 外部Perf监测

### 使用步骤
1. 打包KipDB本体镜像``docker build -t kould/kip-db:v1 .``
2. 打包Perf监测镜像``docker build -f Dockerfile-perf -t kould/perf:v1 .``
3. 以任意形式执行kould/kip
   - 例: ``docker run kould/kip-db:v1``
4. 执行``attach-win.sh <kip-db容器ID>``
   - 例: ``./attach-win.sh 263ad21cc56169ebec79bbf614c6986a78ec89a6e0bdad5e364571d28bee2bfc``
5. 在该bash内输入. ``record.sh <kip-db的server进程pid>``
   - 若不清楚进程id是多少可以直接输入ps，通常为1
   - 注意!： 不要关闭bash，否则会监听失败！
6. **随后去对KipDB进行对应需要监测的操作**
7. 操作完毕后回到**步骤5**的bash内，以ctrl + c终止监听，得到perf.data
8. 继续在该bash内输入``. plot.sh <图片名.svg>``, 即可生成火焰图
    - 导出图片一般可使用 ``docker cp`` 和 ``docker exec`` 或挂载 volume，为方便预览和复制文件，容器内置了轻量网页服务，执行 ``thttpd -p <端口号>`` 即可。由于脚本中没有设置端口转发，需要 ``docker inspect <目标容器ID> | grep IPAdress`` 查看目标容器的 IP，然后在浏览器中访问即可。若需要更灵活的操作，可不用以上脚本手动添加参数运行容器。

参考自：https://chinggg.github.io/post/docker-perf/

### 如果你想参与KipDB或[KipSQL](https://github.com/KipData/KipSQL)，欢迎通过下方微信二维码与我交流
![微信联系方式](./static/images/wechat.png)

### Thanks For
![JetBrains](./static/images/jetbrains.png)
