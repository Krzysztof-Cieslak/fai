//! File-handle capabilities: progressive (chunked) reading and writing of files.
//!
//! Mirrors the network handle pattern (a Fai `KIND_FILE` cell owns a raw
//! `Arc<FileObject>` in its slot, released — closing the file — when the cell
//! dies), but uses the blocking-work pool rather than the I/O reactor: a regular
//! file is not readiness-pollable, so a blocking read/write is offloaded to the
//! pool while the calling task parks (and run inline when there is no scheduler,
//! mirroring `fai_file_read`).

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};

use crate::Value;

/// The OS-side state of an open file handle, owned by its Fai handle cell. A
/// `Reader` and a `Writer` are the same kind of cell at runtime (the opaque Fai
/// types distinguish them); this enum carries the buffered handle.
enum FileObject {
    Reader(Mutex<BufReader<File>>),
    Writer(Mutex<BufWriter<File>>),
}

/// Runs blocking file work on the blocking pool when inside a task (so a worker is
/// not stalled), or inline when there is no scheduler (a program without
/// concurrency). The same dispatch `fai_file_read` uses.
fn run_io<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    if crate::scheduler::in_task() { crate::scheduler::run_blocking(Box::new(f)) } else { f() }
}

/// Wraps file state as a Fai `KIND_FILE` value owning the `Arc`.
fn file_handle_value(obj: FileObject) -> Value {
    let raw = Arc::into_raw(Arc::new(obj)) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_FILE_DESC));
    // SAFETY: `p` has room for the header and one slot.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Releases the `Arc<FileObject>` a dead file cell owned (called by `free_obj`).
pub(crate) fn drop_file_handle(raw: i64) {
    // SAFETY: `raw` came from `Arc::into_raw` in `file_handle_value`.
    drop(unsafe { Arc::from_raw(raw as usize as *const FileObject) });
}

/// Clones the `Arc<FileObject>` from a file value (incrementing its count) so the
/// returned handle outlives an off-worker blocking op; the cell keeps its own
/// reference (released when it dies).
fn file_arc(v: Value) -> Arc<FileObject> {
    // SAFETY: `v` is a live `KIND_FILE` cell whose slot holds the `Arc` pointer.
    let raw = unsafe { crate::read_i64(crate::as_obj(v), crate::HANDLE_PTR_OFFSET) };
    // SAFETY: `raw` came from `Arc::into_raw`; `ManuallyDrop` keeps the cell's
    // reference intact while we clone an extra one to return.
    let borrowed = ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const FileObject) });
    Arc::clone(&borrowed)
}

/// Builds `Ok v` (`Result _ _` is `| Ok 'ok | Err 'err`, so `Ok` is tag 0).
fn ok_result(v: Value) -> Value {
    // SAFETY: one owned field moves into the `Ok` cell.
    unsafe { crate::fai_make_data(0, 1, [v].as_ptr()) }
}

/// Builds `Err <message>` (tag 1) from an error string.
fn err_result(msg: &str) -> Value {
    let s = crate::make_string(msg.as_bytes());
    // SAFETY: one owned field moves into the `Err` cell.
    unsafe { crate::fai_make_data(1, 1, [s].as_ptr()) }
}

/// `FileSystem.openRead`: open `path` for reading. Returns `Result Reader String`.
/// Consumes `path`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_read(path: Value) -> Value {
    // SAFETY: `path` is a boxed `String`.
    let p = unsafe { crate::string_str(path) }.to_owned();
    let outcome = run_io(move || File::open(&p).map(BufReader::new).map_err(|e| e.to_string()));
    crate::fai_drop(path);
    match outcome {
        Ok(r) => ok_result(file_handle_value(FileObject::Reader(Mutex::new(r)))),
        Err(e) => err_result(&e),
    }
}

/// `FileSystem.openWrite`: open `path` for writing, creating or truncating it.
/// Returns `Result Writer String`. Consumes `path`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_write(path: Value) -> Value {
    // SAFETY: `path` is a boxed `String`.
    let p = unsafe { crate::string_str(path) }.to_owned();
    let outcome = run_io(move || File::create(&p).map(BufWriter::new).map_err(|e| e.to_string()));
    crate::fai_drop(path);
    match outcome {
        Ok(w) => ok_result(file_handle_value(FileObject::Writer(Mutex::new(w)))),
        Err(e) => err_result(&e),
    }
}

/// `FileSystem.openAppend`: open `path` for appending, creating it if absent.
/// Returns `Result Writer String`. Consumes `path`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_open_append(path: Value) -> Value {
    // SAFETY: `path` is a boxed `String`.
    let p = unsafe { crate::string_str(path) }.to_owned();
    let outcome = run_io(move || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map(BufWriter::new)
            .map_err(|e| e.to_string())
    });
    crate::fai_drop(path);
    match outcome {
        Ok(w) => ok_result(file_handle_value(FileObject::Writer(Mutex::new(w)))),
        Err(e) => err_result(&e),
    }
}

/// `FileSystem.readChunk`: read up to `max` bytes from a reader. Returns
/// `Result Bytes String`; an empty buffer signals end of file. Consumes the reader
/// reference; `max` is an `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_read_chunk(reader: Value, max: Value) -> Value {
    let cap = crate::unbox_int(max).max(0) as usize;
    crate::fai_drop(max);
    let arc = file_arc(reader);
    let outcome = run_io(move || match &*arc {
        FileObject::Reader(m) => {
            let mut r = m.lock().expect("reader lock");
            let mut buf = vec![0u8; cap];
            match r.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    Ok(buf)
                }
                Err(e) => Err(e.to_string()),
            }
        }
        FileObject::Writer(_) => Err("readChunk: handle is not a reader".to_owned()),
    });
    crate::fai_drop(reader);
    match outcome {
        Ok(buf) => ok_result(crate::make_bytes(&buf)),
        Err(e) => err_result(&e),
    }
}

/// `FileSystem.writeChunk`: write all of `bytes` to a writer. Returns
/// `Result Unit String`. Consumes both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_write_chunk(writer: Value, bytes: Value) -> Value {
    // SAFETY: `bytes` is a boxed `Bytes`; copy it out so the closure owns the data
    // (it may run on another thread) and the reference can be dropped now.
    let data: Vec<u8> = unsafe { crate::bytes_bytes(bytes) }.to_vec();
    crate::fai_drop(bytes);
    let arc = file_arc(writer);
    let outcome = run_io(move || match &*arc {
        FileObject::Writer(m) => {
            m.lock().expect("writer lock").write_all(&data).map_err(|e| e.to_string())
        }
        FileObject::Reader(_) => Err("writeChunk: handle is not a writer".to_owned()),
    });
    crate::fai_drop(writer);
    match outcome {
        Ok(()) => ok_result(crate::FAI_UNIT),
        Err(e) => err_result(&e),
    }
}

/// `FileSystem.closeReader`: release this reference to a reader. The file closes
/// once the last reference is dropped (a unique handle closes immediately).
/// Returns `Unit`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_close_reader(reader: Value) -> Value {
    crate::fai_drop(reader);
    crate::FAI_UNIT
}

/// `FileSystem.closeWriter`: flush a writer's buffered output and release this
/// reference. Returns `Result Unit String` (flushing can fail, e.g. a full disk).
/// The file closes once the last reference is dropped.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_close_writer(writer: Value) -> Value {
    let arc = file_arc(writer);
    let outcome = run_io(move || match &*arc {
        FileObject::Writer(m) => m.lock().expect("writer lock").flush().map_err(|e| e.to_string()),
        FileObject::Reader(_) => Err("closeWriter: handle is not a writer".to_owned()),
    });
    crate::fai_drop(writer);
    match outcome {
        Ok(()) => ok_result(crate::FAI_UNIT),
        Err(e) => err_result(&e),
    }
}
