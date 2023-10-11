use failure::Fail;
use std::io;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::oneshot::error::RecvError;

/// Error type for kvs
#[derive(Fail, Debug)]
#[non_exhaustive]
pub enum KernelError {
    /// IO error
    #[fail(display = "{}", _0)]
    Io(#[cause] io::Error),
    #[fail(display = "{}", _0)]
    Recv(#[cause] RecvError),

    /// Serialization or deserialization error
    #[fail(display = "{}", _0)]
    SerdeBinCode(#[cause] Box<bincode::ErrorKind>),
    /// Remove no-existent key error
    #[fail(display = "Key not found")]
    KeyNotFound,
    #[fail(display = "Data is empty")]
    DataEmpty,
    #[fail(display = "Max Level is 7")]
    LevelOver,
    #[fail(display = "Not the correct type of Cmd")]
    NotMatchCmd,
    #[fail(display = "CRC code does not match")]
    CrcMisMatch,
    #[fail(display = "{}", _0)]
    SledErr(#[cause] sled::Error),
    #[fail(display = "Cache size overflow")]
    CacheSizeOverFlow,
    #[fail(display = "Cache sharding and size overflow")]
    CacheShardingNotAlign,
    #[fail(display = "File not found")]
    FileNotFound,
    /// 正常情况wal在内存中存在索引则表示硬盘中存在有对应的数据
    /// 而错误则是内存存在索引却在硬盘中不存在这个数据
    #[fail(display = "WAL log load error")]
    WalLoad,
    /// Unexpected command type error.
    /// It indicated a corrupted log or a program bug.
    #[fail(display = "Unexpected command type")]
    UnexpectedCommandType,
    #[fail(display = "Process already exists")]
    ProcessExists,
    #[fail(display = "channel is closed")]
    ChannelClose,
    #[fail(display = "{}", _0)]
    NotSupport(&'static str),
}

#[derive(Fail, Debug)]
#[non_exhaustive]
pub enum ConnectionError {
    #[fail(display = "{}", _0)]
    IO(#[cause] io::Error),
    #[fail(display = "disconnected")]
    Disconnected,
    #[fail(display = "write failed")]
    WriteFailed,
    #[fail(display = "wrong instruction")]
    WrongInstruction,
    #[fail(display = "encode error")]
    EncodeErr,
    #[fail(display = "decode error")]
    DecodeErr,
    #[fail(display = "server flush error")]
    FlushError,
    #[fail(display = "{}", _0)]
    StoreErr(#[cause] KernelError),
    #[fail(display = "Failed to connect to server, {}", _0)]
    TonicTransportErr(#[cause] tonic::transport::Error),
    #[fail(display = "Failed to call server, {}", _0)]
    TonicFailureStatus(#[cause] tonic::Status),
    #[fail(display = "Failed to parse addr, {}", _0)]
    AddrParseError(#[cause] std::net::AddrParseError),
}

#[derive(Fail, Debug)]
#[non_exhaustive]
#[allow(missing_copy_implementations)]
pub enum CacheError {
    #[fail(display = "The number of caches cannot be divisible by the number of shards")]
    ShardingNotAlign,
    #[fail(display = "Cache size overflow")]
    CacheSizeOverFlow,
    #[fail(display = "{}", _0)]
    StoreErr(#[cause] KernelError),
}

impl<T> From<SendError<T>> for KernelError {
    #[inline]
    fn from(_: SendError<T>) -> Self {
        KernelError::ChannelClose
    }
}

impl From<io::Error> for ConnectionError {
    #[inline]
    fn from(err: io::Error) -> Self {
        ConnectionError::IO(err)
    }
}

impl From<io::Error> for KernelError {
    #[inline]
    fn from(err: io::Error) -> Self {
        KernelError::Io(err)
    }
}

impl From<RecvError> for KernelError {
    #[inline]
    fn from(err: RecvError) -> Self {
        KernelError::Recv(err)
    }
}

impl From<Box<bincode::ErrorKind>> for KernelError {
    #[inline]
    fn from(err: Box<bincode::ErrorKind>) -> Self {
        KernelError::SerdeBinCode(err)
    }
}

impl From<sled::Error> for KernelError {
    #[inline]
    fn from(err: sled::Error) -> Self {
        KernelError::SledErr(err)
    }
}

impl From<KernelError> for ConnectionError {
    #[inline]
    fn from(err: KernelError) -> Self {
        ConnectionError::StoreErr(err)
    }
}

impl From<tonic::Status> for ConnectionError {
    #[inline]
    fn from(status: tonic::Status) -> Self {
        ConnectionError::TonicFailureStatus(status)
    }
}

impl From<tonic::transport::Error> for ConnectionError {
    #[inline]
    fn from(err: tonic::transport::Error) -> Self {
        ConnectionError::TonicTransportErr(err)
    }
}

impl From<std::net::AddrParseError> for ConnectionError {
    #[inline]
    fn from(err: std::net::AddrParseError) -> Self {
        ConnectionError::AddrParseError(err)
    }
}

impl From<CacheError> for KernelError {
    #[inline]
    fn from(value: CacheError) -> Self {
        match value {
            CacheError::StoreErr(kv_error) => kv_error,
            CacheError::CacheSizeOverFlow => KernelError::CacheSizeOverFlow,
            CacheError::ShardingNotAlign => KernelError::CacheShardingNotAlign,
        }
    }
}

impl From<KernelError> for CacheError {
    #[inline]
    fn from(value: KernelError) -> Self {
        CacheError::StoreErr(value)
    }
}
