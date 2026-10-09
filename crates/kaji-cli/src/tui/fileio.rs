use anyhow::{bail, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

pub fn open_regular(path: &Path) -> Result<File> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        bail!("{} est un dossier", path.display());
    }
    if !metadata.is_file() {
        bail!("{}: only regular files can be read", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    // A path can become a FIFO between stat and open. O_NONBLOCK keeps that
    // race from waiting for a writer; the descriptor's type is checked below.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("{}: only regular files can be read", path.display());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pipes_and_devices_are_refused_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.txt");
        assert!(std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        assert!(open_regular(&path)
            .unwrap_err()
            .to_string()
            .contains("regular"));
        assert!(open_regular(Path::new("/dev/zero")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_regular_file_is_readable_but_a_symlink_to_a_device_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "text").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_regular(&link).is_ok());
        let device = dir.path().join("device");
        std::os::unix::fs::symlink("/dev/zero", &device).unwrap();
        assert!(open_regular(&device).is_err());
    }
}
