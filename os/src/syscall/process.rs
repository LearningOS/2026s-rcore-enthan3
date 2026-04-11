//! Process management syscalls

use crate::mm::translated_byte_buffer_checked;
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next,
    get_current_syscall_times, mmap_current, munmap_current, suspend_current_and_run_next,
};
use crate::timer::get_time_us;
use core::mem::size_of;
use core::slice;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");

    let us = get_time_us();
    let tv = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };

    let src = unsafe {
        slice::from_raw_parts((&tv as *const TimeVal) as *const u8, size_of::<TimeVal>())
    };

    let token = current_user_token();
    let mut bufs = match translated_byte_buffer_checked(token, ts as *mut u8, src.len(), true) {
        Some(v) => v,
        None => return -1,
    };

    let mut offset = 0;
    for buf in bufs.iter_mut() {
        let n = buf.len();
        buf.copy_from_slice(&src[offset..offset + n]);
        offset += n;
    }
    0
}

/// trace syscall
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");

    let token = current_user_token();

    match trace_request {
        // read one byte from current task memory
        0 => {
            let bufs = match translated_byte_buffer_checked(token, id as *mut u8, 1, false) {
                Some(v) => v,
                None => return -1,
            };
            bufs[0][0] as isize
        }
        // write one byte to current task memory
        1 => {
            let mut bufs = match translated_byte_buffer_checked(token, id as *mut u8, 1, true) {
                Some(v) => v,
                None => return -1,
            };
            bufs[0][0] = data as u8;
            0
        }
        // query syscall count of current task
        2 => get_current_syscall_times(id) as isize,
        _ => -1,
    }
}

/// Implement mmap.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    if mmap_current(start, len, prot) {
        0
    } else {
        -1
    }
}

/// Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if munmap_current(start, len) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}