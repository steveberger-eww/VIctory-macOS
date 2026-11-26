//! Where every thread is: `VICTORY_STACKS=<seconds>[,<seconds>...]` prints
//! each thread's unwound stack as `module+rva` that many seconds after
//! [`dump_stacks_on_schedule`], to find what a title waits on.

use std::ffi::c_void;

use windows_sys::Wdk::System::Threading::{
    NtQueryInformationThread, ThreadBasicInformation, ThreadQuerySetWin32StartAddress,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadDescription, OpenThread, ResumeThread,
    SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use crate::crash::{MAX_FRAMES, locate, unwind_into};

/// Reads `VICTORY_STACKS` and, if set, starts a thread that prints every
/// thread's stack at each listed time (seconds from now).
pub fn dump_stacks_on_schedule() {
    let Ok(list) = std::env::var("VICTORY_STACKS") else {
        return;
    };
    let times = schedule(&list);
    let start = std::time::Instant::now();
    // `VICTORY_STACKS_FILE=<path>`: the dumps go to that file instead, in a
    // form that needs no lock another thread could hold (#50: in a freeze
    // the dump thread never printed, as if a stuck thread held stderr, the
    // heap or the loader lock). The module table is written now, while
    // all is well; the dumps give raw addresses.
    let raw = std::env::var_os("VICTORY_STACKS_FILE").and_then(|path| {
        let file = std::fs::File::create(path).ok()?;
        modules(&file);
        Some(file)
    });
    // Room for every thread id, made before any dump.
    let mut ids: Vec<u32> = Vec::with_capacity(8192);
    // `VICTORY_STACKS_BUSY=1`: the dumps as a sampling profile (see `Busy`).
    let mut busy = (std::env::var("VICTORY_STACKS_BUSY").as_deref() == Ok("1")).then(Busy::default);
    std::thread::spawn(move || {
        for at in times {
            std::thread::sleep(at.saturating_sub(start.elapsed()));
            match &raw {
                Some(file) => dump_raw(file, at.as_secs(), &mut ids, busy.as_mut()),
                None => dump_stacks(),
            }
        }
    });
}

/// A line formatted on the stack, written with one `WriteFile`: no
/// allocation, no stderr lock.
fn line(mut file: &std::fs::File, args: std::fmt::Arguments) {
    use std::io::Write;
    struct Buf([u8; 256], usize);
    impl std::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            let n = s.len().min(self.0.len() - 1 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut b = Buf([0; 256], 0);
    let _ = std::fmt::Write::write_fmt(&mut b, args);
    b.0[b.1] = b'\n';
    let _ = file.write_all(&b.0[..=b.1]);
}

/// Every loaded module's name, base and size, so raw addresses can be
/// placed later.
fn modules(file: &std::fs::File) {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    };
    // SAFETY: a snapshot of this process's modules.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return;
    }
    // SAFETY: zeroed is a valid MODULEENTRY32W; its size is set before use.
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    // SAFETY: `snapshot` is open; `entry` is sized.
    let mut more = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
    while more {
        let len = entry.szModule.iter().position(|&c| c == 0).unwrap_or(entry.szModule.len());
        let name = String::from_utf16_lossy(&entry.szModule[..len]);
        line(file, format_args!("module {name} base {:#x} size {:#x}", entry.modBaseAddr as usize, entry.modBaseSize));
        // SAFETY: as above.
        more = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: we opened it.
    unsafe { CloseHandle(snapshot) };
}

/// For many dumps read as a sampling profile: after the first four, only
/// the threads whose top frame differed between dumps are dumped. The rest
/// wait, and leaving them out takes a dump of a title's hundred-odd threads
/// from over a second to a fraction of one.
#[derive(Default)]
struct Busy {
    dumps: u32,
    /// By thread: its top frame in the last dump, and whether it ever changed.
    tops: std::collections::HashMap<u32, (u64, bool)>,
}

/// [`dump_stacks`] into `file`, with a line before each step, so a dump
/// that stops shows where.
fn dump_raw(file: &std::fs::File, at: u64, ids: &mut Vec<u32>, mut busy: Option<&mut Busy>) {
    line(file, format_args!("dump at {at} s: taking a thread snapshot"));
    // SAFETY: plain queries and a snapshot of this process's threads.
    let (me, process, snapshot) = unsafe {
        (GetCurrentThreadId(), GetCurrentProcessId(), CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0))
    };
    if snapshot == INVALID_HANDLE_VALUE {
        line(file, format_args!("dump at {at} s: no snapshot"));
        return;
    }
    ids.clear();
    // SAFETY: zeroed is a valid THREADENTRY32; its size is set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: `snapshot` is open; `entry` is sized.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more && ids.len() < ids.capacity() {
        if entry.th32OwnerProcessID == process && entry.th32ThreadID != me {
            ids.push(entry.th32ThreadID);
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: we opened it.
    unsafe { CloseHandle(snapshot) };
    if let Some(b) = busy.as_mut() {
        b.dumps += 1;
        if b.dumps > 4 {
            ids.retain(|id| b.tops.get(id).is_some_and(|t| t.1));
        }
    }
    line(file, format_args!("dump at {at} s: {} threads", ids.len()));
    for &id in ids.iter() {
        line(file, format_args!("  thread {id}: opening"));
        // SAFETY: opens a thread of our own process.
        let thread = unsafe {
            OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, id)
        };
        if thread.is_null() {
            continue;
        }
        line(file, format_args!("  thread {id} start {:#x}", start_of(thread)));
        let (frames, (priority, base)) = frames_of(thread);
        if let Some(b) = busy.as_mut() {
            let top = frames.first().copied().unwrap_or(0);
            let seen = b.tops.entry(id).or_insert((top, false));
            *seen = (top, seen.1 || seen.0 != top);
        }
        line(file, format_args!("  thread {id} priority {priority} (base {base}), {} frames", frames.len()));
        for (i, pc) in frames.into_iter().enumerate() {
            line(file, format_args!("    #{i:<2} {pc:#x}"));
        }
        // SAFETY: we opened it.
        unsafe { CloseHandle(thread) };
    }
    line(file, format_args!("dump at {at} s: done"));
}

/// The times in a `VICTORY_STACKS` list (seconds, comma-separated), in
/// order. Negative times are now; times no `Duration` holds (`inf`, `NaN`,
/// `1e20`) are left out instead of panicking the dump thread.
fn schedule(list: &str) -> Vec<std::time::Duration> {
    let mut times: Vec<_> = list
        .split(',')
        .filter_map(|t| t.trim().parse::<f64>().ok())
        .filter(|t| !t.is_nan())
        .filter_map(|t| std::time::Duration::try_from_secs_f64(t.max(0.0)).ok())
        .collect();
    times.sort();
    times
}

/// `THREAD_BASIC_INFORMATION` (ntddk).
#[repr(C)]
struct BasicInfo {
    exit_status: i32,
    teb: *const usize,
    client_id: [usize; 2],
    affinity: usize,
    priority: i32,
    base_priority: i32,
}

/// `GetThreadContext` wants a 16-byte aligned context.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

/// Prints every other thread's stack to stderr. Each thread is suspended
/// only while its context is read and unwound into a buffer allocated
/// beforehand (it may hold the heap lock).
pub fn dump_stacks() {
    // SAFETY: plain queries and a snapshot of this process's threads.
    let (me, process, snapshot) = unsafe {
        (
            GetCurrentThreadId(),
            GetCurrentProcessId(),
            CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0),
        )
    };
    if snapshot == INVALID_HANDLE_VALUE {
        return;
    }
    let mut ids = Vec::new();
    // SAFETY: zeroed is a valid THREADENTRY32; its size is set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: `snapshot` is open; `entry` is sized.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == process && entry.th32ThreadID != me {
            ids.push(entry.th32ThreadID);
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: we opened it.
    unsafe { CloseHandle(snapshot) };
    eprintln!("VIctory: stacks of {} threads", ids.len());
    for id in ids {
        // SAFETY: opens a thread of our own process.
        let thread = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                0,
                id,
            )
        };
        if thread.is_null() {
            continue;
        }
        let (frames, (priority, base)) = frames_of(thread);
        eprintln!(
            "  thread {id} \"{}\" start {} priority {priority} (base {base})",
            name_of(thread),
            locate(start_of(thread))
        );
        for (i, pc) in frames.into_iter().enumerate() {
            eprintln!("    #{i:<2} {}", locate(pc as usize));
        }
        // SAFETY: we opened it.
        unsafe { CloseHandle(thread) };
    }
}

/// The thread's frames and its current and base scheduling priority (0-31).
fn frames_of(thread: HANDLE) -> (Vec<u64>, (i32, i32)) {
    let mut frames = Vec::with_capacity(MAX_FRAMES);
    let mut info: BasicInfo = BasicInfo {
        exit_status: 0,
        teb: std::ptr::null(),
        client_id: [0; 2],
        affinity: 0,
        priority: 0,
        base_priority: 0,
    };
    // Suspended first: a running thread may exit and free its TEB between
    // the query and the read below.
    // SAFETY: a thread of this process that is not this one.
    if unsafe { SuspendThread(thread) } == u32::MAX {
        return (frames, (0, 0));
    }
    // SAFETY: writes `info`, whose layout and size are the system's.
    let status = unsafe {
        NtQueryInformationThread(
            thread,
            ThreadBasicInformation,
            (&raw mut info).cast(),
            size_of::<BasicInfo>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status != 0 || info.teb.is_null() {
        // SAFETY: suspended above.
        unsafe { ResumeThread(thread) };
        return (frames, (0, 0));
    }
    // SAFETY: the TEB of a suspended thread of this process: its NT_TIB
    // holds the stack base at +8 and the stack limit at +0x10.
    let (high, low) = unsafe { (info.teb.add(1).read(), info.teb.add(2).read()) };
    // SAFETY: zeroed is a valid CONTEXT.
    let mut context = Aligned(unsafe { std::mem::zeroed() });
    context.0.ContextFlags = CONTEXT_FULL_AMD64;
    // SAFETY: reads and resumes the thread suspended above; nothing until
    // the resume allocates or takes a lock the thread could hold besides
    // the unwinder's own.
    unsafe {
        if GetThreadContext(thread, &mut context.0) != 0 {
            unwind_into(context.0, low, high, &mut frames);
        }
        ResumeThread(thread);
    }
    (frames, (info.priority, info.base_priority))
}

fn start_of(thread: HANDLE) -> usize {
    let mut start = 0usize;
    // SAFETY: writes one pointer-sized start address.
    unsafe {
        NtQueryInformationThread(
            thread,
            ThreadQuerySetWin32StartAddress,
            (&raw mut start).cast(),
            size_of::<usize>() as u32,
            std::ptr::null_mut(),
        )
    };
    start
}

fn name_of(thread: HANDLE) -> String {
    let mut name = std::ptr::null_mut();
    // SAFETY: the system allocates the string; we free it with LocalFree.
    unsafe {
        if GetThreadDescription(thread, &mut name) < 0 || name.is_null() {
            return String::new();
        }
        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
        LocalFree(name.cast::<c_void>());
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw dump of this process (a sleeping thread kept for it) writes
    /// its module table and every step's marker, ending in "done".
    #[test]
    fn raw_dumps_write_markers_and_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stacks.txt");
        let file = std::fs::File::create(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let sleeper = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        modules(&file);
        let mut ids = Vec::with_capacity(8192);
        dump_raw(&file, 7, &mut ids);
        drop(tx);
        sleeper.join().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().any(|l| l.starts_with("module ")), "{text}");
        assert!(text.contains("dump at 7 s: taking a thread snapshot"));
        assert!(text.contains("frames"), "{text}");
        assert!(text.contains("    #0  0x"), "{text}");
        assert!(text.trim_end().ends_with("dump at 7 s: done"), "{text}");
    }

    #[test]
    fn another_threads_stack_unwinds_to_its_start() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let parked = std::thread::spawn(move || {
            // SAFETY: plain query.
            tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            stop_rx.recv().ok();
        });
        let id = rx.recv().unwrap();
        // SAFETY: a thread of this process.
        let thread = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                0,
                id,
            )
        };
        assert!(!thread.is_null());
        let (frames, _) = frames_of(thread);
        // Waiting in the kernel, several frames down from the thread start.
        assert!(frames.len() >= 3, "{frames:?}");
        assert!(start_of(thread) != 0);
        // SAFETY: opened above.
        unsafe { CloseHandle(thread) };
        dump_stacks();
        stop_tx.send(()).unwrap();
        parked.join().unwrap();
    }

    /// Times no `Duration` holds are left out instead of panicking the dump
    /// thread (`from_secs_f64` panicked on `inf`); the rest come in order.
    #[test]
    fn schedules_skip_times_out_of_range() {
        let secs = |list| {
            schedule(list)
                .iter()
                .map(|d| d.as_secs_f64())
                .collect::<Vec<_>>()
        };
        assert_eq!(secs("30, 5,x,-2,inf,NaN,1e20"), [0.0, 5.0, 30.0]);
        assert!(schedule("").is_empty());
    }
}
