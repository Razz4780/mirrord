use std::{ffi::c_ulong, os::fd::RawFd};

use libseccomp::ScmpFd;
use nix::{
    errno::Errno,
    libc::{self, seccomp_notif_addfd},
    Result,
};

const SECCOMP_ADDFD_FLAG_SETFD: u32 = 1;
const SECCOMP_ADDFD_FLAG_SEND: u32 = 2;
const SECCOMP_IOCTL_NOTIF_ADDFD: c_ulong = 1075323139;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScmpNotifAddfd {
    id: u64,
    flags: u32,
    srcfd: RawFd,
    newfd: Option<RawFd>,
    newfd_flags: i32,
}

impl ScmpNotifAddfd {
    fn to_sys(self) -> seccomp_notif_addfd {
        seccomp_notif_addfd {
            id: self.id,
            flags: self.flags,
            srcfd: self.srcfd as _,
            newfd: self.newfd.unwrap_or(0) as _,
            newfd_flags: self.newfd_flags as _,
        }
    }

    pub fn new(
        id: u64,
        srcfd: RawFd,
        newfd: Option<RawFd>,
        send: bool,
        close_on_exec: bool,
    ) -> Self {
        let flags = newfd
            .is_some()
            .then_some(SECCOMP_ADDFD_FLAG_SETFD)
            .unwrap_or_default()
            | send.then_some(SECCOMP_ADDFD_FLAG_SEND).unwrap_or_default();
        let newfd_flags = close_on_exec.then_some(libc::O_CLOEXEC).unwrap_or_default();

        Self {
            id,
            flags,
            srcfd,
            newfd,
            newfd_flags,
        }
    }

    pub fn respond(&self, fd: ScmpFd) -> Result<()> {
        let mut addfd = self.to_sys();

        let res = unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_ADDFD, &mut addfd as *mut _) };
        if res < 0 {
            Err(Errno::last())
        } else {
            Ok(())
        }
    }
}
