//@ignore-target-windows: no libc on Windows
//@compile-flags: -Zmiri-disable-isolation

#![feature(io_error_more)]
#![feature(io_error_uncategorized)]

use std::convert::TryInto;
use std::ffi::CString;
use std::fs::{canonicalize, remove_dir_all, remove_file, File};
use std::io::{Error, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

#[path = "../../utils/mod.rs"]
mod utils;

fn main() {
    test_dup_stdout_stderr();
    test_canonicalize_too_long();
    test_readlink();
    test_file_open_unix_allow_two_args();
    test_file_open_unix_needs_three_args();
    test_file_open_unix_extra_third_arg();
    #[cfg(target_os = "linux")]
    test_o_tmpfile_flag();
    test_posix_mkstemp();
}

/// Prepare: compute filename and make sure the file does not exist.
fn prepare(filename: &str) -> PathBuf {
    let path = utils::tmp().join(filename);
    // Clean the paths for robustness.
    remove_file(&path).ok();
    path
}

/// Prepare directory: compute directory name and make sure it does not exist.
#[allow(unused)]
fn prepare_dir(dirname: &str) -> PathBuf {
    let path = utils::tmp().join(&dirname);
    // Clean the directory for robustness.
    remove_dir_all(&path).ok();
    path
}

/// Prepare like above, and also write some initial content to the file.
fn prepare_with_content(filename: &str, content: &[u8]) -> PathBuf {
    let path = prepare(filename);
    let mut file = File::create(&path).unwrap();
    file.write(content).unwrap();
    path
}

fn test_file_open_unix_allow_two_args() {
    let path = prepare_with_content("test_file_open_unix_allow_two_args.txt", &[]);

    let mut name = path.into_os_string();
    name.push("\0");
    let name_ptr = name.as_bytes().as_ptr().cast::<libc::c_char>();
    let _fd = unsafe { libc::open(name_ptr, libc::O_RDONLY) };
}

fn test_file_open_unix_needs_three_args() {
    let path = prepare_with_content("test_file_open_unix_needs_three_args.txt", &[]);

    let mut name = path.into_os_string();
    name.push("\0");
    let name_ptr = name.as_bytes().as_ptr().cast::<libc::c_char>();
    let _fd = unsafe { libc::open(name_ptr, libc::O_CREAT, 0o666) };
}

fn test_file_open_unix_extra_third_arg() {
    let path = prepare_with_content("test_file_open_unix_extra_third_arg.txt", &[]);

    let mut name = path.into_os_string();
    name.push("\0");
    let name_ptr = name.as_bytes().as_ptr().cast::<libc::c_char>();
    let _fd = unsafe { libc::open(name_ptr, libc::O_RDONLY, 42) };
}

fn test_dup_stdout_stderr() {
    let bytes = b"hello dup fd\n";
    unsafe {
        let new_stdout = libc::fcntl(1, libc::F_DUPFD, 0);
        let new_stderr = libc::fcntl(2, libc::F_DUPFD, 0);
        libc::write(new_stdout, bytes.as_ptr() as *const libc::c_void, bytes.len());
        libc::write(new_stderr, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}

fn test_canonicalize_too_long() {
    // Make sure we get an error for long paths.
    let too_long = "x/".repeat(libc::PATH_MAX.try_into().unwrap());
    assert!(canonicalize(too_long).is_err());
}

fn test_readlink() {
    let bytes = b"Hello, World!\n";
    let path = prepare_with_content("miri_test_fs_link_target.txt", bytes);
    let expected_path = path.as_os_str().as_bytes();

    let symlink_path = prepare("miri_test_fs_symlink.txt");
    std::os::unix::fs::symlink(&path, &symlink_path).unwrap();

    // Test that the expected string gets written to a buffer of proper
    // length, and that a trailing null byte is not written.
    let symlink_c_str = CString::new(symlink_path.as_os_str().as_bytes()).unwrap();
    let symlink_c_ptr = symlink_c_str.as_ptr();

    // Make the buf one byte larger than it needs to be,
    // and check that the last byte is not overwritten.
    let mut large_buf = vec![0xFF; expected_path.len() + 1];
    let res =
        unsafe { libc::readlink(symlink_c_ptr, large_buf.as_mut_ptr().cast(), large_buf.len()) };
    // Check that the resolved path was properly written into the buf.
    assert_eq!(&large_buf[..(large_buf.len() - 1)], expected_path);
    assert_eq!(large_buf.last(), Some(&0xFF));
    assert_eq!(res, large_buf.len() as isize - 1);

    // Test that the resolved path is truncated if the provided buffer
    // is too small.
    let mut small_buf = [0u8; 2];
    let res =
        unsafe { libc::readlink(symlink_c_ptr, small_buf.as_mut_ptr().cast(), small_buf.len()) };
    assert_eq!(small_buf, &expected_path[..small_buf.len()]);
    assert_eq!(res, small_buf.len() as isize);

    // Test that we report a proper error for a missing path.
    let bad_path = CString::new("MIRI_MISSING_FILE_NAME").unwrap();
    let res = unsafe {
        libc::readlink(bad_path.as_ptr(), small_buf.as_mut_ptr().cast(), small_buf.len())
    };
    assert_eq!(res, -1);
    assert_eq!(Error::last_os_error().kind(), ErrorKind::NotFound);
}

#[cfg(target_os = "linux")]
fn test_o_tmpfile_flag() {
    use std::fs::{create_dir, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;
    let dir_path = prepare_dir("miri_test_fs_dir");
    create_dir(&dir_path).unwrap();
    // test that the `O_TMPFILE` custom flag gracefully errors instead of stopping execution
    assert_eq!(
        Some(libc::EOPNOTSUPP),
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE)
            .open(dir_path)
            .unwrap_err()
            .raw_os_error(),
    );
}

fn test_posix_mkstemp() {
    use std::ffi::OsStr;
    use std::os::unix::io::FromRawFd;
    use std::path::Path;

    let valid_template = "fooXXXXXX";
    // C needs to own this as `mkstemp(3)` says:
    // "Since it will be modified, `template` must not be a string constant, but
    // should be declared as a character array."
    // There seems to be no `as_mut_ptr` on `CString` so we need to use `into_raw`.
    let ptr = CString::new(valid_template).unwrap().into_raw();
    let fd = unsafe { libc::mkstemp(ptr) };
    // Take ownership back in Rust to not leak memory.
    let slice = unsafe { CString::from_raw(ptr) };
    assert!(fd > 0);
    let osstr = OsStr::from_bytes(slice.to_bytes());
    let path: &Path = osstr.as_ref();
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.ne("fooXXXXXX"));
    assert!(name.starts_with("foo"));
    assert_eq!(name.len(), 9);
    assert_eq!(
        name.chars().skip(3).filter(char::is_ascii_alphanumeric).collect::<Vec<char>>().len(),
        6
    );
    let file = unsafe { File::from_raw_fd(fd) };
    assert!(file.set_len(0).is_ok());

    let invalid_templates = vec!["foo", "barXX", "XXXXXXbaz", "whatXXXXXXever", "X"];
    for t in invalid_templates {
        let ptr = CString::new(t).unwrap().into_raw();
        let fd = unsafe { libc::mkstemp(ptr) };
        let _ = unsafe { CString::from_raw(ptr) };
        // "On error, -1 is returned, and errno is set to
        // indicate the error"
        assert_eq!(fd, -1);
        let e = std::io::Error::last_os_error();
        assert_eq!(e.raw_os_error(), Some(libc::EINVAL));
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    }
}
