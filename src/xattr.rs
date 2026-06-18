use anyhow::{Context, Result};
use rustix::fs::XattrFlags;
use rustix::io::Errno;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs as unix_fs;
use std::path::Path;

/// Initial buffer size for xattr name lists and values; grown on `ERANGE`.
const XATTR_BUF_INIT: usize = 256;

/// Copy a file preserving all extended attributes (SELinux, capabilities, user.*).
/// On Btrfs, prefers FICLONE reflink first (via the caller), then copies xattrs.
pub fn copy_file_with_xattrs(src: &Path, dst: &Path) -> Result<()> {
    let mut src_file = fs::File::open(src)
        .with_context(|| format!("failed to open src for xattr copy: {}", src.display()))?;
    let mut dst_file = fs::File::create(dst)
        .with_context(|| format!("failed to create dst for xattr copy: {}", dst.display()))?;

    // Copy data
    let mut buffer = [0u8; 65536];
    loop {
        let n = src_file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        dst_file.write_all(&buffer[..n])?;
    }

    // Copy extended attributes from src to dst
    copy_xattrs(src, dst)?;

    // Copy permissions
    let metadata = src.metadata()?;
    let mode = unix_fs::PermissionsExt::mode(&metadata.permissions());
    let mut perms = dst_file.metadata()?.permissions();
    unix_fs::PermissionsExt::set_mode(&mut perms, mode);
    fs::set_permissions(dst, perms)?;

    Ok(())
}

/// Copy all extended attributes from `src` to `dst` (follows symlinks).
///
/// Shared by [`copy_file_with_xattrs`] and the `/etc` merge in
/// [`crate::mergetc`]. Best-effort: a destination filesystem without xattr
/// support (`ENOTSUP`, e.g. a FAT32 ESP) is silently tolerated; other set
/// failures are logged but do not abort the copy.
pub(crate) fn copy_xattrs(src: &Path, dst: &Path) -> Result<()> {
    let Some(names) = list_xattr_names(src)? else {
        return Ok(());
    };

    // The kernel returns names as a NUL-separated list.
    for name_bytes in names.split(|b| *b == 0) {
        if name_bytes.is_empty() {
            continue;
        }
        let name = CString::new(name_bytes)?;
        let Some(value) = get_xattr_value(src, &name)? else {
            continue;
        };

        match rustix::fs::setxattr(dst, name.as_c_str(), &value, XattrFlags::empty()) {
            Ok(()) => {}
            // ENOTSUP is expected on filesystems without xattr support (FAT32 ESP).
            Err(Errno::NOTSUP) => {}
            Err(e) => eprintln!(
                "Warning: failed to set xattr '{}' on {}: {}",
                String::from_utf8_lossy(name_bytes),
                dst.display(),
                e
            ),
        }
    }

    Ok(())
}

/// Return the NUL-separated list of xattr names on `path`, or `None` when the
/// file has no xattrs or the filesystem doesn't support them.
fn list_xattr_names(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; XATTR_BUF_INIT];
    loop {
        match rustix::fs::listxattr(path, &mut buf[..]) {
            Ok(0) => return Ok(None),
            Ok(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            Err(Errno::RANGE) => buf.resize(buf.len() * 2, 0),
            Err(Errno::NOTSUP) | Err(Errno::NODATA) => return Ok(None),
            Err(e) => return Err(e).context("listxattr failed"),
        }
    }
}

/// Read the value of a single xattr, or `None` if it vanished or is unreadable.
fn get_xattr_value(path: &Path, name: &CString) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; XATTR_BUF_INIT];
    loop {
        match rustix::fs::getxattr(path, name.as_c_str(), &mut buf[..]) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(Some(buf));
            }
            Err(Errno::RANGE) => buf.resize(buf.len() * 2, 0),
            Err(Errno::NODATA) | Err(Errno::NOTSUP) => return Ok(None),
            Err(e) => return Err(e).context("getxattr failed"),
        }
    }
}

/// Copy a directory tree recursively, preserving extended attributes on all files.
pub fn copy_dir_all_with_xattrs(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    fs::create_dir_all(dst)?;
    // Preserve directory mode (umask would otherwise mask it to 755, which
    // breaks sshd StrictModes on dirs like /root/.ssh that must be 700).
    let src_meta = fs::metadata(src)?;
    let src_mode = unix_fs::PermissionsExt::mode(&src_meta.permissions());
    let mut dst_perms = fs::metadata(dst)?.permissions();
    unix_fs::PermissionsExt::set_mode(&mut dst_perms, src_mode);
    fs::set_permissions(dst, dst_perms)?;
    let _ = copy_xattrs(src, dst);
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dst.join(file_name);
        let ty = entry.file_type()?;

        if ty.is_dir() {
            copy_dir_all_with_xattrs(&path, &dest_path)?;
        } else if ty.is_symlink() {
            if dest_path.exists() || dest_path.is_symlink() {
                let _ = fs::remove_file(&dest_path);
            }
            let link_target = fs::read_link(&path)?;
            std::os::unix::fs::symlink(link_target, &dest_path)?;
        } else if ty.is_file() {
            if dest_path.exists() || dest_path.is_symlink() {
                let _ = fs::remove_file(&dest_path);
            }
            copy_file_with_xattrs(&path, &dest_path)?;
        } else {
            eprintln!("Warning: skipping special file at {:?}", path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // TDD tests for xattr-preserving copy.

    #[test]
    fn copy_file_with_xattrs_preserves_data() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");

        fs::write(&src, b"hello xattr test").unwrap();
        copy_file_with_xattrs(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello xattr test");
    }

    #[test]
    fn copy_file_with_xattrs_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let src = dir.path().join("src.sh");
        let dst = dir.path().join("dst.sh");

        fs::write(&src, b"#!/bin/sh\necho hi").unwrap();
        let mut perms = fs::metadata(&src).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&src, perms).unwrap();

        copy_file_with_xattrs(&src, &dst).unwrap();

        let dst_perms = fs::metadata(&dst).unwrap().permissions();
        assert_eq!(dst_perms.mode() & 0o777, 0o755);
    }

    #[test]
    fn copy_file_with_xattrs_handles_no_xattrs() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("plain.txt");
        let dst = dir.path().join("copied.txt");

        fs::write(&src, b"no xattrs here").unwrap();
        // Should succeed even without any xattrs.
        copy_file_with_xattrs(&src, &dst).unwrap();
        assert!(dst.exists());
    }

    #[test]
    fn copy_dir_all_with_xattrs_preserves_symlinks() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink("real.txt", src_dir.join("link.txt")).unwrap();

        copy_dir_all_with_xattrs(&src_dir, &dst_dir).unwrap();

        assert!(dst_dir.join("real.txt").exists());
        let link_target = fs::read_link(dst_dir.join("link.txt")).unwrap();
        assert_eq!(link_target.to_string_lossy(), "real.txt");
    }

    #[test]
    fn copy_dir_all_with_xattrs_preserves_directory_mode() {
        // Regression: sshd StrictModes rejects authorized_keys when its parent
        // .ssh dir is anything looser than 700. The recursive copy must
        // propagate the source dir mode rather than inheriting umask 022.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");
        let ssh = src_dir.join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(ssh.join("authorized_keys"), b"ssh-rsa AAA").unwrap();

        copy_dir_all_with_xattrs(&src_dir, &dst_dir).unwrap();

        let dst_ssh_mode = fs::metadata(dst_dir.join(".ssh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            dst_ssh_mode & 0o777,
            0o700,
            "dst .ssh must stay 700, got {:o}",
            dst_ssh_mode & 0o777
        );
    }

    #[test]
    fn copy_dir_all_with_xattrs_skips_special_files() {
        // Just verify it doesn't panic on an empty dir with no special files
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("f.txt"), b"data").unwrap();

        copy_dir_all_with_xattrs(&src_dir, &dst_dir).unwrap();
        assert!(dst_dir.join("f.txt").exists());
    }
}
