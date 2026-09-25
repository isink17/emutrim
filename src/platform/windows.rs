use super::ProcessStats;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::net::Ipv4Addr;
use std::ptr;

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
const PROCESS_VM_READ: u32 = 0x0010;
const TH32CS_SNAPPROCESS: u32 = 0x00000002;
const AF_INET: u32 = 2;
const TCP_TABLE_OWNER_PID_LISTENER: u32 = 3;
const MIB_TCP_STATE_LISTEN: u32 = 2;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

#[repr(C)]
struct TcpRowOwnerPid {
    state: u32,
    local_addr: u32,
    local_port: u32,
    remote_addr: u32,
    remote_port: u32,
    owning_pid: u32,
}

#[repr(C)]
struct TcpTableOwnerPid {
    count: u32,
    rows: [TcpRowOwnerPid; 1],
}

#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    thread_count: u32,
    parent_process_id: u32,
    base_priority: i32,
    flags: u32,
    exe_file: [u16; 260],
}

#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        table: *mut c_void,
        size: *mut u32,
        order: i32,
        family: u32,
        table_class: u32,
        reserved: u32,
    ) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
    fn GetProcessTimes(
        process: Handle,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn GetProcessHandleCount(process: Handle, count: *mut u32) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(
        process: Handle,
        counters: *mut ProcessMemoryCountersEx,
        size: u32,
    ) -> i32;
}

struct OwnedHandle(Handle);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn console_owner_pid(port: u16) -> io::Result<Option<u32>> {
    let mut size = 0;
    let result = unsafe {
        GetExtendedTcpTable(
            ptr::null_mut(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if result != ERROR_INSUFFICIENT_BUFFER || size == 0 {
        return Err(io::Error::other(format!(
            "GetExtendedTcpTable size query failed ({result})"
        )));
    }
    let words = size.div_ceil(size_of::<usize>() as u32) as usize;
    let mut storage = vec![0usize; words];
    let result = unsafe {
        GetExtendedTcpTable(
            storage.as_mut_ptr().cast(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::other(format!(
            "GetExtendedTcpTable failed ({result})"
        )));
    }
    let table = storage.as_ptr().cast::<TcpTableOwnerPid>();
    let count = unsafe { (*table).count as usize };
    let rows = unsafe { ptr::addr_of!((*table).rows).cast::<TcpRowOwnerPid>() };
    let mut owners = Vec::new();
    for index in 0..count {
        let row = unsafe { &*rows.add(index) };
        let local = Ipv4Addr::from(row.local_addr.to_ne_bytes());
        if row.state == MIB_TCP_STATE_LISTEN
            && u16::from_be(row.local_port as u16) == port
            && (local.is_loopback() || local.is_unspecified())
        {
            owners.push(row.owning_pid);
        }
    }
    owners.sort_unstable();
    owners.dedup();
    match owners.as_slice() {
        [] => Ok(None),
        [pid] => Ok(Some(*pid)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("console port {port} has multiple process owners"),
        )),
    }
}

fn process_tree() -> io::Result<HashMap<u32, (u32, u32)>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let _snapshot = OwnedHandle(snapshot);
    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut processes = HashMap::new();
    loop {
        processes.insert(
            entry.process_id,
            (entry.parent_process_id, entry.thread_count),
        );
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            break;
        }
    }
    Ok(processes)
}

pub fn belongs_to_launch(owner_pid: u32, launch_pid: u32) -> io::Result<bool> {
    let processes = process_tree()?;
    Ok(is_descendant_or_same(owner_pid, launch_pid, &processes))
}

fn is_descendant_or_same(
    owner_pid: u32,
    launch_pid: u32,
    processes: &HashMap<u32, (u32, u32)>,
) -> bool {
    let mut current = owner_pid;
    for _ in 0..processes.len() {
        if current == launch_pid {
            return true;
        }
        let Some((parent, _)) = processes.get(&current) else {
            return false;
        };
        if *parent == current || *parent == 0 {
            return false;
        }
        current = *parent;
    }
    false
}

fn filetime_ticks(value: &FileTime) -> u64 {
    (u64::from(value.high) << 32) | u64::from(value.low)
}

fn cpu_seconds(kernel_ticks: u64, user_ticks: u64) -> f64 {
    (kernel_ticks + user_ticks) as f64 / 10_000_000.0
}

pub fn process_stats(pid: u32) -> io::Result<ProcessStats> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _handle = OwnedHandle(handle);

    let mut memory = ProcessMemoryCountersEx {
        cb: size_of::<ProcessMemoryCountersEx>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
        private_usage: 0,
    };
    if unsafe {
        GetProcessMemoryInfo(
            handle,
            &mut memory,
            size_of::<ProcessMemoryCountersEx>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut creation: FileTime = unsafe { zeroed() };
    let mut exit: FileTime = unsafe { zeroed() };
    let mut kernel: FileTime = unsafe { zeroed() };
    let mut user: FileTime = unsafe { zeroed() };
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let processes = process_tree()?;
    let thread_count = processes
        .get(&pid)
        .map(|(_, threads)| *threads)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process exited during sampling"))?;
    let mut handles = 0;
    let handle_count =
        (unsafe { GetProcessHandleCount(handle, &mut handles) } != 0).then_some(handles);

    Ok(ProcessStats {
        pid,
        working_set_bytes: memory.working_set_size as u64,
        private_bytes: memory.private_usage as u64,
        cpu_seconds: cpu_seconds(filetime_ticks(&kernel), filetime_ticks(&user)),
        thread_count,
        handle_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launched_process_must_own_console_or_be_ancestor() {
        let parents = HashMap::from([(50, (40, 2)), (40, (30, 1)), (70, (60, 1))]);
        assert!(is_descendant_or_same(50, 40, &parents));
        assert!(is_descendant_or_same(50, 30, &parents));
        assert!(!is_descendant_or_same(70, 30, &parents));
        assert!(!is_descendant_or_same(999, 30, &parents));
    }

    #[test]
    fn filetime_ticks_convert_to_cpu_seconds() {
        assert_eq!(cpu_seconds(5_000_000, 15_000_000), 2.0);
    }
}
