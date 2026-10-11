//! Nonblocking Unix pipes and process-group ownership retained until reaping.

use super::Spec;
use mio::{Interest, Registry, Token};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;

pub(super) struct Pipe(File);

pub(super) fn inherit(index: u8) -> io::Result<File> {
    let handle = match index {
        0 => std::io::stdin().as_fd().try_clone_to_owned(),
        1 => std::io::stdout().as_fd().try_clone_to_owned(),
        _ => std::io::stderr().as_fd().try_clone_to_owned(),
    }?;
    Ok(File::from(handle))
}

pub(super) fn pipe() -> io::Result<(File, File)> {
    let (read, write) = std::io::pipe()?;
    Ok((File::from(OwnedFd::from(read)), File::from(OwnedFd::from(write))))
}

pub(super) fn endpoint(output: bool) -> io::Result<(Pipe, File)> {
    let (read, write) = pipe()?;
    let (parent, child) = if output { (read, write) } else { (write, read) };
    let fd = parent.as_raw_fd();
    // SAFETY: fd is owned and open; F_GETFL/F_SETFL take no pointer arguments.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((Pipe(parent), child))
}

impl Pipe {
    pub(super) fn register(
        &mut self,
        registry: &Registry,
        token: usize,
        output: bool,
    ) -> io::Result<()> {
        registry.register(
            &mut mio::unix::SourceFd(&self.0.as_raw_fd()),
            Token(token),
            if output { Interest::READABLE } else { Interest::WRITABLE },
        )
    }
    pub(super) fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
    pub(super) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    pub(super) fn drained(&mut self) -> io::Result<bool> {
        Ok(true)
    }
}

pub(super) struct Child(std::process::Child);

pub(super) fn spawn(spec: &Spec, input: File, output: File, error: File) -> io::Result<Child> {
    let mut command = spec.command()?;
    command.stdin(input).stdout(output).stderr(error).process_group(0);
    command.spawn().map(Child)
}

impl Child {
    pub(super) fn status(&mut self) -> io::Result<Option<super::Exit>> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: this PID is our unreaped child; waitid initializes info. WNOWAIT
        // retains the group leader's PID until group shutdown, preventing reuse.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.0.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: waitid succeeded, including the zero-PID no-status case.
        let info = unsafe { info.assume_init() };
        // SAFETY: waitid supplies the SIGCHLD variant of siginfo_t.
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        // SAFETY: same initialized SIGCHLD variant.
        let code = unsafe { info.si_status() };
        Ok(Some(if info.si_code == libc::CLD_EXITED {
            super::Exit::Code(i64::from(code))
        } else {
            super::Exit::Signal(i64::from(code), info.si_code == libc::CLD_DUMPED)
        }))
    }
    pub(super) fn kill(&self) -> io::Result<()> {
        // SAFETY: the unreaped leader reserves this PID/group identity. All
        // children are started as new process-group leaders by spawn.
        if unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            // Darwin's POSIX kill skips zombies while walking a group, then
            // returns EPERM if none remain. Verify membership rather than hiding
            // an actual permission failure against a live child or descendant.
            #[cfg(target_os = "macos")]
            if error.raw_os_error() == Some(libc::EPERM)
                && no_live_group_members(self.0.id() as libc::pid_t)
            {
                return Ok(());
            }
            if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
        }
    }
    pub(super) fn reap(&mut self) -> io::Result<()> {
        self.0.wait().map(|_| ())
    }
}

#[cfg(target_os = "macos")]
fn no_live_group_members(group: libc::pid_t) -> bool {
    let mut pids = vec![0 as libc::pid_t; 32];
    loop {
        let Some(bytes) = pids.len().checked_mul(std::mem::size_of::<libc::pid_t>()) else {
            return false;
        };
        let Ok(bytes) = i32::try_from(bytes) else { return false };
        // SAFETY: libproc writes at most bytes into the owned PID buffer. Its
        // wrapper returns a PID count and uses zero for both failure and empty
        // results, so clear/check this thread's errno to distinguish them.
        let count = unsafe {
            *libc::__error() = 0;
            libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes)
        };
        let last = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if count < 0 || (count == 0 && last != 0 && last != libc::ESRCH) {
            return false;
        }
        if count as usize >= pids.len() {
            let length = pids.len();
            if pids.try_reserve_exact(length).is_err() {
                return false;
            }
            pids.resize(length * 2, 0);
            continue;
        }
        for &pid in &pids[..count as usize] {
            if pid <= 0 {
                continue;
            }
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            // SAFETY: the output buffer has exactly the advertised structure size.
            let written = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
            };
            if written != size {
                if written == 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return false;
            }
            // SAFETY: a complete successful proc_pidinfo initialized every field.
            let info = unsafe { info.assume_init() };
            if info.pbi_pgid == group as u32 && info.pbi_status != libc::SZOMB {
                return false;
            }
        }
        return true;
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn a_live_process_group_is_not_treated_as_zombies() {
        // SAFETY: getpgrp reads this process's group identifier.
        assert!(!super::no_live_group_members(unsafe { libc::getpgrp() }));
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // The supervisor normally reaps explicitly. wait is idempotent in std.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.kill();
            let _ = self.reap();
        }
    }
}
