use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use nix::{errno::Errno, libc, sys::eventfd::EfdFlags, unistd, Result};

#[derive(Debug)]
pub struct Semaphore {
    event_fd: OwnedFd,
}

impl Semaphore {
    pub fn new() -> Result<Self> {
        let flags = EfdFlags::EFD_CLOEXEC
            .union(EfdFlags::EFD_SEMAPHORE)
            .union(EfdFlags::EFD_NONBLOCK);

        let res = unsafe { libc::eventfd(0, flags.bits()) };
        let event_fd = unsafe { OwnedFd::from_raw_fd(Errno::result(res)?) };

        Ok(Self { event_fd })
    }

    pub fn try_clone(&self) -> Result<Self> {
        let event_fd = self.event_fd.try_clone().map_err(|_| Errno::last())?;

        Ok(Self { event_fd })
    }

    pub fn add_permit(&mut self) -> Result<()> {
        unistd::write(self.event_fd.as_fd(), &1_u64.to_ne_bytes())?;

        Ok(())
    }

    pub fn take_permit(&mut self) -> Result<()> {
        let mut arr = [0; std::mem::size_of::<u64>()];
        unistd::read(self.event_fd.as_raw_fd(), &mut arr)?;

        Ok(())
    }
}

impl AsFd for Semaphore {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.event_fd.as_fd()
    }
}

#[cfg(test)]
mod test {
    use std::{panic, process, thread, time::Duration};

    use nix::{
        poll::{self, PollFd, PollFlags, PollTimeout},
        sys::{
            prctl,
            signal::Signal,
            wait::{self, WaitPidFlag, WaitStatus},
        },
        unistd::ForkResult,
    };

    use super::*;

    #[test]
    fn semaphore_test_fork() {
        let mut semaphore = Semaphore::new().unwrap();

        let fork_result = unsafe { unistd::fork() };

        match fork_result.unwrap() {
            ForkResult::Child => {
                panic::set_hook(Box::new(|panic_info| {
                    eprintln!("{panic_info:?}");
                    process::exit(-1);
                }));

                prctl::set_pdeathsig(Signal::SIGKILL).unwrap();

                let mut pollfds = [PollFd::new(semaphore.as_fd(), PollFlags::POLLIN)];
                poll::poll(&mut pollfds, PollTimeout::NONE).unwrap();
                assert!(pollfds[0].revents().unwrap().contains(PollFlags::POLLIN));
                semaphore.take_permit().unwrap();

                process::exit(0);
            }

            ForkResult::Parent { child } => {
                thread::sleep(Duration::from_millis(100));
                assert_eq!(
                    wait::waitpid(child, Some(WaitPidFlag::WNOHANG)),
                    Ok(WaitStatus::StillAlive),
                );

                semaphore.add_permit().unwrap();
                assert_eq!(wait::waitpid(child, None), Ok(WaitStatus::Exited(child, 0)),);
            }
        }
    }

    #[test]
    fn semaphore_test_thread() {
        let mut semaphore = Semaphore::new().unwrap();
        let mut semaphore_clone = semaphore.try_clone().unwrap();
        let handle = thread::spawn(move || {
            let mut pollfds = [PollFd::new(semaphore_clone.as_fd(), PollFlags::POLLIN)];
            poll::poll(&mut pollfds, PollTimeout::NONE).unwrap();
            assert!(pollfds[0].revents().unwrap().contains(PollFlags::POLLIN));
            semaphore_clone.take_permit().unwrap();
        });

        thread::sleep(Duration::from_millis(100));
        assert!(!handle.is_finished());

        semaphore.add_permit().unwrap();
        handle.join().unwrap();
    }
}
