# hath-rust

Hentai@Home but rusty.

**Under development, stability is not guaranteed.**

Unofficial.

## Features
### New
Features not included in the official.
* Lower memory usage
* Parallel async cache scan
* TLS 1.3
* Seamless certificate update
* Using ChaCha20 on hardware without AES acceleration

### Works
Features that are included in the official and are working.
* Cache and Proxy
* Gallery downloader
* Speed test
* Cache size management
* Logging

### Not works
Included in the official release but not yet implemented.
* Initialize client[^init]
* Disk space check[^disk]

### No planned
* HTTP/2[^h2]
* Bandwidth limit


[^init]: Need to manually create a `client_login` file in the data directory.
[^disk]: Only checks the cache size and does not aware of downloads or other space usages.
[^h2]: Multiplexing is useless for H@H, and a large number of connections will take up more system resources.