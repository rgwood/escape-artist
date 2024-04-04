fn main() {
    // https://github.com/wrapperup/iconify-rs/issues/38
    if is_docker::is_docker() {
        println!("cargo:rustc-env=ICONIFY_CACHE_DIR=/tmp/iconify/");
    }
}