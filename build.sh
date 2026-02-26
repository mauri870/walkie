#!/usr/bin/env bash
set -e

cargo build --target x86_64-pc-windows-gnu --release
zip -j walkie-windows.zip target/x86_64-pc-windows-gnu/release/walkie.exe
cargo install --path=.
