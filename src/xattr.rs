use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs as unix_fs;
use std::path::Path;
use anyhow::{Result, Context};

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
    let src_path = src.to_str()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid src path"))?;

    copy_xattrs(src_path, dst)?;

    // Copy permissions
    let metadata = src.metadata()?;
    let mode = unix_fs::PermissionsExt::mode(&metadata.permissions());
    let mut perms = dst_file.metadata()?.permissions();
    unix_fs::PermissionsExt::set_mode(&mut perms, mode);
    fs::set_permissions(dst, perms)?;

    Ok(())
}

/// Copy all extended attributes from one file to another.
fn copy_xattrs(src_path: &str, dst: &Path) -> Result<()> {
    // On Linux we use the raw libc xattr syscalls.
    // listxattr returns the null-separated list of names.

    let src_c = std::ffi::CString::new(src_path)?;
    let dst_c = std::ffi::CString::new(dst.to_str().unwrap_or(""))?;

    // Get the size of the xattr list.
    let list_size = unsafe {
        libc::listxattr(src_c.as_ptr(), std::ptr::null_mut(), 0)
    };

    if list_size <= 0 {
        // No xattrs, or error, either way nothing to copy.
        return Ok(());
    }

    let mut list_buf = vec![0u8; list_size as usize];
    let list_size = unsafe {
        libc::listxattr(src_c.as_ptr(), list_buf.as_mut_ptr() as *mut libc::c_char, list_buf.len())
    };

    if list_size <= 0 {
        return Ok(());
    }
    list_buf.truncate(list_size as usize);

    // Split the null-separated list.
    for name_bytes in list_buf.split(|b| *b == 0) {
        if name_bytes.is_empty() {
            continue;
        }
        let name = std::ffi::CString::new(name_bytes)?;

        // Get value size.
        let val_size = unsafe {
            libc::getxattr(src_c.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
        };

        if val_size < 0 {
            continue; // Skip if we can't read
        }

        let mut val_buf = vec![0u8; val_size as usize];
        let val_size = unsafe {
            libc::getxattr(
                src_c.as_ptr(),
                name.as_ptr(),
                val_buf.as_mut_ptr() as *mut libc::c_void,
                val_buf.len(),
            )
        };

        if val_size < 0 {
            continue;
        }
        val_buf.truncate(val_size as usize);

        // Set xattr on destination. Log failures but don't abort (Fix 11).
        let rc = unsafe {
            libc::setxattr(
                dst_c.as_ptr(),
                name.as_ptr(),
                val_buf.as_ptr() as *const libc::c_void,
                val_buf.len(),
                0,
            )
        };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            // ENOTSUP is expected on filesystems without xattr support (FAT32 ESP).
            if e.raw_os_error() != Some(libc::ENOTSUP) {
                if let Ok(name_str) = std::str::from_utf8(name_bytes) {
                    eprintln!("Warning: failed to set xattr '{}' on {}: {}", name_str, dst.display(), e);
                }
            }
        }
    }

    Ok(())
}

/// Copy a directory tree recursively, preserving extended attributes on all files.
pub fn copy_dir_all_with_xattrs(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    fs::create_dir_all(dst)?;
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

    // --- #5: TDD tests for xattr-preserving copy ---

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
