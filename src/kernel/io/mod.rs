pub(crate) mod buf;
pub(crate) mod direct;
mod mem;

use crate::kernel::io::buf::{BufIoReader, BufIoWriter};
use crate::kernel::io::direct::{DirectIoReader, DirectIoWriter};
use crate::kernel::io::mem::{MemIoReader, MemIoWriter};
use crate::kernel::Result;
use crate::KernelError;
use bytes::BytesMut;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Copy, Clone)]
pub enum FileExtension {
    Log,
    SSTable,
    Manifest,
}

impl FileExtension {
    pub(crate) fn extension_str(&self) -> &'static str {
        match self {
            FileExtension::Log => "log",
            FileExtension::SSTable => "sst",
            FileExtension::Manifest => "manifest",
        }
    }

    /// 对文件夹路径填充日志文件名
    pub(crate) fn path_with_gen(&self, dir: &Path, gen: i64) -> PathBuf {
        dir.join(format!("{gen}.{}", self.extension_str()))
    }
}

pub struct IoFactory {
    dir_path: Arc<PathBuf>,
    extension: Arc<FileExtension>,
    mem_files: Mutex<HashMap<i64, MemIoWriter>>,
}

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum IoType {
    Buf,
    Direct,
    Mem,
}

impl IoFactory {
    #[inline]
    pub fn reader(&self, gen: i64, io_type: IoType) -> Result<Box<dyn IoReader>> {
        let dir_path = Arc::clone(&self.dir_path);
        let extension = Arc::clone(&self.extension);

        Ok(match io_type {
            IoType::Buf => Box::new(BufIoReader::new(dir_path, gen, extension)?),
            IoType::Direct => Box::new(DirectIoReader::new(dir_path, gen, extension)?),
            IoType::Mem => {
                let bytes = self
                    .mem_files
                    .lock()
                    .get(&gen)
                    .ok_or(KernelError::FileNotFound)?
                    .bytes();
                Box::new(MemIoReader::new(gen, bytes))
            }
        })
    }

    #[inline]
    pub fn writer(&self, gen: i64, io_type: IoType) -> Result<Box<dyn IoWriter>> {
        let dir_path = Arc::clone(&self.dir_path);
        let extension = Arc::clone(&self.extension);

        Ok(match io_type {
            IoType::Buf => Box::new(BufIoWriter::new(dir_path, gen, extension)?),
            IoType::Direct => Box::new(DirectIoWriter::new(dir_path, gen, extension)?),
            IoType::Mem => Box::new(self.load_mem_file(gen)),
        })
    }

    #[inline]
    pub fn get_path(&self) -> &PathBuf {
        &self.dir_path
    }

    fn load_mem_file(&self, gen: i64) -> MemIoWriter {
        self.mem_files
            .lock()
            .entry(gen)
            .or_insert_with(|| MemIoWriter::new(BytesMut::new()))
            .clone()
    }

    #[inline]
    pub fn new(dir_path: impl Into<PathBuf>, extension: FileExtension) -> Result<Self> {
        let path_buf = dir_path.into();
        // 创建文件夹（如果他们缺失）
        fs::create_dir_all(&path_buf)?;
        let dir_path = Arc::new(path_buf);
        let extension = Arc::new(extension);
        let mem_files = Mutex::new(HashMap::new());

        Ok(Self {
            dir_path,
            extension,
            mem_files,
        })
    }

    #[inline]
    pub fn clean(&self, gen: i64) -> Result<()> {
        if self.mem_files.lock().remove(&gen).is_some() {
            return Ok(());
        }

        fs::remove_file(self.extension.path_with_gen(&self.dir_path, gen))?;
        Ok(())
    }

    #[inline]
    pub fn exists(&self, gen: i64) -> Result<bool> {
        if self.mem_files.lock().contains_key(&gen) {
            return Ok(true);
        }

        let path = self.extension.path_with_gen(&self.dir_path, gen);
        Ok(fs::try_exists(path)?)
    }
}

pub trait IoReader: Send + Sync + 'static + Read + Seek {
    fn get_gen(&self) -> i64;

    fn get_path(&self) -> PathBuf;

    #[inline]
    fn file_size(&self) -> Result<u64> {
        let path_buf = self.get_path();
        Ok(fs::metadata(path_buf)?.len())
    }

    fn get_type(&self) -> IoType;
}

pub trait IoWriter: Send + Sync + 'static + Write {
    fn current_pos(&mut self) -> Result<u64>;
}
