# hath-rust
[![Build](../../actions/workflows/build.yml/badge.svg)](../../actions/workflows/build.yml)

The unofficial Hentai@Home client written in Rust.

## Install
Read the [Wiki](https://github.com/james58899/hath-rust/wiki/Install)

## Features
### New
Features not included in the official.
* Lower memory usage
* Parallel async cache scan
* TLS 1.3
* Seamless certificate update
* Using ChaCha20 on hardware without AES acceleration
* Send filename to browser[^filename]

### Works
Features that are included in the official and are working.
* Cache and Proxy
* Gallery downloader
* Speed test
* Cache size management
* Logging
* Disk space check
* Download cache files through proxy

### No planned
* HTTP/2[^h2]
* Bandwidth limit

## Platform support
The following conditions will be passed before release.

* Build: CI build success
* Run: Check binary runable
* Test: Test on real environment

|           Platform            | Build |  Run  | Test  |
| ----------------------------- | :---: | :---: | :---: |
| x86_64-unknown-linux-gnu      |  ✅   |  ✅  |  ✅   |
| aarch64-unknown-linux-gnu     |  ✅   |  ❌  |  ❌   |
| armv7-unknown-linux-gnueabihf |  ✅   |  ❌  |  ❌   |
| x86_64-pc-windows-msvc        |  ✅   |  ✅  |  ❌   |
| i686-pc-windows-msvc          |  ✅   |  ✅  |  ❌   |
| x86_64-apple-darwin           |  ✅   |  ❌  |  ❌   |
| aarch64-apple-darwin          |  ✅   |  ✅  |  ❌   |

See https://doc.rust-lang.org/stable/rustc/platform-support.html


[^h2]: Multiplexing is useless for H@H, and a large number of connections will take up more system resources.
[^filename]: If the filename is not sent, some browsers may download using the wrong name.