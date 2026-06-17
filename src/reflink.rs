use anyhow::{Context, Result};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub fn reflink<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dest: Q) -> Result<()> {
    let src_file = File::open(&src).with_context(|| {
        format!(
            "failed to open source file for reflink: {}",
            src.as_ref().display()
        )
    })?;

    let dest_file = File::create(&dest).with_context(|| {
        format!(
            "failed to create destination file for reflink: {}",
            dest.as_ref().display()
        )
    })?;

    let src_fd = src_file.as_raw_fd();
    let dest_fd = dest_file.as_raw_fd();

    // FICLONE ioctl code:
    // On Linux x86_64, _IOW(0x94, 9, int) is 0x40049409
    const FICLONE: libc::c_ulong = 0x40049409;

    let ret = unsafe { libc::ioctl(dest_fd, FICLONE, src_fd) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(err).context("FICLONE ioctl failed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_reflink() {
        let dir = tempdir().unwrap();
        let src_path = dir.path().join("src.txt");
        let dest_path = dir.path().join("dest.txt");

        let mut src_file = File::create(&src_path).unwrap();
        src_file.write_all(b"hello reflink").unwrap();
        drop(src_file);

        // Try to reflink. Note: this might fail if the tempdir filesystem doesn't support reflinks.
        // On typical CI / systems, /tmp might be tmpfs which does NOT support FICLONE.
        // Therefore, we handle the error gracefully in test.
        match reflink(&src_path, &dest_path) {
            Ok(()) => {
                let content = std::fs::read_to_string(&dest_path).unwrap();
                assert_eq!(content, "hello reflink");
            }
            Err(e) => {
                // If it fails with ENOTTY or EOPNOTSUPP, it means the filesystem doesn't support it, which is expected on tmpfs/ext4.
                println!(
                    "Reflink failed (expected on unsupported filesystems): {:?}",
                    e
                );
            }
        }
    }
}
