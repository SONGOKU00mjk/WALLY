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

fn main() {
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
