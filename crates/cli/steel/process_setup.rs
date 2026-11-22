//! Unix startup for the Steel child process.
use std::{fs::File, io, os::fd::FromRawFd};

pub(crate) fn checked(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

pub fn private_protocol() -> io::Result<(File, File)> {
    // Steel's standard ports must never reference the JSONL transport. Close-on-exec also prevents
    // accidental inheritance should initialization acquire subprocess functionality in the future.
    unsafe {
        let input = File::from_raw_fd(checked(libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3))?);
        let output = File::from_raw_fd(checked(libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3))?);
        let null = File::open("/dev/null")?;
        use std::os::fd::AsRawFd;
        checked(libc::dup2(null.as_raw_fd(), 0))?;
        let sink = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
        checked(libc::dup2(sink.as_raw_fd(), 1))?;
        checked(libc::dup2(sink.as_raw_fd(), 2))?;
        Ok((input, output))
    }
}

pub fn resource_limits() -> io::Result<()> {
    for (resource, limit) in [
        #[cfg(target_os = "linux")] // Darwin aliases RLIMIT_AS to RLIMIT_RSS.
        (libc::RLIMIT_AS, 512 * 1024 * 1024),
        #[cfg(target_os = "linux")]
        (libc::RLIMIT_STACK, 16 * 1024 * 1024),
        (libc::RLIMIT_CORE, 0),
        (libc::RLIMIT_CPU, 120),
    ] {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        unsafe {
            checked(libc::getrlimit(resource, &mut limits))?;
            limits.rlim_cur = limits.rlim_cur.min(limit);
            limits.rlim_max = limits.rlim_max.min(limit);
            checked(libc::setrlimit(resource, &limits))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod limit_tests {
    #[test]
    fn accepts_stricter_inherited_limits() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_setup::limit_tests::cpu_probe",
                "--ignored",
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
    #[test]
    #[ignore]
    fn cpu_probe() {
        let mut cap = libc::rlimit {
            rlim_cur: 5,
            rlim_max: 5,
        };
        unsafe {
            assert_eq!(libc::setrlimit(libc::RLIMIT_CPU, &cap), 0);
        }
        super::resource_limits().unwrap();
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_CPU, &mut cap), 0);
        }
        assert_eq!(cap.rlim_cur, 5);
    }
}
