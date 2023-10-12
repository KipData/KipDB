#![feature(fs_try_exists)]
#![feature(cursor_remaining)]
#![feature(slice_pattern)]
#![feature(bound_map)]

pub mod config;
pub mod error;
pub mod kernel;
pub mod proto;
pub mod server;

pub use error::KernelError;

pub const DEFAULT_PORT: u16 = 6333;

pub const LOCAL_IP: &str = "127.0.0.1";
