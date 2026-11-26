use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
pub enum Signal {
    #[serde(rename = "SIGHUP")]
    Hup,
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGQUIT")]
    Quit,
    #[serde(rename = "SIGTERM")]
    Term,
    #[serde(rename = "SIGPIPE")]
    Pipe,
    #[serde(rename = "SIGKILL")]
    Kill,
    #[serde(rename = "SIGUSR1")]
    Usr1,
    #[serde(rename = "SIGUSR2")]
    Usr2,
}

impl Signal {
    pub const TERMINATING: [Self; 4] = [Self::Int, Self::Term, Self::Hup, Self::Quit];

    pub fn name(self) -> &'static str {
        match self {
            Self::Hup => "SIGHUP",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Term => "SIGTERM",
            Self::Pipe => "SIGPIPE",
            Self::Kill => "SIGKILL",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
        }
    }

    pub fn number(self) -> libc::c_int {
        match self {
            Self::Hup => libc::SIGHUP,
            Self::Int => libc::SIGINT,
            Self::Quit => libc::SIGQUIT,
            Self::Term => libc::SIGTERM,
            Self::Pipe => libc::SIGPIPE,
            Self::Kill => libc::SIGKILL,
            Self::Usr1 => libc::SIGUSR1,
            Self::Usr2 => libc::SIGUSR2,
        }
    }
}

pub struct Signals {
    received: Arc<AtomicUsize>,
    registrations: Vec<signal_hook::SigId>,
}

impl Signals {
    pub fn install() -> io::Result<Self> {
        // An inherited SIG_IGN for SIGCHLD would discard child wait results.
        if unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        let mut signals = Self {
            received: Arc::new(AtomicUsize::new(0)),
            registrations: Vec::new(),
        };
        for (index, signal) in Signal::TERMINATING.into_iter().enumerate() {
            let received = Arc::clone(&signals.received);
            let value = index + 1;
            // The handler only touches a lock-free integer and performs no I/O.
            let id = unsafe {
                signal_hook::low_level::register(signal.number(), move || {
                    let mut state = received.load(Ordering::Relaxed);
                    while state < 8 {
                        let next = if state == 0 {
                            value
                        } else {
                            state | (value << 3)
                        };
                        match received.compare_exchange_weak(
                            state,
                            next,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(current) => state = current,
                        }
                    }
                })
            }?;
            signals.registrations.push(id);
        }
        Ok(signals)
    }

    pub fn first(&self) -> Option<Signal> {
        Self::decode(self.received.load(Ordering::Relaxed) & 7)
    }

    pub fn second(&self) -> Option<Signal> {
        Self::decode(self.received.load(Ordering::Relaxed) >> 3)
    }

    fn decode(value: usize) -> Option<Signal> {
        value.checked_sub(1).map(|index| Signal::TERMINATING[index])
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

pub fn terminate(signal: Signal) -> ! {
    // Rust ignores SIGPIPE. Restore the OS action for the signal in either case.
    let _ = signal_hook::low_level::emulate_default_handler(signal.number());
    std::process::exit(128 + signal.number());
}

pub fn duplicate(fd: RawFd) -> io::Result<File> {
    // Keep the duplicate out of unrelated children and away from descriptors 0-2.
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(duplicated) })
}

pub fn isolate_stdio() -> io::Result<()> {
    // The supervisor has its own copies. Keep native stdio in the embedded
    // runtime and its libraries away from the program streams.
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for fd in 0..=2 {
        loop {
            if unsafe { libc::dup2(null.as_raw_fd(), fd) } != -1 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

pub fn is_terminal(file: &File) -> bool {
    unsafe { libc::isatty(file.as_raw_fd()) == 1 }
}

pub fn parent_pid() -> u32 {
    unsafe { libc::getppid() as u32 }
}

pub fn process_group() -> u32 {
    unsafe { libc::getpgrp() as u32 }
}

pub fn wait_readable(fd: RawFd, cancel: RawFd) -> io::Result<bool> {
    let mut fds = [
        libc::pollfd {
            fd: cancel,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } >= 0 {
            return Ok(fds[0].revents == 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub fn wait_writable(fd: RawFd) -> io::Result<()> {
    let mut fd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        if unsafe { libc::poll(&mut fd, 1, -1) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub struct Group {
    child: Child,
    reaped: bool,
}

impl Group {
    pub fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn signal(&mut self, signal: Signal) -> io::Result<()> {
        let pid = self.pid() as libc::pid_t;
        // The unreaped leader reserves the process group ID.
        if unsafe { libc::kill(-pid, signal.number()) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
        if unsafe { libc::kill(pid, signal.number()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn reap(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.reaped {
            return Ok(None);
        }
        // Inspect the leader without consuming its wait status. Kill any
        // remaining members of its group before releasing the group ID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(error)
            };
        }
        if info.si_signo == 0 {
            return Ok(None);
        }
        let _ = self.signal(Signal::Kill);
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(Some(status))
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.signal(Signal::Kill);
            let _ = self.reap();
        }
    }
}
