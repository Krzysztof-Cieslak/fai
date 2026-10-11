//! Native filesystem capability, with lossless names, structured errors and
//! shared handle state. OS work runs on the blocking pool; Fai values stay on
//! the calling task. Closing a handle expires every alias.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::Value;
use crate::scheduler::{CancellationProbe, cancellation_probe};

type Result<T> = std::result::Result<T, Error>;

// Declaration order agrees with FileSystem.ErrorKind in the embedded library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    InvalidPath,
    InvalidArgument,
    InvalidEncoding,
    Closed,
    Unsupported,
    CrossDevice,
    TooManyLinks,
    TooLarge,
    Cancelled,
    IoFailure,
}

#[derive(Debug)]
struct Error {
    kind: Kind,
    message: String,
    code: Option<i32>,
}

impl Error {
    fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), code: None }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        use std::io::ErrorKind as E;
        let kind = match error.kind() {
            E::NotFound => Kind::NotFound,
            E::AlreadyExists => Kind::AlreadyExists,
            E::PermissionDenied => Kind::PermissionDenied,
            E::NotADirectory => Kind::NotDirectory,
            E::IsADirectory => Kind::IsDirectory,
            E::DirectoryNotEmpty => Kind::DirectoryNotEmpty,
            E::InvalidInput => Kind::InvalidArgument,
            E::InvalidData => Kind::InvalidEncoding,
            E::Unsupported => Kind::Unsupported,
            E::CrossesDevices => Kind::CrossDevice,
            _ if link_loop(error.raw_os_error()) => Kind::TooManyLinks,
            E::FileTooLarge | E::OutOfMemory => Kind::TooLarge,
            _ => Kind::IoFailure,
        };
        Self { kind, message: error.to_string(), code: error.raw_os_error() }
    }
}

fn link_loop(code: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        code == Some(libc::ELOOP)
    }
    #[cfg(windows)]
    {
        code == Some(windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME as i32)
    }
}

fn check(probe: &CancellationProbe) -> Result<()> {
    if probe.is_cancelled() {
        Err(Error::new(Kind::Cancelled, "filesystem operation cancelled"))
    } else {
        Ok(())
    }
}

fn run_io<T: Send + 'static>(
    f: impl FnOnce(CancellationProbe) -> Result<T> + Send + 'static,
) -> Result<T> {
    let probe = cancellation_probe();
    let work = move || {
        check(&probe)?;
        f(probe)
    };
    if crate::scheduler::in_task() {
        crate::scheduler::run_blocking(Box::new(work))
    } else {
        work()
    }
}

fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: every supplied field is owned and transferred into the data cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

fn some(value: Option<Value>) -> Value {
    value.map_or(crate::imm_int(0), |v| data(1, &[v]))
}

fn result<T>(
    outcome: Result<T>,
    operation: &str,
    path: Option<&Path>,
    destination: Option<&Path>,
    encode: impl FnOnce(T) -> Value,
) -> Value {
    match outcome {
        Ok(value) => data(0, &[encode(value)]),
        Err(error) => {
            // Record layout is sorted by label: destination, kind, message,
            // nativeCode, operation, path. Option payloads use the uniform ABI.
            let fields = [
                some(destination.map(|p| crate::make_bytes(&crate::path::encode(p.as_os_str())))),
                crate::imm_int(error.kind as i64),
                crate::make_string(error.message.as_bytes()),
                some(error.code.map(|code| crate::fai_box_int(i64::from(code)))),
                crate::make_string(operation.as_bytes()),
                some(path.map(|p| crate::make_bytes(&crate::path::encode(p.as_os_str())))),
            ];
            data(1, &[data(0, &fields)])
        }
    }
}

fn decode_path(value: Value) -> Result<PathBuf> {
    // SAFETY: Path is an opaque alias over Bytes, without a wrapper cell.
    crate::path::decode(unsafe { crate::bytes_bytes(value) })
        .map_err(|_| Error::new(Kind::InvalidPath, "invalid native path units"))
}

enum FileState {
    Reader(Option<BufReader<File>>),
    Writer(Option<BufWriter<File>>),
}
struct FileObject {
    path: PathBuf,
    state: Mutex<FileState>,
}

fn file_handle_value(path: PathBuf, state: FileState) -> Value {
    let raw =
        Arc::into_raw(Arc::new(FileObject { path, state: Mutex::new(state) })) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_FILE_DESC));
    // SAFETY: the allocated handle has one slot holding the Arc owner.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Releases the native owner at the file cell's final drop.
pub(crate) fn drop_file_handle(raw: i64) {
    // SAFETY: raw was created by Arc::into_raw for this handle cell.
    drop(unsafe { Arc::from_raw(raw as usize as *const FileObject) });
}

fn file_arc(value: Value) -> Arc<FileObject> {
    // SAFETY: the typed Reader/Writer is a live file handle; keep its Arc intact.
    let raw = unsafe { crate::read_i64(crate::as_obj(value), crate::HANDLE_PTR_OFFSET) };
    // SAFETY: ManuallyDrop borrows the cell's Arc owner while cloning it.
    let borrowed = ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const FileObject) });
    Arc::clone(&borrowed)
}

fn open(path: Value, mode: i64) -> Value {
    let decoded = decode_path(path);
    let location = decoded.as_ref().ok().cloned();
    let outcome = decoded.and_then(|path| {
        run_io(move |_| {
            if mode == -1 {
                return Ok((
                    path.clone(),
                    FileState::Reader(Some(BufReader::new(File::open(path)?))),
                ));
            }
            let mut options = OpenOptions::new();
            options.write(true);
            match mode {
                0 => {
                    options.create_new(true);
                }
                1 => {
                    options.create(true).truncate(true);
                }
                2 => {
                    options.create(true).append(true);
                }
                3 => {}
                _ => return Err(Error::new(Kind::InvalidArgument, "invalid writer mode")),
            }
            Ok((path.clone(), FileState::Writer(Some(BufWriter::new(options.open(path)?)))))
        })
    });
    crate::fai_drop(path);
    result(
        outcome,
        if mode == -1 { "openRead" } else { "openWriter" },
        location.as_deref(),
        None,
        |(path, state)| file_handle_value(path, state),
    )
}

/// Opens a native path for reading, consuming it.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_read(path: Value) -> Value {
    open(path, -1)
}
/// Opens a native path for truncating writes, consuming it.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_write(path: Value) -> Value {
    open(path, 1)
}
/// Opens a native path for append, consuming it.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_append(path: Value) -> Value {
    open(path, 2)
}
/// Opens with an explicit exclusive/truncate/append/existing writer policy.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_writer(path: Value, mode: Value) -> Value {
    let mode_value = crate::unbox_int(mode);
    crate::fai_drop(mode);
    open(path, mode_value)
}

fn closed() -> Error {
    Error::new(Kind::Closed, "file handle is closed")
}

/// Reads up to max bytes. A positive large request uses bounded native chunks.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_read_chunk(reader: Value, max: Value) -> Value {
    let count = crate::unbox_int(max);
    crate::fai_drop(max);
    let arc = file_arc(reader);
    let path = arc.path.clone();
    let outcome = run_io(move |probe| {
        if count <= 0 {
            return Err(Error::new(Kind::InvalidArgument, "read size must be positive"));
        }
        let mut state = arc.state.lock().expect("file state");
        check(&probe)?;
        let FileState::Reader(reader) = &mut *state else {
            return Err(Error::new(Kind::InvalidArgument, "not a reader"));
        };
        let reader = reader.as_mut().ok_or_else(closed)?;
        let mut bytes = vec![0; count.min(65536) as usize];
        let length = reader.read(&mut bytes)?;
        bytes.truncate(length);
        Ok(bytes)
    });
    crate::fai_drop(reader);
    result(outcome, "readChunk", Some(&path), None, |bytes| crate::make_bytes(&bytes))
}

/// Writes all bytes, observing cancellation between bounded native writes.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_write_chunk(writer: Value, bytes: Value) -> Value {
    // SAFETY: bytes has static type Bytes; the blocking worker owns a Rust copy.
    let bytes_value = unsafe { crate::bytes_bytes(bytes) }.to_vec();
    crate::fai_drop(bytes);
    let arc = file_arc(writer);
    let path = arc.path.clone();
    let outcome = run_io(move |probe| {
        let mut state = arc.state.lock().expect("file state");
        check(&probe)?;
        let FileState::Writer(writer) = &mut *state else {
            return Err(Error::new(Kind::InvalidArgument, "not a writer"));
        };
        let writer = writer.as_mut().ok_or_else(closed)?;
        for bytes in bytes_value.chunks(65536) {
            check(&probe)?;
            writer.write_all(bytes)?;
        }
        Ok(())
    });
    crate::fai_drop(writer);
    result(outcome, "writeChunk", Some(&path), None, |_| crate::FAI_UNIT)
}

fn close_state(state: &mut FileState) -> Result<()> {
    match state {
        FileState::Reader(reader) => {
            reader.take();
            Ok(())
        }
        FileState::Writer(writer) => match writer.take() {
            None => Ok(()),
            Some(mut writer) => {
                let outcome = writer.flush().map_err(Error::from);
                // The handle is closed even on flush failure. Do not retry failed
                // buffered output as a hidden side effect of BufWriter::drop.
                drop(writer.into_parts());
                outcome
            }
        },
    }
}

fn close(value: Value, operation: &'static str) -> Value {
    let arc = file_arc(value);
    let path = arc.path.clone();
    let outcome = run_io(move |_| close_state(&mut arc.state.lock().expect("file state")));
    crate::fai_drop(value);
    result(outcome, operation, Some(&path), None, |_| crate::FAI_UNIT)
}
/// Closes and expires every alias. Repeated close succeeds.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_close_reader(reader: Value) -> Value {
    close(reader, "closeReader")
}
/// Flushes, closes and expires every alias, including on flush failure.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_close_writer(writer: Value) -> Value {
    close(writer, "closeWriter")
}

fn writer_control(writer: Value, sync: Option<bool>) -> Value {
    let arc = file_arc(writer);
    let path = arc.path.clone();
    let outcome = run_io(move |probe| {
        let mut state = arc.state.lock().expect("file state");
        check(&probe)?;
        let FileState::Writer(writer) = &mut *state else {
            return Err(Error::new(Kind::InvalidArgument, "not a writer"));
        };
        let writer = writer.as_mut().ok_or_else(closed)?;
        writer.flush()?;
        match sync {
            Some(true) => writer.get_ref().sync_all()?,
            Some(false) => writer.get_ref().sync_data()?,
            None => {}
        }
        Ok(())
    });
    crate::fai_drop(writer);
    result(
        outcome,
        if sync.is_some() { "syncWriter" } else { "flushWriter" },
        Some(&path),
        None,
        |_| crate::FAI_UNIT,
    )
}
/// Flushes user-space output buffers.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_flush(writer: Value) -> Value {
    writer_control(writer, None)
}
/// Flushes and synchronizes data, optionally including metadata.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_sync(writer: Value, full: Value) -> Value {
    let flag = crate::unbox_int(full) != 0;
    crate::fai_drop(full);
    writer_control(writer, Some(flag))
}
/// Seeks to an absolute nonnegative byte offset and returns the resulting offset.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_seek(handle: Value, offset: Value) -> Value {
    let offset_value = crate::unbox_int(offset);
    crate::fai_drop(offset);
    let arc = file_arc(handle);
    let path = arc.path.clone();
    let outcome = run_io(move |probe| {
        if offset_value < 0 {
            return Err(Error::new(Kind::InvalidArgument, "negative seek offset"));
        }
        let mut state = arc.state.lock().expect("file state");
        check(&probe)?;
        let position = SeekFrom::Start(offset_value as u64);
        let offset = match &mut *state {
            FileState::Reader(reader) => reader.as_mut().ok_or_else(closed)?.seek(position)?,
            FileState::Writer(writer) => writer.as_mut().ok_or_else(closed)?.seek(position)?,
        };
        i64::try_from(offset).map_err(|_| Error::new(Kind::TooLarge, "seek result exceeds Int"))
    });
    crate::fai_drop(handle);
    result(outcome, "seek", Some(&path), None, |offset| crate::fai_box_int(offset))
}

fn read_all(path: &Path, limit: i64, probe: &CancellationProbe) -> Result<Vec<u8>> {
    if limit < 0 {
        return Err(Error::new(Kind::InvalidArgument, "negative read limit"));
    }
    let mut reader = File::open(path)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        check(probe)?;
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            return Ok(bytes);
        }
        if n as u64 > (limit as u64).saturating_sub(bytes.len() as u64) {
            return Err(Error::new(Kind::TooLarge, "file exceeds read limit"));
        }
        bytes.try_reserve(n).map_err(|e| Error::new(Kind::TooLarge, e.to_string()))?;
        bytes.extend_from_slice(&buffer[..n]);
    }
}

fn read_path(path: Value, limit: i64, text: bool) -> Value {
    let decoded = decode_path(path);
    let location = decoded.as_ref().ok().cloned();
    let outcome = decoded.and_then(|path| {
        run_io(move |probe| {
            let bytes = read_all(&path, limit, &probe)?;
            if text && std::str::from_utf8(&bytes).is_err() {
                return Err(Error::new(Kind::InvalidEncoding, "file is not UTF-8"));
            }
            Ok(bytes)
        })
    });
    crate::fai_drop(path);
    result(
        outcome,
        if text { "readFile" } else { "readBytes" },
        location.as_deref(),
        None,
        |bytes| if text { crate::make_string(&bytes) } else { crate::make_bytes(&bytes) },
    )
}
/// Reads a complete UTF-8 file with checked allocation growth.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_read(path: Value) -> Value {
    read_path(path, i64::MAX, true)
}
/// Reads binary data up to an explicit nonnegative byte limit.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_read_bytes(path: Value, limit: Value) -> Value {
    let count = crate::unbox_int(limit);
    crate::fai_drop(limit);
    read_path(path, count, false)
}
fn write_path(path: Value, contents: Value, text: bool) -> Value {
    // SAFETY: the public entry selects the matching statically typed buffer.
    let bytes =
        unsafe { if text { crate::string_bytes(contents) } else { crate::bytes_bytes(contents) } }
            .to_vec();
    crate::fai_drop(contents);
    let decoded = decode_path(path);
    let location = decoded.as_ref().ok().cloned();
    let outcome = decoded.and_then(|path| {
        run_io(move |probe| {
            let mut file = File::create(path)?;
            for chunk in bytes.chunks(65536) {
                check(&probe)?;
                file.write_all(chunk)?;
            }
            Ok(())
        })
    });
    crate::fai_drop(path);
    result(
        outcome,
        if text { "writeFile" } else { "writeBytes" },
        location.as_deref(),
        None,
        |_| crate::FAI_UNIT,
    )
}
/// Writes a complete UTF-8 file, creating or truncating the destination.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_write(path: Value, text: Value) -> Value {
    write_path(path, text, true)
}
/// Writes complete binary data, creating or truncating the destination.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_write_bytes(path: Value, bytes: Value) -> Value {
    write_path(path, bytes, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_expires_shared_reader_state() {
        let file = tempfile::tempfile().unwrap();
        let handle = Arc::new(FileObject {
            path: PathBuf::new(),
            state: Mutex::new(FileState::Reader(Some(BufReader::new(file)))),
        });
        let alias = handle.clone();
        close_state(&mut handle.state.lock().unwrap()).unwrap();
        assert!(matches!(*alias.state.lock().unwrap(), FileState::Reader(None)));
        close_state(&mut alias.state.lock().unwrap()).unwrap();
    }
    #[test]
    fn close_flushes_and_expires_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        writer.write_all(b"pending").unwrap();
        let mut state = FileState::Writer(Some(writer));
        close_state(&mut state).unwrap();
        assert!(matches!(state, FileState::Writer(None)));
        assert_eq!(std::fs::read(path).unwrap(), b"pending");
    }
    #[test]
    fn read_limit_detects_growth_without_metadata_assumptions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"four").unwrap();
        assert_eq!(read_all(&path, 3, &cancellation_probe()).unwrap_err().kind, Kind::TooLarge);
        assert_eq!(read_all(&path, 4, &cancellation_probe()).unwrap(), b"four");
    }
}
