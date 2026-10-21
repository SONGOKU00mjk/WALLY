//! A memfd-backed shm file, mmapped for direct pixel writes.
//!
//! wally needs exactly one file descriptor per output: an anonymous,
//! close-on-exec memory file that wl_shm pools are created from.
//!
//! The framebuffer does not have to live in this process's RSS. The
//! compositor reads the pool through its own mapping of the same memfd,
//! so after a commit wally can drop the pages from its page tables
//! ([`ShmFile::relax`]) and the on-screen wallpaper is unaffected — the
//! bytes live on in the memfd. A still wallpaper therefore costs RSS
//! only while it is actually being drawn, which is how a pure-CPU shm
//! client keeps the same accounting advantage a GPU-based daemon gets
//! from keeping its framebuffer in VRAM.

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
        let fd = unsafe { libc::memfd_create(c"wally-shm".as_ptr(), libc::MFD_CLOEXEC) };
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

    /// Drop this mapping's pages out of the process RSS.
    ///
    /// `MADV_DONTNEED` on a `MAP_SHARED` file mapping only zaps the page
    /// table entries. The data itself lives in the memfd (tmpfs), not in
    /// this mapping — the compositor keeps reading it through its own
    /// mapping, and the next [`Self::slice`] / [`Self::slice_mut`] call
    /// transparently re-faults the pages. Nothing is lost; this is
    /// purely an RSS statement: between repaints the framebuffer is
    /// charged to no one's resident set but exists in the file.
    ///
    /// Never call this on a buffer that doubles as a GIF compositing
    /// canvas (hosted mode) — it is written every step and would just
    /// re-fault for nothing. Errors are ignored: this is best-effort.
    pub fn relax(&self) {
        unsafe {
            libc::madvise(self.ptr as *mut libc::c_void, self.len, libc::MADV_DONTNEED);
        }
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

    /// `relax` must be invisible to the data: bytes written before it
    /// read back identically after it, including across re-faulted
    /// pages. Multi-page so MADV_DONTNEED actually has range to drop.
    #[test]
    fn relax_keeps_bytes_in_the_file() {
        let mut shm = ShmFile::new(1 << 20).unwrap();
        shm.slice_mut()[0] = 42;
        shm.slice_mut()[4096] = 9;
        shm.slice_mut()[(1 << 20) - 1] = 7;
        shm.relax();
        assert_eq!(shm.slice()[0], 42);
        assert_eq!(shm.slice()[4096], 9);
        assert_eq!(shm.slice()[(1 << 20) - 1], 7);
        // And the mapping stays writable afterwards.
        shm.slice_mut()[1] = 5;
        assert_eq!(shm.slice()[1], 5);
    }
}
