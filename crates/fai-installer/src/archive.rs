//! Exact-member archive extraction; archive paths are never used as destination paths.

use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::Path,
};

use fai_toolchain::{Result, file_digest, invalid};

pub fn extract(archive: &Path, destination: &Path, windows: bool, expected: &str) -> Result<()> {
    std::fs::create_dir_all(destination.join("bin"))?;
    let executable = if windows { "bin/fai.exe" } else { "bin/fai" };
    let mut seen = BTreeSet::new();
    let mut write = |name: &str, size: u64, reader: &mut dyn Read| -> Result<()> {
        if !matches!(name, "LICENSE" | "metadata.json") && name != executable {
            return Err(invalid(format!("unexpected archive member: {name}")));
        }
        if !seen.insert(name.to_owned()) || size > 2 * 1024 * 1024 * 1024 {
            return Err(invalid("duplicate or oversized archive member"));
        }
        if name != executable && size > 8 * 1024 * 1024 {
            return Err(invalid("archive metadata exceeds the size limit"));
        }
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination.join(name))?;
        if std::io::copy(&mut reader.take(size + 1), &mut output)? != size {
            return Err(invalid("archive member size mismatch"));
        }
        output.flush()?;
        output.sync_all()?;
        Ok(())
    };
    if windows {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)
            .map_err(|error| invalid(error.to_string()))?;
        if zip.len() != 3 {
            return Err(invalid("release archive must contain exactly three files"));
        }
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index).map_err(|error| invalid(error.to_string()))?;
            if entry.is_dir()
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 != 0 && mode & 0o170000 != 0o100000)
            {
                return Err(invalid("release archive contains a non-regular file"));
            }
            let name = entry.name().to_owned();
            write(&name, entry.size(), &mut entry)?;
        }
    } else {
        let gzip = flate2::read::GzDecoder::new(std::fs::File::open(archive)?);
        let mut tar = tar::Archive::new(gzip);
        for entry in tar.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                return Err(invalid("release archive contains a non-regular file"));
            }
            let path = entry.path_bytes();
            let name = std::str::from_utf8(&path)
                .map_err(|_| invalid("archive path is not UTF-8"))?
                .to_owned();
            write(&name, entry.size(), &mut entry)?;
        }
    }
    if seen
        != BTreeSet::from([executable.to_owned(), "LICENSE".to_owned(), "metadata.json".to_owned()])
    {
        return Err(invalid("release archive is incomplete"));
    }
    let executable = destination.join(executable);
    if file_digest(&executable)? != expected {
        return Err(invalid("extracted executable checksum mismatch"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_fixture(
        extra: Option<(&str, tar::EntryType)>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("compiler.tar.gz");
        let gzip = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(gzip);
        let mut append = |name: &str, kind, bytes: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(kind);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append(&header, bytes).unwrap();
        };
        append("bin/fai", tar::EntryType::Regular, b"compiler");
        append("metadata.json", tar::EntryType::Regular, b"{}");
        append("LICENSE", tar::EntryType::Regular, b"license");
        if let Some((name, kind)) = extra {
            append(name, kind, b"");
        }
        archive.into_inner().unwrap().finish().unwrap();
        (root, path)
    }

    #[test]
    fn valid_archive_extracts_only_declared_files() {
        let (root, archive) = tar_fixture(None);
        let destination = root.path().join("out");
        extract(&archive, &destination, false, &fai_toolchain::digest(b"compiler")).unwrap();
        assert_eq!(std::fs::read(destination.join("bin/fai")).unwrap(), b"compiler");
    }

    #[test]
    fn parent_paths_are_rejected_before_writing_outside_staging() {
        let (root, archive) = tar_fixture(Some(("../outside", tar::EntryType::Regular)));
        assert!(
            extract(&archive, &root.path().join("out"), false, &fai_toolchain::digest(b"compiler"))
                .is_err()
        );
        assert!(!root.path().join("outside").exists());
    }

    #[test]
    fn duplicate_executables_are_rejected() {
        let (root, archive) = tar_fixture(Some(("bin/fai", tar::EntryType::Regular)));
        assert!(
            extract(&archive, &root.path().join("out"), false, &fai_toolchain::digest(b"compiler"))
                .is_err()
        );
    }

    #[test]
    fn symbolic_links_are_rejected() {
        let (root, archive) = tar_fixture(Some(("link", tar::EntryType::Symlink)));
        assert!(
            extract(&archive, &root.path().join("out"), false, &fai_toolchain::digest(b"compiler"))
                .is_err()
        );
    }

    #[test]
    fn executable_checksum_is_checked_after_extraction() {
        let (root, archive) = tar_fixture(None);
        assert!(extract(&archive, &root.path().join("out"), false, &"0".repeat(64)).is_err());
    }

    fn zip_fixture(license_name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("compiler.zip");
        let mut archive = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        archive.start_file("bin/fai.exe", options).unwrap();
        archive.write_all(b"compiler").unwrap();
        archive.start_file("metadata.json", options).unwrap();
        archive.write_all(b"{}").unwrap();
        archive.start_file(license_name, options).unwrap();
        archive.write_all(b"license").unwrap();
        archive.finish().unwrap();
        (root, path)
    }

    #[test]
    fn windows_zip_is_extracted_with_the_executable_identity_intact() {
        let (root, archive) = zip_fixture("LICENSE");
        let destination = root.path().join("out");
        extract(&archive, &destination, true, &fai_toolchain::digest(b"compiler")).unwrap();
        assert_eq!(std::fs::read(destination.join("bin/fai.exe")).unwrap(), b"compiler");
    }

    #[test]
    fn windows_zip_paths_cannot_escape_staging() {
        let (root, archive) = zip_fixture("../outside");
        assert!(
            extract(&archive, &root.path().join("out"), true, &fai_toolchain::digest(b"compiler"))
                .is_err()
        );
        assert!(!root.path().join("outside").exists());
    }
}
