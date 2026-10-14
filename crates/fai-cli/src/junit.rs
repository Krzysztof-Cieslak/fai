//! Client-owned atomic publication of a completed JUnit artifact.

use std::io::{self, Write};
use std::path::Path;

pub(crate) fn preparation_error(
    error: &fai_driver::DriverError,
    opts: fai_driver::RenderOpts,
) -> String {
    let rendered = fai_driver::render_workspace_error(error, opts);
    fai_driver::junit_error_report(&format!("{}{}", rendered.stdout, rendered.stderr))
}

pub(crate) fn write_report(path: &Path, xml: &str) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new().prefix(".fai-junit-").tempfile_in(parent)?;
    temporary.write_all(xml.as_bytes())?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_a_report_and_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/report.xml");
        write_report(&path, "first").unwrap();
        write_report(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn failed_publication_leaves_an_existing_destination_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.xml");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "original").unwrap();
        assert!(write_report(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(path.join("keep")).unwrap(), "original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
