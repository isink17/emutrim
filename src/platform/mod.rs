#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;

#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(not(any(windows, target_os = "macos")))]
pub use unsupported::*;
#[cfg(windows)]
pub use windows::*;

pub const fn supports_verified_integrated_start() -> bool {
    cfg!(any(
        windows,
        all(target_os = "macos", target_arch = "aarch64")
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn integrated_start_capability_matches_supported_hosts() {
        assert_eq!(
            super::supports_verified_integrated_start(),
            cfg!(any(
                windows,
                all(target_os = "macos", target_arch = "aarch64")
            ))
        );
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessStats {
    pub pid: u32,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub cpu_seconds: f64,
    pub thread_count: u32,
    pub handle_count: Option<u32>,
}
