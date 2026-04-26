use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{
    block_current_and_run_next, current_process, current_task, ProcessControlBlockInner,
    TaskControlBlock,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::{sync::Arc, vec};

const DEADLOCK: isize = -0xdead;

fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

fn task_tid(task: &Arc<TaskControlBlock>) -> usize {
    task.inner_exclusive_access().res.as_ref().unwrap().tid
}

fn ensure_mutex_state(inner: &mut ProcessControlBlockInner, tid: usize, mutex_id: usize) {
    if inner.mutex_owners.len() <= mutex_id {
        inner.mutex_owners.resize(mutex_id + 1, None);
    }
    if inner.mutex_requests.len() <= tid {
        inner.mutex_requests.resize(tid + 1, None);
    }
}

fn mutex_deadlocked(inner: &ProcessControlBlockInner, start_tid: usize) -> bool {
    let mut seen = vec![false; inner.mutex_requests.len().max(start_tid + 1)];
    let mut tid = start_tid;
    loop {
        if tid >= seen.len() {
            return false;
        }
        if seen[tid] {
            return tid == start_tid;
        }
        seen[tid] = true;
        let mutex_id = match inner.mutex_requests.get(tid).copied().flatten() {
            Some(id) => id,
            None => return false,
        };
        tid = match inner.mutex_owners.get(mutex_id).copied().flatten() {
            Some(owner) => owner,
            None => return false,
        };
    }
}

fn ensure_semaphore_state(inner: &mut ProcessControlBlockInner, tid: usize, sem_id: usize) {
    while inner.semaphore_alloc.len() <= tid {
        inner.semaphore_alloc.push(vec![0; inner.semaphore_available.len()]);
        inner.semaphore_request.push(vec![0; inner.semaphore_available.len()]);
    }
    for row in inner.semaphore_alloc.iter_mut() {
        if row.len() <= sem_id {
            row.resize(sem_id + 1, 0);
        }
    }
    for row in inner.semaphore_request.iter_mut() {
        if row.len() <= sem_id {
            row.resize(sem_id + 1, 0);
        }
    }
}

fn semaphore_deadlocked(inner: &ProcessControlBlockInner) -> bool {
    let sem_count = inner.semaphore_available.len();
    let task_count = inner
        .tasks
        .len()
        .max(inner.semaphore_alloc.len())
        .max(inner.semaphore_request.len());
    let mut work = inner.semaphore_available.clone();
    let mut finish = vec![false; task_count];
    loop {
        let mut progress = false;
        for tid in 0..task_count {
            if finish[tid] {
                continue;
            }
            let request = inner.semaphore_request.get(tid);
            let can_finish = (0..sem_count).all(|sem_id| {
                request
                    .and_then(|row| row.get(sem_id))
                    .copied()
                    .unwrap_or(0)
                    <= work[sem_id]
            });
            if can_finish {
                if let Some(alloc) = inner.semaphore_alloc.get(tid) {
                    for sem_id in 0..sem_count {
                        work[sem_id] += alloc.get(sem_id).copied().unwrap_or(0);
                    }
                }
                finish[tid] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    for tid in 0..task_count {
        if finish[tid] {
            continue;
        }
        let alloc_sum: usize = inner
            .semaphore_alloc
            .get(tid)
            .map(|row| row.iter().sum())
            .unwrap_or(0);
        let request_sum: usize = inner
            .semaphore_request
            .get(tid)
            .map(|row| row.iter().sum())
            .unwrap_or(0);
        if alloc_sum > 0 && request_sum > 0 {
            return true;
        }
    }
    false
}
/// sleep syscall
pub fn sys_sleep(ms: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_sleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}
/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        if process_inner.mutex_owners.len() <= id {
            process_inner.mutex_owners.resize(id + 1, None);
        }
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_owners.push(None);
        process_inner.mutex_list.len() as isize - 1
    }
}
/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let tid = current_tid();
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    if process_inner.deadlock_detect {
        ensure_mutex_state(&mut process_inner, tid, mutex_id);
        if let Some(owner) = process_inner.mutex_owners[mutex_id] {
            process_inner.mutex_requests[tid] = Some(mutex_id);
            if owner == tid || mutex_deadlocked(&process_inner, tid) {
                process_inner.mutex_requests[tid] = None;
                return DEADLOCK;
            }
        } else {
            process_inner.mutex_owners[mutex_id] = Some(tid);
        }
    }
    drop(process_inner);
    drop(process);
    mutex.lock();
    0
}
/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let tid = current_tid();
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    let detect = process_inner.deadlock_detect;
    ensure_mutex_state(&mut process_inner, tid, mutex_id);
    drop(process_inner);
    drop(process);
    let waking_task = mutex.unlock();
    if detect {
        let process = current_process();
        let mut process_inner = process.inner_exclusive_access();
        ensure_mutex_state(&mut process_inner, tid, mutex_id);
        if process_inner.mutex_owners[mutex_id] == Some(tid) {
            if let Some(task) = waking_task {
                let waking_tid = task_tid(&task);
                ensure_mutex_state(&mut process_inner, waking_tid, mutex_id);
                process_inner.mutex_owners[mutex_id] = Some(waking_tid);
                process_inner.mutex_requests[waking_tid] = None;
            } else {
                process_inner.mutex_owners[mutex_id] = None;
            }
        }
    }
    0
}
/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        if process_inner.semaphore_available.len() <= id {
            process_inner.semaphore_available.resize(id + 1, 0);
        }
        process_inner.semaphore_available[id] = res_count;
        ensure_semaphore_state(&mut process_inner, 0, id);
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_available.push(res_count);
        let id = process_inner.semaphore_list.len() - 1;
        ensure_semaphore_state(&mut process_inner, 0, id);
        process_inner.semaphore_list.len() - 1
    };
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let tid = current_tid();
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    let detect = process_inner.deadlock_detect;
    if detect {
        ensure_semaphore_state(&mut process_inner, tid, sem_id);
    }
    drop(process_inner);
    let waking_task = sem.up();
    if detect {
        let process = current_process();
        let mut process_inner = process.inner_exclusive_access();
        ensure_semaphore_state(&mut process_inner, tid, sem_id);
        if process_inner.semaphore_alloc[tid][sem_id] > 0 {
            process_inner.semaphore_alloc[tid][sem_id] -= 1;
        }
        if let Some(task) = waking_task {
            let waking_tid = task_tid(&task);
            ensure_semaphore_state(&mut process_inner, waking_tid, sem_id);
            if process_inner.semaphore_request[waking_tid][sem_id] > 0 {
                process_inner.semaphore_request[waking_tid][sem_id] -= 1;
            }
            process_inner.semaphore_alloc[waking_tid][sem_id] += 1;
        } else {
            process_inner.semaphore_available[sem_id] += 1;
        }
    }
    0
}
/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let tid = current_tid();
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    if process_inner.deadlock_detect {
        ensure_semaphore_state(&mut process_inner, tid, sem_id);
        if process_inner.semaphore_available[sem_id] > 0 {
            process_inner.semaphore_available[sem_id] -= 1;
            process_inner.semaphore_alloc[tid][sem_id] += 1;
        } else {
            process_inner.semaphore_request[tid][sem_id] += 1;
            if semaphore_deadlocked(&process_inner) {
                process_inner.semaphore_request[tid][sem_id] -= 1;
                return DEADLOCK;
            }
        }
    }
    drop(process_inner);
    sem.down();
    0
}
/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// YOUR JOB: Implement deadlock detection, but might not all in this syscall
pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect");
    current_process().inner_exclusive_access().deadlock_detect = enabled != 0;
    0
}
