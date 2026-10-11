//! Exclusive temporary resources, retained by native owners and explicitly
//! transferable with persistence. Cleanup closes all writer aliases first.

use super::*;
use ring::rand::{SecureRandom, SystemRandom};
use std::fs;

pub(super) struct Temporary {
    path: Option<PathBuf>,
    directory: bool,
    writer: Option<Arc<FileObject>>,
    failure: Option<Error>,
}

impl Temporary {
    fn close_writer(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.take()
            && let Err(error) = close_state(&mut writer.state.lock().expect("temporary writer"))
        {
            self.failure = Some(error);
        }
        self.failure.clone().map_or(Ok(()), Err)
    }

    pub(super) fn close(&mut self) -> Result<()> {
        let flushed = self.close_writer();
        if let Some(path) = &self.path {
            let removed =
                if self.directory { fs::remove_dir_all(path) } else { fs::remove_file(path) };
            match removed {
                Ok(()) => self.path = None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.path = None,
                Err(error) => {
                    return Err(match flushed {
                        Ok(()) => error.into(),
                        Err(mut first) => {
                            first.message.push_str(&format!("; cleanup: {error}"));
                            first
                        }
                    });
                }
            }
        }
        flushed
    }

    fn persist(
        &mut self,
        destination: &Path,
        overwrite: bool,
        probe: &CancellationProbe,
    ) -> Result<PathBuf> {
        self.close_writer()?;
        check(probe)?;
        let path = self.path.as_ref().ok_or_else(closed)?;
        directory::rename(path, destination, overwrite)?;
        self.path = None;
        Ok(destination.to_owned())
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        // Final-drop cleanup is a fallback without an error channel. Move the
        // remaining native owners to the blocking pool, bypassing cancellation.
        let path = self.path.take();
        let directory = self.directory;
        let writer = self.writer.take();
        let work = move || {
            if let Some(writer) = writer {
                let _ = close_state(&mut writer.state.lock().expect("temporary writer"));
            }
            if let Some(path) = path {
                let _ = if directory { fs::remove_dir_all(path) } else { fs::remove_file(path) };
            }
        };
        if crate::scheduler::in_task() {
            crate::scheduler::run_blocking(Box::new(work));
        } else {
            work();
        }
    }
}

fn create(parent: &Path, directory: bool, probe: &CancellationProbe) -> Result<Temporary> {
    let parent = fs::canonicalize(parent)?;
    let entropy = SystemRandom::new();
    for _ in 0..128 {
        check(probe)?;
        let mut random = [0u8; 16];
        entropy
            .fill(&mut random)
            .map_err(|_| Error::new(Kind::IoFailure, "temporary name entropy failed"))?;
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = parent.join(format!("fai-{name}"));
        let created = if directory {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&path).map(|_| None)
        } else {
            let mut builder = OpenOptions::new();
            builder.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                builder.mode(0o600);
            }
            builder.open(&path).map(Some)
        };
        match created {
            Ok(file) => {
                let writer = file.map(|file| {
                    Arc::new(FileObject {
                        path: path.clone(),
                        state: Mutex::new(FileState::Writer(Some(BufWriter::new(file)))),
                    })
                });
                return Ok(Temporary { path: Some(path), directory, writer, failure: None });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::new(Kind::AlreadyExists, "temporary name collision limit exceeded"))
}

/// Atomically create a temporary file or directory in an explicit parent.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_temp_create(parent: Value, directory: Value) -> Value {
    let is_directory = crate::unbox_int(directory) != 0;
    crate::fai_drop(directory);
    let decoded = decode_path(parent);
    let path = decoded.as_ref().ok().cloned();
    let outcome =
        decoded.and_then(|parent| run_io(move |probe| create(&parent, is_directory, &probe)));
    crate::fai_drop(parent);
    result(outcome, "createTemporary", path.as_deref(), None, |temp| {
        file_handle_value(
            temp.path.as_ref().expect("new temporary path").clone(),
            FileState::Temporary(Box::new(temp)),
        )
    })
}

/// Obtain the still-owned temporary resource's native path.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_temp_path(resource: Value) -> Value {
    let arc = file_arc(resource);
    let outcome = match &*arc.state.lock().expect("temporary state") {
        FileState::Temporary(temp) => temp.path.clone().ok_or_else(closed),
        _ => Err(Error::new(Kind::InvalidArgument, "not a temporary resource")),
    };
    crate::fai_drop(resource);
    result(outcome, "temporaryPath", Some(&arc.path), None, |path| {
        crate::make_bytes(&crate::path::encode(path.as_os_str()))
    })
}

/// Obtain a shared writer alias for a live temporary file.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_temp_writer(resource: Value) -> Value {
    let arc = file_arc(resource);
    let outcome = match &*arc.state.lock().expect("temporary state") {
        FileState::Temporary(temp) => temp.writer.clone().ok_or_else(closed),
        _ => Err(Error::new(Kind::InvalidArgument, "not a temporary file")),
    };
    crate::fai_drop(resource);
    result(outcome, "temporaryWriter", Some(&arc.path), None, file_handle_arc)
}

/// Close writer aliases and delete the temporary entry if it has not persisted.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_temp_close(resource: Value) -> Value {
    close(resource, "closeTemporary")
}

/// Transfer a temporary resource with an explicit replacement policy. Failure
/// before successful rename leaves the resource owned for cleanup.
#[unsafe(no_mangle)]
pub extern "C" fn fai_fs_temp_persist(
    resource: Value,
    destination: Value,
    overwrite: Value,
) -> Value {
    let arc = file_arc(resource);
    let path = arc.path.clone();
    let decoded = decode_path(destination);
    let target = decoded.as_ref().ok().cloned();
    let replace = crate::unbox_int(overwrite) != 0;
    crate::fai_drop(overwrite);
    let outcome = decoded.and_then(|destination| {
        run_io(move |probe| match &mut *arc.state.lock().expect("temporary state") {
            FileState::Temporary(temp) => temp.persist(&destination, replace, &probe),
            _ => Err(Error::new(Kind::InvalidArgument, "not a temporary resource")),
        })
    });
    crate::fai_drop(resource);
    crate::fai_drop(destination);
    result(outcome, "persistTemporary", Some(&path), target.as_deref(), |path| {
        crate::make_bytes(&crate::path::encode(path.as_os_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_drop_removes_an_owned_directory() {
        let parent = tempfile::tempdir().unwrap();
        let temp = create(parent.path(), true, &cancellation_probe()).unwrap();
        let path = temp.path.clone().unwrap();
        fs::write(path.join("child"), "content").unwrap();
        drop(temp);
        assert!(!path.exists());
    }
    #[test]
    fn failed_persistence_keeps_ownership() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("existing");
        fs::write(&destination, "keep").unwrap();
        let mut temp = create(parent.path(), false, &cancellation_probe()).unwrap();
        let path = temp.path.clone().unwrap();
        assert!(temp.persist(&destination, false, &cancellation_probe()).is_err());
        assert_eq!(temp.path.as_ref(), Some(&path));
        drop(temp);
        assert!(!path.exists());
        assert_eq!(fs::read(destination).unwrap(), b"keep");
    }
}
