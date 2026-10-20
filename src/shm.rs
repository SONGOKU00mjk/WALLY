//! A memfd-backed shm file, mmapped for direct pixel writes.
//!
//! wally needs exactly one file descriptor per output: an anonymous,
//! close-on-exec memory file that wl_shm pools are created from.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd};

pub struct ShmFile {
    file: File,
    ptr: *mut u8,
    len: usize,
}

// The raw pointer is only dereferenced through `slice()`, which takes
// `&mut self`; the file is process-local shared memory and never leaves
// the single-threaded event loop.
unsafe impl Send for ShmFile {}
unsafe impl Sync for ShmFile {}

impl ShmFile {
    pub fn new(len: usize) -> io::Result<Self> {
        let fd = unsafe {
            libc::memfd_create(c"wally-shm".as_ptr(), libc::MFD_CLOEXEC)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64)?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            // `file` drops here and closes the fd.
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            ptr: ptr as *mut u8,
            len,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The fd handed to `wl_shm.create_pool`.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// The whole buffer as a shared byte slice.
    pub fn slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The whole buffer as a mutable byte slice.
    pub fn slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for ShmFile {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_and_roundtrips_bytes() {
        let mut shm = ShmFile::new(64).unwrap();
        assert_eq!(shm.len(), 64);
        shm.slice_mut()[0] = 42;
        shm.slice_mut()[63] = 7;
        assert_eq!(shm.slice()[0], 42);
        assert_eq!(shm.slice()[63], 7);
    }
}
