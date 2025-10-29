# hath-rust
[![Build](../../actions/workflows/build.yml/badge.svg)](../../actions/workflows/build.yml) [![Docker Pulls](https://img.shields.io/docker/pulls/james58899/hath-rust)](https://hub.docker.com/r/james58899/hath-rust)

The unofficial Hentai@Home client written in Rust.

## Install
Read the [Wiki](https://github.com/james58899/hath-rust/wiki/Install)

## Features
### New
Features not included in the official.
* Lower memory usage
* Parallel async cache scan
* Seamless certificate update
* Using ChaCha20 on hardware without AES acceleration
* Send filename to browser[^filename]
* Monitoring endpoints ([wiki](https://github.com/james58899/hath-rust/wiki/Monitoring))
* Strict SNI checking (Default off, may reduce quality)
* Post-quantum cryptography[^pqc]
* HTTP/3 (Experimental)

### Works
Features that are included in the official and are working.
* Cache and Proxy
* Gallery downloader
* Speed test
* Cache size management
* Logging
* Disk space check
* Download cache files through proxy
* Flood control

### No planned
* HTTP/2[^h2]
* Bandwidth limit

## Platform support
Please refer to the rust website for the platform name: https://doc.rust-lang.org/stable/rustc/platform-support.html

### Tier 1
Main supported platforms.  
Tested in real environments before release.

|          Platform         |
| ------------------------- |
| x86_64-unknown-linux-gnu  |
| x86_64-unknown-linux-musl |

### Tier 2
Secondary supported platforms.  
Due to the lack of hardware or real environment, it was not tested before release, relying on users to report bugs.

|            Platform            |
| ------------------------------ |
| aarch64-unknown-linux-gnu      |
| aarch64-unknown-linux-musl     |
| armv7-unknown-linux-gnueabihf  |
| armv7-unknown-linux-musleabihf |
| x86_64-pc-windows-msvc         |
| i686-pc-windows-msvc           |
| x86_64-apple-darwin            |
| aarch64-apple-darwin           |

### Tier 3
Experimental platform.  
Not guaranteed to work, may break at any time.

|         Platform        |
| ----------------------- |
| aarch64-linux-android   |
| armv7-linux-androideabi |
| i686-linux-android      |
| x86_64-linux-android    |


[^h2]: Multiplexing is useless for H@H, and a large number of connections will take up more system resources.
[^filename]: If the filename is not sent, some browsers may download using the wrong name.
[^pqc]: Support X25519MLKEM768 key agreement