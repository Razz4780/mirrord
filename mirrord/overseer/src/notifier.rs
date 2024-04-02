use std::{
    ops::{ControlFlow, Deref},
    os::fd::{AsRawFd, OwnedFd},
};

use libseccomp::{
    error::{SeccompErrno, SeccompError},
    ScmpNotifReq, ScmpNotifResp, ScmpNotifRespFlags,
};
use nix::{
    errno::Errno,
    sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout},
};
use thiserror::Error;
use tokio::sync::mpsc::{self, Receiver, Sender};

use self::semaphore::Semaphore;
use crate::notifier::addfd::ScmpNotifAddfd;

mod addfd;
mod semaphore;

#[derive(Debug)]
pub struct Action {
    pub notification: Notification,
    pub action: ActionInner,
}

#[derive(Debug)]
pub enum ActionInner {
    LetThrough,
    ReturnFd(OwnedFd),
    ReturnValue(i64),
    ReturnError(i32),
}

#[derive(Debug)]
pub struct Notification(ScmpNotifReq);

impl Deref for Notification {
    type Target = ScmpNotifReq;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("receiving seccomp notification failed: {0}")]
    SeccompRecv(SeccompError),
    #[error("action channel semaphore operation failed: {0}")]
    Semaphore(Errno),
    #[error("epoll operation failed: {0}")]
    Epoll(Errno),
    #[error("failed to send action to the notifier thread: channel closed")]
    ActionSend,
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

pub struct Notifier {
    seccomp_unotify_fd: OwnedFd,
    notification_tx: Sender<Notification>,
    semaphore: Semaphore,
    epoll: Epoll,
    action_rx: Receiver<Action>,
}

impl Notifier {
    const CONTROL_CHANNEL_CAPACITY: usize = 512;

    const EPOLL_SEMAPHORE_DATA: u64 = 1;
    const EPOLL_SECCOMP_UNOTIFY_DATA: u64 = 2;

    pub fn new(seccomp_unotify_fd: OwnedFd) -> Result<(Self, NotifierHandle)> {
        let semaphores = {
            let semaphore = Semaphore::new().map_err(Error::Semaphore)?;
            let semamphore_clone = semaphore.try_clone().map_err(Error::Semaphore)?;

            (semaphore, semamphore_clone)
        };

        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).map_err(Error::Epoll)?;
        epoll
            .add(
                &semaphores.0,
                EpollEvent::new(EpollFlags::EPOLLIN, Self::EPOLL_SEMAPHORE_DATA),
            )
            .map_err(Error::Epoll)?;
        epoll
            .add(
                &seccomp_unotify_fd,
                EpollEvent::new(
                    EpollFlags::EPOLLIN.union(EpollFlags::EPOLLHUP),
                    Self::EPOLL_SECCOMP_UNOTIFY_DATA,
                ),
            )
            .map_err(Error::Epoll)?;

        let (notification_tx, notification_rx) = mpsc::channel(Self::CONTROL_CHANNEL_CAPACITY);
        let (action_tx, action_rx) = mpsc::channel(Self::CONTROL_CHANNEL_CAPACITY);

        let handle = NotifierHandle {
            notification_rx,
            action_tx,
            semaphore: semaphores.1,
        };

        let notifier = Self {
            seccomp_unotify_fd,
            notification_tx,
            semaphore: semaphores.0,
            epoll,
            action_rx,
        };

        Ok((notifier, handle))
    }

    pub fn handle_semaphore_ready(&mut self, events: EpollFlags) -> ControlFlow<Result<()>> {
        if events.contains(EpollFlags::EPOLLERR) {
            panic!("semaphore counter value overflow");
        }

        if !events.contains(EpollFlags::EPOLLIN) {
            return ControlFlow::Continue(());
        }

        loop {
            let action = match self.semaphore.take_permit() {
                Ok(()) => self.action_rx.blocking_recv(),
                Err(Errno::EAGAIN) => break ControlFlow::Continue(()),
                Err(e) => break ControlFlow::Break(Err(Error::Semaphore(e))),
            };

            let Some(action) = action else {
                break ControlFlow::Break(Ok(()));
            };

            eprintln!("handling action {action:?}");
            let result = match action.action {
                ActionInner::LetThrough => {
                    ScmpNotifResp::new_continue(action.notification.id, ScmpNotifRespFlags::empty())
                        .respond(self.seccomp_unotify_fd.as_raw_fd())
                        .map_err(|e| e.to_string())
                }
                ActionInner::ReturnFd(fd) => {
                    ScmpNotifAddfd::new(action.notification.id, fd.as_raw_fd(), None, true, true)
                        .respond(self.seccomp_unotify_fd.as_raw_fd())
                        .map_err(|e| e.to_string())
                }
                ActionInner::ReturnValue(val) => {
                    ScmpNotifResp::new_val(action.notification.id, val, ScmpNotifRespFlags::empty())
                        .respond(self.seccomp_unotify_fd.as_raw_fd())
                        .map_err(|e| e.to_string())
                }
                ActionInner::ReturnError(error) => ScmpNotifResp::new_error(
                    action.notification.id,
                    -error,
                    ScmpNotifRespFlags::empty(),
                )
                .respond(self.seccomp_unotify_fd.as_raw_fd())
                .map_err(|e| e.to_string()),
            };

            eprintln!("handling action result: {result:?}");
        }
    }

    pub fn handle_seccomp_unotify_ready(&mut self, events: EpollFlags) -> ControlFlow<Result<()>> {
        if events.contains(EpollFlags::EPOLLHUP) {
            return ControlFlow::Break(Ok(()));
        }

        if !events.contains(EpollFlags::EPOLLIN) {
            return ControlFlow::Continue(());
        }

        let raw = match ScmpNotifReq::receive(self.seccomp_unotify_fd.as_raw_fd()) {
            Ok(raw) => raw,
            Err(error) if error.errno() == Some(SeccompErrno::ENOENT) => {
                return ControlFlow::Continue(())
            }
            Err(error) => return ControlFlow::Break(Err(Error::SeccompRecv(error))),
        };

        if self
            .notification_tx
            .blocking_send(Notification(raw))
            .is_err()
        {
            return ControlFlow::Break(Ok(()));
        }

        ControlFlow::Continue(())
    }

    fn tick(&mut self) -> ControlFlow<Result<()>> {
        let mut events = [EpollEvent::empty(), EpollEvent::empty()];
        let epoll_res = self.epoll.wait(&mut events, EpollTimeout::NONE);
        let ready = match epoll_res {
            Ok(ready) => ready,
            Err(Errno::EINTR) => return ControlFlow::Continue(()),
            Err(error) => return ControlFlow::Break(Err(Error::Epoll(error))),
        };

        for i in 0..ready {
            match events[i].data() {
                Self::EPOLL_SECCOMP_UNOTIFY_DATA => {
                    eprintln!("unotify ready");
                    self.handle_seccomp_unotify_ready(events[i].events())?
                }
                Self::EPOLL_SEMAPHORE_DATA => {
                    eprintln!("semaphore ready");
                    self.handle_semaphore_ready(events[i].events())?
                }
                other => panic!("unknown data found inside epoll_event after epoll: {other}"),
            }
        }

        ControlFlow::Continue(())
    }

    pub fn run(mut self) -> Result<()> {
        loop {
            if let ControlFlow::Break(res) = self.tick() {
                break res;
            }
        }
    }
}

pub struct NotifierHandle {
    notification_rx: Receiver<Notification>,
    action_tx: Sender<Action>,
    semaphore: Semaphore,
}

impl NotifierHandle {
    pub async fn receive(&mut self) -> Option<Notification> {
        self.notification_rx.recv().await
    }

    pub async fn respond(&mut self, notification: Notification, action: ActionInner) -> Result<()> {
        self.semaphore.add_permit().map_err(Error::Semaphore)?;
        self.action_tx
            .send(Action {
                notification,
                action,
            })
            .await
            .map_err(|_| Error::ActionSend)?;

        Ok(())
    }
}

impl Drop for NotifierHandle {
    fn drop(&mut self) {
        let _ = self.semaphore.add_permit();
    }
}

#[cfg(test)]
mod test {
    use std::process;

    use libseccomp::{ScmpAction, ScmpArch, ScmpFilterContext, ScmpSyscall};
    use nix::{
        sys::{
            prctl,
            wait::{self, WaitStatus},
        },
        unistd::{self, ForkResult, Pid},
    };
    use tokio::{runtime::Builder, task};

    use super::*;
    use crate::fd_share;

    #[test]
    fn notifier_fork_test() {
        let (mut fd_tx, mut fd_rx) = fd_share::channel().unwrap();

        let fork_result = unsafe { unistd::fork().unwrap() };
        match fork_result {
            ForkResult::Parent { child } => {
                let seccomp_unotify_fd = fd_rx.receive().unwrap();

                Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        let (notifier, mut handle) = Notifier::new(seccomp_unotify_fd).unwrap();
                        let notifier_task = task::spawn_blocking(move || notifier.run());
                        let waitpid_task = task::spawn_blocking(move || wait::waitpid(child, None));

                        let notification = handle.receive().await.unwrap();
                        assert_eq!(notification.data.syscall, ScmpSyscall::new("getpid"));
                        assert_eq!(notification.pid, child.as_raw() as u32);
                        handle
                            .respond(notification, ActionInner::ReturnValue(0))
                            .await
                            .unwrap();

                        let notification = handle.receive().await.unwrap();
                        assert_eq!(notification.data.syscall, ScmpSyscall::new("getpid"));
                        assert_eq!(notification.pid, child.as_raw() as u32);
                        handle
                            .respond(notification, ActionInner::ReturnValue(1))
                            .await
                            .unwrap();

                        assert_eq!(
                            waitpid_task.await.unwrap(),
                            Ok(WaitStatus::Exited(child, 0)),
                        );

                        notifier_task.await.unwrap().unwrap();
                    });
            }

            ForkResult::Child => {
                prctl::set_no_new_privs().unwrap();

                let mut filter = ScmpFilterContext::new_filter(ScmpAction::Allow).unwrap();
                filter.add_arch(ScmpArch::Native).unwrap();
                filter
                    .add_rule(ScmpAction::Notify, ScmpSyscall::new("getpid"))
                    .unwrap();
                filter.load().unwrap();
                let notifier = filter.get_notify_fd().unwrap();

                fd_tx.send(notifier).unwrap();

                assert_eq!(unistd::getpid(), Pid::from_raw(0),);

                assert_eq!(unistd::getpid(), Pid::from_raw(1),);

                process::exit(0);
            }
        }
    }
}
