#![allow(non_camel_case_types)]

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::mem;
use std::process::exit;
use std::rc::Rc;
use std::thread::sleep_ms;
use log::{error, info, warn};
use crate::backend::{Backend, RegisterARM64};
use crate::emulator::{AndroidEmulator, SvcMemory, VMPointer};
use crate::emulator::thread::TaskStatus;
use crate::keystone::assemble_no_check;
use crate::linux::syscalls;
use crate::memory::svc_memory::SvcCallResult;

const EXCP_UDEF: u32 = 1;
const EXCP_SWI: u32 = 2;
const EXCP_BKPT: u32 = 7;

pub const POST_CALLBACK_SYSCALL_NUMBER: u64 = 0x8866 - 1;
pub const PRE_CALLBACK_SYSCALL_NUMBER: u64 = 0x8888 - 1;

const SWI_MAX: i32 = 0xffff;

#[inline]
#[cfg(feature = "unicorn_backend")]
fn arm64_syscall_handler_unicorn<T: Clone>(unicorn: &unicorn_engine::Unicorn<T>, intno: u32, swi: i32, emulator: &AndroidEmulator<T>) {
    if intno == EXCP_BKPT { // brk
        panic!("Not support brk!");
    }

    if intno == EXCP_UDEF { // udef
        unicorn.dump_context(0, 0);
        panic!("Not support udef: swi={}", swi);
    }

    if intno != EXCP_SWI {
        panic!("Unsupported INTNO: {}!", intno);
    }

    let nr = get_syscall(&emulator.backend);
    let svc_memory = &mut emulator.inner_mut().svc_memory;
    if swi != 0 {
        if swi == SWI_MAX {
            panic!("余生很长，何必慌张") // PopContextException
        }
        if swi == SWI_MAX - 1 {
            panic!("完成了很想做的事情之后，真的很轻松啊 ୧⍢⃝୨") // ThreadContextSwitchException
        }
        let svc = svc_memory.get_svc(swi as u32);
        if let Some(svc) = svc {
            match svc.handle(&emulator) {
                Ok(Some(ret)) => {
                    unicorn.reg_write_i64(unicorn_engine::RegisterARM64::X0, ret).unwrap();
                }
                Ok(None) => {}
                Err(e) => {
                    error!("svc handle failed: {:?}", e);
                    unicorn.emu_stop()
                        .expect("failed to stop emulator");
                }
            }
            return;
        }
        unicorn.emu_stop()
            .expect("failed to stop emulator");
        panic!("swi number: {} not found", swi);
    }
    else if nr == Syscalls::__NR_io_setup && swi == 0 && unicorn.reg_read(unicorn_engine::RegisterARM64::X16).unwrap() == POST_CALLBACK_SYSCALL_NUMBER {
        let number = unicorn.reg_read(unicorn_engine::RegisterARM64::X12).unwrap();
        let svc = svc_memory.get_svc(number as u32);
        if svc.is_none() {
            unicorn.emu_stop()
                .expect("failed to stop emu");
            panic!("svc number: {} not found", number);
        }
        svc.unwrap().on_post_callback(&emulator);
        return;
    }
    else if nr == Syscalls::__NR_io_setup && swi == 0 && unicorn.reg_read(unicorn_engine::RegisterARM64::X16).unwrap() == PRE_CALLBACK_SYSCALL_NUMBER {
        let number = unicorn.reg_read(unicorn_engine::RegisterARM64::X12).unwrap();
        let svc = svc_memory.get_svc(number as u32);
        if svc.is_none() {
            unicorn.emu_stop()
                .expect("failed to stop emu");
            panic!("svc number: {} not found", number);
        }
        svc.unwrap().on_pre_callback(&emulator);
        return;
    }

    if option_env!("PRINT_SYSCALL_TIME_COST") == Some("1") {
        let start = std::time::Instant::now();
        syscall(nr, &emulator.backend, emulator);
        let cost = start.elapsed();
        info!("syscall: {:?} cost: {:?}", nr, cost);
    } else {
        syscall(nr, &emulator.backend, emulator);
    }
}

#[inline]
#[cfg(feature = "dynarmic_backend")]
fn arm64_syscall_handler_dynarmic<T: Clone>(swi: i32, emulator: &AndroidEmulator<T>) {
    let backend = &emulator.backend;
    let nr = get_syscall(backend);
    let svc_memory = &mut emulator.inner_mut().svc_memory;
    if swi != 0 {
        if swi == SWI_MAX {
            panic!("余生很长，何必慌张") // PopContextException
        }
        if swi == SWI_MAX - 1 {
            panic!("完成了很想做的事情之后，真的很轻松啊 ୧⍢⃝୨") // ThreadContextSwitchException
        }
        let svc = svc_memory.get_svc(swi as u32);
        if let Some(svc) = svc {
            match svc.handle(&emulator) {
                SvcCallResult::VOID => {}
                SvcCallResult::FUCK(e) => {
                    error!("svc handle failed: {:?}", e);
                    backend.emu_stop(TaskStatus::X, emulator)
                        .expect("failed to stop emulator");
                }
                SvcCallResult::RET(ret) => {
                    backend.reg_write_i64(RegisterARM64::X0, ret).unwrap();
                }
            }
            return;
        }
        backend.emu_stop(TaskStatus::X, emulator)
            .expect("failed to stop emulator");
        panic!("swi number: {} not found", swi);
    }
    else if nr == Syscalls::__NR_io_setup && swi == 0 && backend.reg_read(RegisterARM64::X16).unwrap() == POST_CALLBACK_SYSCALL_NUMBER {
        let number = backend.reg_read(RegisterARM64::X12).unwrap();
        let svc = svc_memory.get_svc(number as u32);
        if svc.is_none() {
            backend.emu_stop(TaskStatus::X, emulator)
                .expect("failed to stop emu");
            panic!("svc number: {} not found", number);
        }
        svc.unwrap().on_post_callback(&emulator);
        return;
    }
    else if nr == Syscalls::__NR_io_setup && swi == 0 && backend.reg_read(RegisterARM64::X16).unwrap() == PRE_CALLBACK_SYSCALL_NUMBER {
        let number = backend.reg_read(RegisterARM64::X12).unwrap();
        let svc = svc_memory.get_svc(number as u32);
        if svc.is_none() {
            backend.emu_stop(TaskStatus::X, emulator)
                .expect("failed to stop emu");
            panic!("svc number: {} not found", number);
        }
        svc.unwrap().on_pre_callback(&emulator);
        return;
    }

    if option_env!("PRINT_SYSCALL_TIME_COST") == Some("1") {
        let start = std::time::Instant::now();
        syscall(nr, backend, emulator);
        let cost = start.elapsed();
        info!("[dynarmic] syscall: {:?} cost: {:?}", nr, cost);
    } else {
        syscall(nr, backend, emulator);
    }
}

#[inline]
fn syscall<'a, T: Clone>(nr: Syscalls, backend: &Backend<'a, T>, emulator: &AndroidEmulator<'a, T>)  {
    if option_env!("EMU_LOG") == Some("1") {
        info!("syscall: {:?}", nr);
    }
    let _ = match nr {
        Syscalls::__NR_openat => {
            syscalls::syscall_openat(backend, emulator);
        }
        Syscalls::__NR_close => {
            syscalls::syscall_close(backend, emulator);
        }
        Syscalls::__NR_read => {
            syscalls::syscall_read(backend, emulator);
        }
        Syscalls::__NR3264_fstat => {
            syscalls::syscall_fstat(backend, emulator);
        }
        Syscalls::__NR_futex => {
            syscalls::syscall_futex(backend, emulator);
        }
        Syscalls::__NR_clock_gettime => {
            syscalls::syscall_clock_gettime(backend, emulator);
        }
        Syscalls::__NR_prctl => {
            syscalls::syscall_prctl(backend, emulator);
        }
        Syscalls::__NR_gettimeofday => {
            syscalls::syscall_gettimeofday(backend, emulator);
        }
        Syscalls::__NR_brk => {
            syscalls::syscall_brk(backend, emulator);
        }
        Syscalls::__NR_munmap => {
            syscalls::syscall_munmap(backend, emulator);
        }
        Syscalls::__NR3264_mmap => {
            syscalls::syscall_mmap(backend, emulator);
        }
        Syscalls::__NR_mprotect => {
            syscalls::syscall_mprotect(backend, emulator);
        }
        Syscalls::__NR_madvise => {
            syscalls::syscall_madvise(backend, emulator);
        }
        Syscalls::__NR_geteuid => {
            syscalls::syscall_geteuid(backend, emulator);
        }
        Syscalls::__NR_renameat => {
            syscalls::syscall_renameat(backend, emulator);
        }
        Syscalls::__NR3264_fstatat => {
            syscalls::syscall_fstatat(backend, emulator);
        }
        Syscalls::__NR_getppid => {
            syscalls::syscall_getppid(backend, emulator);
        }
        Syscalls::__NR_getpid => {
            syscalls::syscall_getpid(backend, emulator);
        }
        Syscalls::__NR_getuid => {
            syscalls::syscall_getuid(backend, emulator);
        }
        Syscalls::__NR_clone => {
            syscalls::syscall_clone(backend, emulator);
        }
        Syscalls::__NR_sigaltstack => {
            syscalls::syscall_sigaltstack(backend, emulator);
        }
        Syscalls::__NR3264_lseek => {
            syscalls::syscall_lseek(backend, emulator);
        }
        Syscalls::__NR_mkdirat => {
            syscalls::syscall_mkdirat(backend, emulator);
        }
        Syscalls::__NR_set_tid_address => {
            syscalls::syscall_set_tid_address(backend, emulator);
        }
        Syscalls::__NR_rt_sigprocmask => {
            syscalls::syscall_rt_sigprocmask(backend, emulator);
        }
        Syscalls::__NR_exit => {
            syscalls::syscall_exit(backend, emulator);
        }
        Syscalls::__NR_faccessat => {
            syscalls::syscall_faccessat(backend, emulator);
        }
        Syscalls::__NR_getdents64 => {
            syscalls::syscall_getdents64(backend, emulator);
        }
        Syscalls::__NR_write => {
            syscalls::syscall_write(backend, emulator);
        }
        Syscalls::__NR_socket => {
            syscalls::syscall_socket(backend, emulator);
        }
        Syscalls::__NR_connect => {
            syscalls::syscall_connect(backend, emulator);
        }
        Syscalls::__NR_pipe2 => {
            syscalls::syscall_pipe2(backend, emulator);
        }
        _ => {
            info!("Unsupported syscall: {:?}", nr);
            backend.emu_stop(TaskStatus::X, emulator)
                .expect("failed to stop emulator");
            panic!("Unsupported syscall: {:?}", nr);
        }
    };
}

pub(crate) fn register_syscall_handler<T: Clone>(emu: &AndroidEmulator<T>) {
    let emulator = emu.clone();

    #[cfg(feature = "unicorn_backend")]
    if let Backend::Unicorn(unicorn) = &emu.backend {
        unicorn.add_intr_hook(move |backend, intno| {
            if intno == 1 || intno == 2 || intno == 7 {
                let mut swi;
                let pc = backend.reg_read(unicorn_engine::RegisterARM64::PC).unwrap();
                let mut swi_buf = [0u8; 4];
                backend.mem_read(pc - 4, &mut swi_buf).unwrap();
                swi = (i32::from_le_bytes(swi_buf) >> 5) & 0xffff;
                arm64_syscall_handler_unicorn(backend, intno, swi, &emulator);
            } else {
                warn!("Unsupported INTNO: {}!", intno);
            }
        }).expect("failed to add_intr_hook");
        return;
    }

    #[cfg(feature = "dynarmic_backend")]
    if let Backend::Dynarmic(dynarmic) = &emu.backend {
        dynarmic.set_svc_callback(move |backend, swi, until, pc| {
            if until == pc {
                emulator.emu_stop(TaskStatus::X)
                    .expect("failed to stop emulator");
                return;
            }
            unsafe { arm64_syscall_handler_dynarmic(mem::transmute(swi), &emulator) }
        });
        return;
    }

    panic!("Unsupported backend: failed to register syscall handler");
}

pub fn get_syscall<T: Clone>(uc: &Backend<T>) -> Syscalls {
    let syscall = uc.reg_read(RegisterARM64::X8).unwrap();
    unsafe { mem::transmute(syscall) }
}

#[repr(u64)]
#[derive(Debug, PartialEq, Copy, Clone)]
//taken from android kernel's include/uapi/asm-generic/unistd.h
pub enum Syscalls {
    __NR_io_setup	 = 0,
    __NR_io_destroy	 = 1,
    __NR_io_submit	 = 2,
    __NR_io_cancel	 = 3,
    __NR_io_getevents	 = 4,
    __NR_setxattr	 = 5,
    __NR_lsetxattr	 = 6,
    __NR_fsetxattr	 = 7,
    __NR_getxattr	 = 8,
    __NR_lgetxattr	 = 9,
    __NR_fgetxattr	 = 10,
    __NR_listxattr	 = 11,
    __NR_llistxattr	 = 12,
    __NR_flistxattr	 = 13,
    __NR_removexattr	 = 14,
    __NR_lremovexattr	 = 15,
    __NR_fremovexattr	 = 16,
    __NR_getcwd	 = 17,
    __NR_lookup_dcookie	 = 18,
    __NR_eventfd2	 = 19,
    __NR_epoll_create1	 = 20,
    __NR_epoll_ctl	 = 21,
    __NR_epoll_pwait	 = 22,
    __NR_dup	 = 23,
    __NR_dup3	 = 24,
    __NR3264_fcntl	 = 25,
    __NR_inotify_init1	 = 26,
    __NR_inotify_add_watch	 = 27,
    __NR_inotify_rm_watch	 = 28,
    __NR_ioctl	 = 29,
    __NR_ioprio_set	 = 30,
    __NR_ioprio_get	 = 31,
    __NR_flock	 = 32,
    __NR_mknodat	 = 33,
    __NR_mkdirat	 = 34,
    __NR_unlinkat	 = 35,
    __NR_symlinkat	 = 36,
    __NR_linkat	 = 37,
    __NR_renameat	 = 38,
    __NR_umount2	 = 39,
    __NR_mount	 = 40,
    __NR_pivot_root	 = 41,
    __NR_nfsservctl	 = 42,
    __NR3264_statfs	 = 43,
    __NR3264_fstatfs	 = 44,
    __NR3264_truncate	 = 45,
    __NR3264_ftruncate	 = 46,
    __NR_fallocate	 = 47,
    __NR_faccessat	 = 48,
    __NR_chdir	 = 49,
    __NR_fchdir	 = 50,
    __NR_chroot	 = 51,
    __NR_fchmod	 = 52,
    __NR_fchmodat	 = 53,
    __NR_fchownat	 = 54,
    __NR_fchown	 = 55,
    __NR_openat	 = 56,
    __NR_close	 = 57,
    __NR_vhangup	 = 58,
    __NR_pipe2	 = 59,
    __NR_quotactl	 = 60,
    __NR_getdents64	 = 61,
    __NR3264_lseek	 = 62,
    __NR_read	 = 63,
    __NR_write	 = 64,
    __NR_readv	 = 65,
    __NR_writev	 = 66,
    __NR_pread64	 = 67,
    __NR_pwrite64	 = 68,
    __NR_preadv	 = 69,
    __NR_pwritev	 = 70,
    __NR3264_sendfile	 = 71,
    __NR_pselect6	 = 72,
    __NR_ppoll	 = 73,
    __NR_signalfd4	 = 74,
    __NR_vmsplice	 = 75,
    __NR_splice	 = 76,
    __NR_tee	 = 77,
    __NR_readlinkat	 = 78,
    __NR3264_fstatat	 = 79,
    __NR3264_fstat	 = 80,
    __NR_sync	 = 81,
    __NR_fsync	 = 82,
    __NR_fdatasync	 = 83,
    __NR_sync_file_range2	 = 84,
    // __NR_sync_file_range	 = 84,
    __NR_timerfd_create	 = 85,
    __NR_timerfd_settime	 = 86,
    __NR_timerfd_gettime	 = 87,
    __NR_utimensat	 = 88,
    __NR_acct	 = 89,
    __NR_capget	 = 90,
    __NR_capset	 = 91,
    __NR_personality	 = 92,
    __NR_exit	 = 93,
    __NR_exit_group	 = 94,
    __NR_waitid	 = 95,
    __NR_set_tid_address	 = 96,
    __NR_unshare	 = 97,
    __NR_futex	 = 98,
    __NR_set_robust_list	 = 99,
    __NR_get_robust_list	 = 100,
    __NR_nanosleep	 = 101,
    __NR_getitimer	 = 102,
    __NR_setitimer	 = 103,
    __NR_kexec_load	 = 104,
    __NR_init_module	 = 105,
    __NR_delete_module	 = 106,
    __NR_timer_create	 = 107,
    __NR_timer_gettime	 = 108,
    __NR_timer_getoverrun	 = 109,
    __NR_timer_settime	 = 110,
    __NR_timer_delete	 = 111,
    __NR_clock_settime	 = 112,
    __NR_clock_gettime	 = 113,
    __NR_clock_getres	 = 114,
    __NR_clock_nanosleep	 = 115,
    __NR_syslog	 = 116,
    __NR_ptrace	 = 117,
    __NR_sched_setparam	 = 118,
    __NR_sched_setscheduler	 = 119,
    __NR_sched_getscheduler	 = 120,
    __NR_sched_getparam	 = 121,
    __NR_sched_setaffinity	 = 122,
    __NR_sched_getaffinity	 = 123,
    __NR_sched_yield	 = 124,
    __NR_sched_get_priority_max	 = 125,
    __NR_sched_get_priority_min	 = 126,
    __NR_sched_rr_get_interval	 = 127,
    __NR_restart_syscall	 = 128,
    __NR_kill	 = 129,
    __NR_tkill	 = 130,
    __NR_tgkill	 = 131,
    __NR_sigaltstack	 = 132,
    __NR_rt_sigsuspend	 = 133,
    __NR_rt_sigaction	 = 134,
    __NR_rt_sigprocmask	 = 135,
    __NR_rt_sigpending	 = 136,
    __NR_rt_sigtimedwait	 = 137,
    __NR_rt_sigqueueinfo	 = 138,
    __NR_rt_sigreturn	 = 139,
    __NR_setpriority	 = 140,
    __NR_getpriority	 = 141,
    __NR_reboot	 = 142,
    __NR_setregid	 = 143,
    __NR_setgid	 = 144,
    __NR_setreuid	 = 145,
    __NR_setuid	 = 146,
    __NR_setresuid	 = 147,
    __NR_getresuid	 = 148,
    __NR_setresgid	 = 149,
    __NR_getresgid	 = 150,
    __NR_setfsuid	 = 151,
    __NR_setfsgid	 = 152,
    __NR_times	 = 153,
    __NR_setpgid	 = 154,
    __NR_getpgid	 = 155,
    __NR_getsid	 = 156,
    __NR_setsid	 = 157,
    __NR_getgroups	 = 158,
    __NR_setgroups	 = 159,
    __NR_uname	 = 160,
    __NR_sethostname	 = 161,
    __NR_setdomainname	 = 162,
    __NR_getrlimit	 = 163,
    __NR_setrlimit	 = 164,
    __NR_getrusage	 = 165,
    __NR_umask	 = 166,
    __NR_prctl	 = 167,
    __NR_getcpu	 = 168,
    __NR_gettimeofday	 = 169,
    __NR_settimeofday	 = 170,
    __NR_adjtimex	 = 171,
    __NR_getpid	 = 172,
    __NR_getppid	 = 173,
    __NR_getuid	 = 174,
    __NR_geteuid	 = 175,
    __NR_getgid	 = 176,
    __NR_getegid	 = 177,
    __NR_gettid	 = 178,
    __NR_sysinfo	 = 179,
    __NR_mq_open	 = 180,
    __NR_mq_unlink	 = 181,
    __NR_mq_timedsend	 = 182,
    __NR_mq_timedreceive	 = 183,
    __NR_mq_notify	 = 184,
    __NR_mq_getsetattr	 = 185,
    __NR_msgget	 = 186,
    __NR_msgctl	 = 187,
    __NR_msgrcv	 = 188,
    __NR_msgsnd	 = 189,
    __NR_semget	 = 190,
    __NR_semctl	 = 191,
    __NR_semtimedop	 = 192,
    __NR_semop	 = 193,
    __NR_shmget	 = 194,
    __NR_shmctl	 = 195,
    __NR_shmat	 = 196,
    __NR_shmdt	 = 197,
    __NR_socket	 = 198,
    __NR_socketpair	 = 199,
    __NR_bind	 = 200,
    __NR_listen	 = 201,
    __NR_accept	 = 202,
    __NR_connect	 = 203,
    __NR_getsockname	 = 204,
    __NR_getpeername	 = 205,
    __NR_sendto	 = 206,
    __NR_recvfrom	 = 207,
    __NR_setsockopt	 = 208,
    __NR_getsockopt	 = 209,
    __NR_shutdown	 = 210,
    __NR_sendmsg	 = 211,
    __NR_recvmsg	 = 212,
    __NR_readahead	 = 213,
    __NR_brk	 = 214,
    __NR_munmap	 = 215,
    __NR_mremap	 = 216,
    __NR_add_key	 = 217,
    __NR_request_key	 = 218,
    __NR_keyctl	 = 219,
    __NR_clone	 = 220,
    __NR_execve	 = 221,
    __NR3264_mmap	 = 222,
    __NR3264_fadvise64	 = 223,
    __NR_swapon	 = 224,
    __NR_swapoff	 = 225,
    __NR_mprotect	 = 226,
    __NR_msync	 = 227,
    __NR_mlock	 = 228,
    __NR_munlock	 = 229,
    __NR_mlockall	 = 230,
    __NR_munlockall	 = 231,
    __NR_mincore	 = 232,
    __NR_madvise	 = 233,
    __NR_remap_file_pages	 = 234,
    __NR_mbind	 = 235,
    __NR_get_mempolicy	 = 236,
    __NR_set_mempolicy	 = 237,
    __NR_migrate_pages	 = 238,
    __NR_move_pages	 = 239,
    __NR_rt_tgsigqueueinfo	 = 240,
    __NR_perf_event_open	 = 241,
    __NR_accept4	 = 242,
    __NR_recvmmsg	 = 243,
    __NR_arch_specific_syscall	 = 244,
    __NR_wait4	 = 260,
    __NR_prlimit64	 = 261,
    __NR_fanotify_init	 = 262,
    __NR_fanotify_mark	 = 263,
    __NR_name_to_handle_at	 = 264,
    __NR_open_by_handle_at	 =  265,
    __NR_clock_adjtime	 = 266,
    __NR_syncfs	 = 267,
    __NR_setns	 = 268,
    __NR_sendmmsg	 = 269,
    __NR_process_vm_readv	 = 270,
    __NR_process_vm_writev	 = 271,
    __NR_kcmp	 = 272,
    __NR_finit_module	 = 273,
    __NR_sched_setattr	 = 274,
    __NR_sched_getattr	 = 275,
    __NR_renameat2	 = 276,
    __NR_seccomp	 = 277,
    __NR_getrandom	 = 278,
    __NR_memfd_create	 = 279,
    __NR_bpf	 = 280,
    __NR_execveat	 = 281,
    __NR_userfaultfd	 = 282,
    __NR_membarrier	 = 283,
    __NR_mlock2	 = 284,
    __NR_copy_file_range	 = 285,
    __NR_preadv2	 = 286,
    __NR_pwritev2	 = 287,
    __NR_pkey_mprotect	 = 288,
    __NR_pkey_alloc	 = 289,
    __NR_pkey_free	 = 290,
    __NR_statx	 = 291,
    __NR_syscalls	 = 292,
    __NR_open	 = 1024,
    __NR_link	 = 1025,
    __NR_unlink	 = 1026,
    __NR_mknod	 = 1027,
    __NR_chmod	 = 1028,
    __NR_chown	 = 1029,
    __NR_mkdir	 = 1030,
    __NR_rmdir	 = 1031,
    __NR_lchown	 = 1032,
    __NR_access	 = 1033,
    __NR_rename	 = 1034,
    __NR_readlink	 = 1035,
    __NR_symlink	 = 1036,
    __NR_utimes	 = 1037,
    __NR3264_stat	 = 1038,
    __NR3264_lstat	 = 1039,
    // __NR_syscalls	 = (__NR3264_lstat+1),
    __NR_pipe	 = 1040,
    __NR_dup2	 = 1041,
    __NR_epoll_create	 = 1042,
    __NR_inotify_init	 = 1043,
    __NR_eventfd	 = 1044,
    __NR_signalfd	 = 1045,
    // __NR_syscalls	 = (__NR_signalfd+1),
    __NR_sendfile	 = 1046,
    __NR_ftruncate	 = 1047,
    __NR_truncate	 = 1048,
    __NR_stat	 = 1049,
    __NR_lstat	 = 1050,
    __NR_fstat	 = 1051,
    __NR_fcntl	 = 1052,
    __NR_fadvise64	 = 1053,
    __NR_newfstatat	 = 1054,
    __NR_fstatfs	 = 1055,
    __NR_statfs	 = 1056,
    __NR_lseek	 = 1057,
    __NR_mmap	 = 1058,
    // __NR_syscalls	 = (__NR_mmap+1),
    __NR_alarm	 = 1059,
    __NR_getpgrp	 = 1060,
    __NR_pause	 = 1061,
    __NR_time	 = 1062,
    __NR_utime	 = 1063,
    __NR_creat	 = 1064,
    __NR_getdents	 = 1065,
    __NR_futimesat	 = 1066,
    __NR_select	 = 1067,
    __NR_poll	 = 1068,
    __NR_epoll_wait	 = 1069,
    __NR_ustat	 = 1070,
    __NR_vfork	 = 1071,
    __NR_oldwait4	 = 1072,
    __NR_recv	 = 1073,
    __NR_send	 = 1074,
    __NR_bdflush	 = 1075,
    __NR_umount	 = 1076,
    __NR_uselib	 = 1077,
    __NR__sysctl	 = 1078,
    __NR_fork	 = 1079,
    // __NR_syscalls	 = (__NR_fork+1),
    // __NR_fcntl	 = __NR3264_fcntl,
    // __NR_statfs	 = __NR3264_statfs,
    // __NR_fstatfs	 = __NR3264_fstatfs,
    // __NR_truncate	 = __NR3264_truncate,
    // __NR_ftruncate	 = __NR3264_ftruncate,
    // __NR_lseek	 = __NR3264_lseek,
    // __NR_sendfile	 = __NR3264_sendfile,
    // __NR_newfstatat	 = __NR3264_fstatat,
    // __NR_fstat	 = __NR3264_fstat,
    // __NR_mmap	 = __NR3264_mmap,
    // __NR_fadvise64	 = __NR3264_fadvise64,
    // __NR_stat	 = __NR3264_stat,
    // __NR_lstat	 = __NR3264_lstat,
    // __NR_fcntl64	 = __NR3264_fcntl,
    // __NR_statfs64	 = __NR3264_statfs,
    // __NR_fstatfs64	 = __NR3264_fstatfs,
    // __NR_truncate64	 = __NR3264_truncate,
    // __NR_ftruncate64	 = __NR3264_ftruncate,
    // __NR_llseek	 = __NR3264_lseek,
    // __NR_sendfile64	 = __NR3264_sendfile,
    // __NR_fstatat64	 = __NR3264_fstatat,
    // __NR_fstat64	 = __NR3264_fstat,
    // __NR_mmap2	 = __NR3264_mmap,
    // __NR_fadvise64_64	 = __NR3264_fadvise64,
    // __NR_stat64	 = __NR3264_stat,
    // __NR_lstat64	 = __NR3264_lstat,

    None,
}