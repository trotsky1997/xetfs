fn main() {
    #[cfg(windows)]
    winfsp_wrs_build::build();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_FUSE").is_some()
    {
        println!(
            "cargo:rustc-link-search=framework=/Library/Filesystems/macfuse.fs/Contents/Frameworks"
        );
    }
}
