//! Directory cursors, metadata, identity, and explicit path mutation policies.

use super::*;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn entry_kind(metadata: &fs::Metadata) -> i64 {
    kind_of(metadata.file_type())
}
fn kind_of(kind: fs::FileType) -> i64 {
    if kind.is_symlink() {
        2
    } else if kind.is_dir() {
        1
    } else if kind.is_file() {
        0
    } else {
        3
    }
}

fn identity(path: &Path, follow: bool, metadata: &fs::Metadata) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = (path, follow);
        Ok([metadata.dev().to_le_bytes(), metadata.ino().to_le_bytes()].concat())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        let _ = metadata;
        let file = OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_BACKUP_SEMANTICS | if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT },
            )
            .open(path)?;
        file_identity(&file)
    }
}

fn file_identity(file: &File) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        identity(Path::new(""), true, &file.metadata()?)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = Default::default();
        // SAFETY: the handle is live and info is valid writable storage.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok([
            info.dwVolumeSerialNumber.to_le_bytes(),
            info.nFileIndexHigh.to_le_bytes(),
            info.nFileIndexLow.to_le_bytes(),
        ]
        .concat())
    }
}

fn time_value(time: std::io::Result<SystemTime>) -> Option<Value> {
    let time = time.ok()?;
    let nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    const DAY: i128 = 86_400_000_000_000;
    let day = i64::try_from(nanos.div_euclid(DAY)).ok()?;
    let nano = nanos.rem_euclid(DAY) as i64;
    Some(data(0, &[crate::fai_box_int(day), crate::fai_box_int(nano)]))
}

/// Obtain followed or link metadata with nanosecond Instants and native identity.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_metadata(path: Value, follow: Value) -> Value {
    let followed = crate::unbox_int(follow) != 0;
    crate::fai_drop(follow);
    let decoded = decode_path(path);
    let location = decoded.as_ref().ok().cloned();
    let outcome = decoded.and_then(|path| {
        run_io(move |_| {
            let metadata =
                if followed { fs::metadata(&path) } else { fs::symlink_metadata(&path) }?;
            let id = identity(&path, followed, &metadata)?;
            let len = i64::try_from(metadata.len())
                .map_err(|_| Error::new(Kind::TooLarge, "file length exceeds Int"))?;
            Ok((metadata, id, len))
        })
    });
    crate::fai_drop(path);
    result(
        outcome,
        if followed { "metadata" } else { "linkMetadata" },
        location.as_deref(),
        None,
        |(m, id, len)| {
            data(
                0,
                &[
                    some(time_value(m.accessed())),
                    some(time_value(m.created())),
                    crate::make_bytes(&id),
                    crate::imm_int(entry_kind(&m)),
                    crate::fai_box_int(len),
                    some(time_value(m.modified())),
                    crate::from_bool(m.permissions().readonly()),
                ],
            )
        },
    )
}

/// Compare followed file identity rather than path spelling.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_same_file(a: Value, b: Value) -> Value {
    let left = decode_path(a);
    let right = decode_path(b);
    let pa = left.as_ref().ok().cloned();
    let pb = right.as_ref().ok().cloned();
    let outcome = left.and_then(|a| {
        right.and_then(|b| {
            run_io(move |_| {
                Ok(identity(&a, true, &fs::metadata(&a)?)?
                    == identity(&b, true, &fs::metadata(&b)?)?)
            })
        })
    });
    crate::fai_drop(a);
    crate::fai_drop(b);
    result(outcome, "sameFile", pa.as_deref(), pb.as_deref(), crate::from_bool)
}

/// Current directory, temporary directory, canonical path, or symlink target.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_location(op: Value, path: Value) -> Value {
    let code = crate::unbox_int(op);
    crate::fai_drop(op);
    let decoded = decode_path(path);
    let location = decoded.as_ref().ok().cloned();
    let outcome = decoded.and_then(|path| {
        run_io(move |_| {
            Ok(match code {
                0 => std::env::current_dir()?,
                1 => std::env::temp_dir(),
                2 => fs::canonicalize(path)?,
                3 => fs::read_link(path)?,
                _ => return Err(Error::new(Kind::InvalidArgument, "invalid path operation")),
            })
        })
    });
    crate::fai_drop(path);
    result(
        outcome,
        match code {
            0 => "currentDirectory",
            1 => "temporaryDirectory",
            2 => "canonicalize",
            _ => "readLink",
        },
        if code < 2 { None } else { location.as_deref() },
        None,
        |p| crate::make_bytes(&crate::path::encode(p.as_os_str())),
    )
}

pub(super) fn rename(a: &Path, b: &Path, replace: bool) -> Result<()> {
    #[cfg(unix)]
    {
        if replace {
            return fs::rename(a, b).map_err(Into::into);
        }
        use std::os::unix::ffi::OsStrExt;
        let a = std::ffi::CString::new(a.as_os_str().as_bytes())
            .map_err(|_| Error::new(Kind::InvalidPath, "NUL in source"))?;
        let b = std::ffi::CString::new(b.as_os_str().as_bytes())
            .map_err(|_| Error::new(Kind::InvalidPath, "NUL in destination"))?;
        #[cfg(target_os = "linux")]
        // SAFETY: both NUL-terminated paths live through the atomic rename call.
        let status = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        // SAFETY: both NUL-terminated paths live through the atomic rename call.
        let status = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_EXCL) };
        if status == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().into()) }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};
        let a: Vec<u16> = a.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = b.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: both UTF-16 paths are NUL-terminated and live for the call.
        if unsafe {
            MoveFileExW(a.as_ptr(), b.as_ptr(), if replace { MOVEFILE_REPLACE_EXISTING } else { 0 })
        } == 0
        {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }
}

fn copy(a: &Path, b: &Path, replace: bool, probe: &CancellationProbe) -> Result<()> {
    let mut source = File::open(a)?;
    let source_meta = source.metadata()?;
    if !source_meta.is_file() {
        return Err(Error::new(Kind::InvalidArgument, "copy source is not a regular file"));
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if replace {
        options.create(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut destination = options.open(b)?;
    let destination_meta = destination.metadata()?;
    if !destination_meta.is_file() || destination_meta.file_type().is_symlink() {
        return Err(Error::new(Kind::InvalidArgument, "copy destination is not a regular file"));
    }
    if file_identity(&source)? == file_identity(&destination)? {
        return Err(Error::new(Kind::InvalidArgument, "source and destination are the same file"));
    }
    check(probe)?;
    destination.set_len(0)?;
    let mut buffer = [0; 65536];
    loop {
        check(probe)?;
        let n = source.read(&mut buffer)?;
        if n == 0 {
            return Ok(());
        }
        destination.write_all(&buffer[..n])?;
    }
}

fn mutate(op: i64, a: &Path, b: &Path, probe: &CancellationProbe) -> Result<()> {
    match op {
        0 => fs::create_dir(a)?,
        1 => fs::create_dir_all(a)?,
        2 => fs::remove_file(a)?,
        3 => fs::remove_dir(a)?,
        4 => rename(a, b, false)?,
        5 => rename(a, b, true)?,
        6 => copy(a, b, false, probe)?,
        7 => copy(a, b, true, probe)?,
        8 | 9 => {
            #[cfg(unix)]
            std::os::unix::fs::symlink(a, b)?;
            #[cfg(windows)]
            if op == 9 {
                std::os::windows::fs::symlink_dir(a, b)?;
            } else {
                std::os::windows::fs::symlink_file(a, b)?;
            }
        }
        10 => fs::hard_link(a, b)?,
        _ => return Err(Error::new(Kind::InvalidArgument, "invalid filesystem operation")),
    }
    Ok(())
}

/// Explicit create, delete, copy, rename, replacement, and link operations.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_mutate(op: Value, a: Value, b: Value) -> Value {
    let code = crate::unbox_int(op);
    crate::fai_drop(op);
    let left = decode_path(a);
    let right = decode_path(b);
    let pa = left.as_ref().ok().cloned();
    let pb = right.as_ref().ok().cloned();
    let outcome =
        left.and_then(|a| right.and_then(|b| run_io(move |probe| mutate(code, &a, &b, &probe))));
    crate::fai_drop(a);
    crate::fai_drop(b);
    let name = match code {
        0 => "createDirectory",
        1 => "createDirectories",
        2 => "removeFile",
        3 => "removeDirectory",
        4 => "rename",
        5 => "replace",
        6 | 7 => "copy",
        8 | 9 => "symlink",
        10 => "hardLink",
        _ => "mutate",
    };
    result(outcome, name, pa.as_deref(), if code >= 4 { pb.as_deref() } else { None }, |_| {
        crate::FAI_UNIT
    })
}

/// Open a directory cursor; final drop or explicit close releases enumeration.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_open_directory(path: Value) -> Value {
    let decoded = decode_path(path);
    let location = decoded.as_ref().ok().cloned();
    let outcome = decoded.and_then(|path| run_io(move |_| Ok((path.clone(), fs::read_dir(path)?))));
    crate::fai_drop(path);
    result(outcome, "openDirectory", location.as_deref(), None, |(path, dir)| {
        file_handle_value(path, FileState::Directory(Some(dir)))
    })
}

/// Consume one directory entry, preserving names that have no Unicode spelling.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_next_directory(cursor: Value) -> Value {
    let arc = file_arc(cursor);
    let path = arc.path.clone();
    let outcome = run_io(move |probe| {
        let mut state = arc.state.lock().expect("directory state");
        check(&probe)?;
        let FileState::Directory(dir) = &mut *state else {
            return Err(Error::new(Kind::InvalidArgument, "not a directory cursor"));
        };
        let dir = dir.as_mut().ok_or_else(closed)?;
        dir.next()
            .transpose()?
            .map(|entry| Ok((entry.file_type().ok().map(kind_of), entry.file_name(), entry.path())))
            .transpose()
    });
    crate::fai_drop(cursor);
    result(outcome, "nextDirectory", Some(&path), None, |entry| {
        some(entry.map(|(kind, name, path)| {
            data(
                0,
                &[
                    some(kind.map(crate::imm_int)),
                    crate::make_bytes(&crate::path::encode(&name)),
                    crate::make_bytes(&crate::path::encode(path.as_os_str())),
                ],
            )
        }))
    })
}

/// Close a directory cursor and expire its aliases.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_close_directory(cursor: Value) -> Value {
    close(cursor, "closeDirectory")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rename_never_overwrites_without_replacement() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::write(&a, "first").unwrap();
        fs::write(&b, "second").unwrap();
        assert_eq!(rename(&a, &b, false).unwrap_err().kind, Kind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"second");
        rename(&a, &b, true).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"first");
    }
    #[test]
    fn copying_onto_a_hard_link_does_not_truncate() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::write(&a, "keep").unwrap();
        fs::hard_link(&a, &b).unwrap();
        assert_eq!(
            copy(&a, &b, true, &cancellation_probe()).unwrap_err().kind,
            Kind::InvalidArgument
        );
        assert_eq!(fs::read(a).unwrap(), b"keep");
    }
}
