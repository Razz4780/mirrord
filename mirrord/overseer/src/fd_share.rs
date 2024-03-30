use std::{
    io::{IoSlice, IoSliceMut},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

use nix::{
    errno::Errno,
    fcntl::{self, FcntlArg},
    sys::socket::{
        self, AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType,
    },
};
use thiserror::Error;

/// Errors that can occur when using [`FdShare`] channel.
#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to execute IO on the UNIX socket: {0}")]
    SocketIo(Errno),
    #[error("failed to check received file descriptor for validity: {0}")]
    FdCheck(Errno),
    #[error("received a message with no file descriptor")]
    NoFd,
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A channel for sending file descriptors between processes.
/// Utilizes a UNIX domain datagram socket and `SCM_RIGHTS` ancillary data.
///
/// File descriptors may be sent using [`Self::send`] and received using [`Self::receive`].
/// Order is preserved, as this type of socket does not reorder datagrams.
pub struct FdShare {
    sender: OwnedFd,
    receiver: OwnedFd,
}

impl FdShare {
    /// Creates a new instance of this channel.
    pub fn new() -> Result<Self> {
        let (sender, receiver) = socket::socketpair(
            AddressFamily::Unix,
            SockType::Datagram,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .map_err(Error::SocketIo)?;

        let (sender, receiver) =
            unsafe { (OwnedFd::from_raw_fd(sender), OwnedFd::from_raw_fd(receiver)) };

        Ok(Self { sender, receiver })
    }

    /// Sends the given file descriptor in ancillary data.
    /// This descriptor can be received with [`Self::receive`].
    ///
    /// # Note
    ///
    /// One byte of real data is sent as well, basing on [`unix` manual](https://man7.org/linux/man-pages/man7/unix.7.html):
    ///
    /// "When sending ancillary data over a UNIX domain datagram socket, it is not necessary
    /// on Linux to send any accompanying real data.  However, portable applications should
    /// also include at least one byte of real data when sending ancillary data over a
    /// datagram socket."
    pub fn send(&mut self, fd: RawFd) -> Result<()> {
        socket::sendmsg::<()>(
            self.sender.as_raw_fd(),
            &[IoSlice::new(b"0")],
            &[ControlMessage::ScmRights(&[fd])],
            MsgFlags::empty(),
            None,
        )
        .map_err(Error::SocketIo)?;

        Ok(())
    }

    /// Receives a file descriptor.
    ///
    /// # Note
    ///
    /// The file descriptor is checked for validity with `fcntl(fd, F_GETFD)`, basing on [`unix` manual](https://man7.org/linux/man-pages/man7/unix.7.html):
    ///
    /// "If the number of file descriptors received in the
    /// ancillary data would cause the process to exceed its
    /// RLIMIT_NOFILE resource limit (see getrlimit(2)), the
    /// excess file descriptors are automatically closed in the
    /// receiving process."
    pub fn receive(&mut self) -> Result<OwnedFd> {
        let mut buffer = [0_u8; 4];
        let mut io_slices = [IoSliceMut::new(buffer.as_mut())];
        let mut buffer = nix::cmsg_space!(RawFd);
        let recv_result = socket::recvmsg::<()>(
            self.receiver.as_raw_fd(),
            &mut io_slices,
            Some(&mut buffer),
            MsgFlags::empty(),
        )
        .map_err(Error::SocketIo)?;

        let fd = recv_result
            .cmsgs()
            .into_iter()
            .find_map(|msg| match msg {
                ControlMessageOwned::ScmRights(fds) => fds.into_iter().next(),
                _ => None,
            })
            .ok_or(Error::NoFd)?;

        fcntl::fcntl(fd, FcntlArg::F_GETFD).map_err(Error::FdCheck)?;

        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        Ok(fd)
    }
}

#[cfg(test)]
mod test {
    use std::{
        ffi::CString,
        fs::File,
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd},
    };

    use nix::{
        sys::{
            memfd::{self, MemFdCreateFlag},
            wait::{self, WaitStatus},
        },
        unistd::{self, ForkResult},
    };

    use super::FdShare;

    #[test]
    fn fd_send_test() {
        let mut fd_share = FdShare::new().unwrap();
        let fork_result = unsafe { unistd::fork() };

        match fork_result.unwrap() {
            ForkResult::Child => {
                let fd = fd_share.receive().unwrap();
                let mut file = File::from(fd);
                let mut buf = String::new();
                file.read_to_string(&mut buf).unwrap();
                assert_eq!(buf, "lorem ipsum");
            }

            ForkResult::Parent { child } => {
                let mut file = {
                    let name = CString::new("test-file").unwrap();
                    let fd = memfd::memfd_create(&name, MemFdCreateFlag::MFD_CLOEXEC).unwrap();
                    unsafe { File::from_raw_fd(fd) }
                };

                file.write_all(b"lorem ipsum").unwrap();
                fd_share.send(file.as_raw_fd()).unwrap();

                match wait::waitpid(child, None) {
                    Ok(WaitStatus::Exited(pid, 0)) if pid == child => {}
                    other => panic!("unexpected waitpid result: {other:?}"),
                }
            }
        }
    }
}
