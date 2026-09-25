#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
mod unsupported;

#[cfg(not(windows))]
pub use unsupported::*;
#[cfg(windows)]
pub use windows::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessStats {
    pub pid: u32,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub cpu_seconds: f64,
    pub thread_count: u32,
    pub handle_count: Option<u32>,
}
