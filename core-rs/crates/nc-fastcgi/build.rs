//! Build script: embed the packaged default location of the PHP bootstrap shim.
//!
//! At runtime `nc-fastcgi` resolves the shim path in this order:
//!
//! 1. `NC_PHP_SHIM` environment variable (deployment override; full path to
//!    the shim's `index.php`).
//! 2. The packaged default embedded here — used only if the file exists.
//! 3. The in-tree development layout `{nc_root}/core-rs/php-shim/index.php`.
//!
//! Distro packagers retarget (2) at build time by setting `NCSHIMDIR` to the
//! package's architecture-independent data directory:
//!
//! ```sh
//! NCSHIMDIR=/usr/share/nc-server cargo build --release
//! ```
//!
//! The shim then defaults to `$NCSHIMDIR/php-shim/index.php`.  The built-in
//! default follows the FHS (`/usr/share` for architecture-independent
//! read-only data), matching the layout documented in `packaging/README.md`.
//!
//! The `rerun-if-env-changed` directive is the reason this is a build script
//! rather than a bare `option_env!` in the crate: without it cargo does not
//! fingerprint `NCSHIMDIR`, and rebuilding with a changed value would
//! silently reuse the stale baked-in path.

fn main() {
    println!("cargo:rerun-if-env-changed=NCSHIMDIR");

    let share_dir =
        std::env::var("NCSHIMDIR").unwrap_or_else(|_| "/usr/share/nc-server".to_owned());
    let shim_path = std::path::Path::new(&share_dir)
        .join("php-shim")
        .join("index.php");
    println!(
        "cargo:rustc-env=NC_DEFAULT_SHIM_PATH={}",
        shim_path.display()
    );
}
