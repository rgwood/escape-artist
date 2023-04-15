set shell := ["nu", "-c"]

# this doesn't quite work perfectly because the terminal is stuck in raw mode after the restart
watch:
    watchexec --exts=rs,js,html,css --on-busy-update=restart -- cargo run

run:
    cargo run

test:
    cargo test

watch-tests:
    watch . { cargo test } --glob=**/*.rs

expected_filename := if os_family() == "windows" { "escape-artist.exe" } else { "escape-artist" }

build-release:
    cargo build --release
    ls target/release

publish-to-local-bin: build-release
    cp target/release/{{expected_filename}} ~/bin/

build-linux-x64:
    cross build --target x86_64-unknown-linux-gnu --release

build-linux-arm64:
    cross build --target aarch64-unknown-linux-gnu --release

build-windows-on-linux:
    cross build --target x86_64-pc-windows-gnu --release
