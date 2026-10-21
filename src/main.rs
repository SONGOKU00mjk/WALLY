//! wally — a minimal Wayland wallpaper setter.
//!
//! Modules:
//! - [`cli`]: swaybg-style argument parsing into a per-output [`cli::Config`]
//! - [`source`]: image loading (static, GIF, animated WebP, AVIF, JPEG XL)
//! - [`render`]: pure composition into XRGB8888 buffers
//! - [`shm`]: memfd-backed framebuffers
//! - [`wayland`]: layer-shell surfaces and the event loop

mod cli;
mod render;
mod shm;
mod source;
mod wayland;

/// Keep the glibc allocator honest about large allocations. Without
/// this, glibc raises its mmap threshold after the first big block is
/// freed and starts serving multi-megabyte buffers from the sbrk arena,
/// where freed pages stay resident — a still wallpaper would keep its
/// 8 MB decode buffer in RSS forever. Pinning the threshold keeps every
/// large buffer a direct mmap, so freeing it returns the pages to the
/// OS immediately: the same behavior the musl build had. (musl has no
/// mallopt and its allocator already behaves this way.)
pub fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
    }
}

fn main() {
    tune_allocator();

    match cli::parse() {
        cli::Parsed::Help(text) => print!("{text}"),
        cli::Parsed::Version => println!("wally {}", env!("CARGO_PKG_VERSION")),
        cli::Parsed::Run(config) => {
            if let Err(e) = wayland::run(config) {
                eprintln!("wally: {e:#}");
                std::process::exit(1);
            }
        }
    }
}
