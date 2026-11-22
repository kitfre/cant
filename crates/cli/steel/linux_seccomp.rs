//! Linux syscall filter, installed after engine initialization.
use crate::process_setup::checked;
use std::io;

pub fn install() -> io::Result<()> {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc00000b7;
    fn instruction(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }
    // seccomp_data offsets: nr=0, arch=4. Wrong ABI kills the process; unlisted syscalls fail EPERM.
    let mut filter = vec![
        instruction(0x20, 0, 0, 4),
        instruction(0x15, 1, 0, ARCH),
        instruction(0x06, 0, 0, 0x80000000),
        instruction(0x20, 0, 0, 0),
    ];
    for syscall in [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_lseek,
        libc::SYS_brk,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_clock_gettime,
        libc::SYS_gettimeofday,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getcwd,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_sched_yield,
        libc::SYS_getrandom,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_restart_syscall,
    ] {
        filter.push(instruction(0x15, 0, 1, syscall as u32));
        filter.push(instruction(0x06, 0, 0, 0x7fff0000));
    }
    filter.push(instruction(0x06, 0, 0, 0x00050000 | libc::EPERM as u32));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    unsafe {
        checked(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
        // Synchronize across any threads initialized by the engine before guest evaluation.
        let result = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &program,
        );
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result != 0 {
            return Err(io::Error::other(
                "Could not synchronize seccomp across runner threads",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn seccomp_covers_every_thread() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "linux_seccomp::tests::kernel_probe",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    #[test]
    #[ignore]
    fn kernel_probe() {
        use std::{sync::mpsc, thread};
        let (send, receive) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready.send(()).unwrap();
            receive.recv().unwrap();
            assert_eq!(
                std::fs::read("/dev/null").unwrap_err().raw_os_error(),
                Some(libc::EPERM)
            );
        });
        // TSYNC must cover an initialized worker, not race glibc's thread-start rseq setup.
        started.recv().unwrap();
        crate::process_setup::resource_limits().unwrap();
        super::install().unwrap();
        send.send(()).unwrap();
        assert_eq!(
            std::fs::read("/dev/null").unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        unsafe {
            assert_eq!(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0), -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        }
        assert_eq!(
            std::process::Command::new("/usr/bin/true")
                .status()
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        worker.join().unwrap();
        std::process::exit(0);
    }
}
