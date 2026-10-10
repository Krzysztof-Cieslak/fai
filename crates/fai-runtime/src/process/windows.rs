//! Overlapped Windows pipes and suspended creation into a kill-on-close Job.

use super::Spec;
use mio::{Interest, Registry, Token};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;

pub(super) struct Pipe(mio::windows::NamedPipe);

pub(super) fn pipe() -> io::Result<(File, File)> {
    let (read, write) = std::io::pipe()?;
    Ok((File::from(OwnedHandle::from(read)), File::from(OwnedHandle::from(write))))
}

pub(super) fn endpoint(output: bool) -> io::Result<(Pipe, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        r"\\.\pipe\fai-process-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let parent = mio::windows::NamedPipe::new(&name)?;
    let child = OpenOptions::new().read(!output).write(output).open(name)?;
    Ok((Pipe(parent), child))
}

impl Pipe {
    pub(super) fn register(
        &mut self,
        registry: &Registry,
        token: usize,
        _: bool,
    ) -> io::Result<()> {
        registry.register(&mut self.0, Token(token), Interest::READABLE | Interest::WRITABLE)?;
        match self.0.connect() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            result => result,
        }
    }
    pub(super) fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
    pub(super) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    pub(super) fn drained(&mut self) -> io::Result<bool> {
        // mio checks its pending write before accepting another (even empty)
        // write. flush is a no-op and cannot establish this completion boundary.
        match self.0.write(&[]) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }
}

pub(super) struct Child {
    process: OwnedHandle,
    job: OwnedHandle,
}

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn quote(argument: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for c in argument.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n('\\', if c == '"' { slashes * 2 + 1 } else { slashes }));
        result.push(c);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

fn inherited(file: &File) -> io::Result<OwnedHandle> {
    let mut handle = std::ptr::null_mut();
    // SAFETY: the source File remains live and the initialized destination is
    // immediately owned. Only these duplicated handles are in the child list.
    unsafe {
        let current = GetCurrentProcess();
        if DuplicateHandle(
            current,
            file.as_raw_handle(),
            current,
            &mut handle,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(handle))
    }
}

pub(super) fn spawn(spec: &Spec, input: File, output: File, error: File) -> io::Result<Child> {
    let command = spec.command()?;
    let application = wide(command.get_program());
    let line = std::iter::once(command.get_program().to_string_lossy().into_owned())
        .chain(spec.arguments.iter().cloned())
        .map(|s| quote(&s))
        .collect::<Vec<_>>()
        .join(" ");
    let mut line = wide(std::ffi::OsStr::new(&line));
    let directory = command.get_current_dir().map(|p| wide(p.as_os_str()));
    let mut environment = Vec::new();
    for (name, value) in spec.environment() {
        environment.extend(name.encode_wide());
        environment.push('=' as u16);
        environment.extend(value.encode_wide());
        environment.push(0);
    }
    environment.push(0);
    if environment.len() == 1 {
        environment.push(0);
    }
    let input = inherited(&input)?;
    let output = inherited(&output)?;
    let error = inherited(&error)?;
    let mut handles = [input.as_raw_handle(), output.as_raw_handle(), error.as_raw_handle()];
    // SAFETY: all Win32 structures have their required sizes initialized, UTF-16
    // strings are NUL-terminated and remain live through CreateProcessW. The
    // suspended primary thread is resumed only after successful Job assignment.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle::from_raw_handle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let list = storage.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
            return Err(io::Error::last_os_error());
        }
        let configured = UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            std::mem::size_of_val(&handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) != 0;
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of_val(&startup) as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = list;
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        let created = configured
            && CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_NO_WINDOW,
                environment.as_ptr().cast(),
                directory.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            ) != 0;
        let failure = io::Error::last_os_error();
        DeleteProcThreadAttributeList(list);
        if !created {
            return Err(failure);
        }
        let process = OwnedHandle::from_raw_handle(info.hProcess);
        let thread = OwnedHandle::from_raw_handle(info.hThread);
        if AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) == 0 {
            let failure = io::Error::last_os_error();
            TerminateProcess(process.as_raw_handle(), 1);
            WaitForSingleObject(process.as_raw_handle(), INFINITE);
            return Err(failure);
        }
        let child = Child { process, job };
        if ResumeThread(thread.as_raw_handle()) == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(child)
    }
}

impl Child {
    pub(super) fn status(&mut self) -> io::Result<Option<super::Exit>> {
        // SAFETY: process is an owned process handle with query/wait rights.
        unsafe {
            let wait = WaitForSingleObject(self.process.as_raw_handle(), 0);
            if wait == windows_sys::Win32::Foundation::WAIT_TIMEOUT {
                return Ok(None);
            }
            if wait == windows_sys::Win32::Foundation::WAIT_FAILED {
                return Err(io::Error::last_os_error());
            }
            let mut code = 0;
            if GetExitCodeProcess(self.process.as_raw_handle(), &mut code) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(super::Exit::Code(i64::from(code))))
        }
    }
    pub(super) fn kill(&self) -> io::Result<()> {
        // SAFETY: job is an owned Job handle; terminating it includes descendants.
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub(super) fn reap(&mut self) -> io::Result<()> {
        // SAFETY: wait retains the owned process handle until completion.
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) }
            == windows_sys::Win32::Foundation::WAIT_FAILED
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.kill();
        let _ = self.reap();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn argument_quoting_preserves_quotes_and_trailing_slashes() {
        assert_eq!(super::quote("a\\\"b\\"), "\"a\\\\\\\"b\\\\\"");
    }
}
