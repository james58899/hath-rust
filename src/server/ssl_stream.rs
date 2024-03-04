use std::{
    io,
    mem::ManuallyDrop,
    os::fd::AsRawFd,
    pin::{pin, Pin},
    ptr,
    task::{
        ready, Context,
        Poll::{self, Ready},
    },
};

use foreign_types_shared::ForeignType;
use futures::Future;
use openssl::{
    error::ErrorStack,
    ssl::{ErrorCode, Ssl},
};
use openssl_sys::{
    BIO_ctrl, BIO_new_socket, SSL_accept, SSL_get_error, SSL_get_wbio, SSL_peek_ex, SSL_read_ex, SSL_set_bio, SSL_shutdown, SSL_write_ex,
    BIO_CTRL_FLUSH,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, Interest, ReadBuf},
    net::TcpStream,
};

pub struct SslStream {
    ssl: ManuallyDrop<Ssl>,
    stream: TcpStream,
}

impl Drop for SslStream {
    fn drop(&mut self) {
        // ssl holds a reference to method internally so it has to drop first
        unsafe {
            ManuallyDrop::drop(&mut self.ssl);
        }
    }
}

impl SslStream {
    pub fn new(ssl: Ssl, stream: TcpStream) -> Result<Self, ErrorStack> {
        let bio = unsafe { cvt_p(BIO_new_socket(stream.as_raw_fd(), 0))? };
        unsafe {
            SSL_set_bio(ssl.as_ptr(), bio, bio);
        }
        Ok(Self {
            ssl: ManuallyDrop::new(ssl),
            stream,
        })
    }

    pub async fn accept(&self) -> io::Result<()> {
        self.stream
            .async_io(Interest::READABLE, || {
                let ret = unsafe { SSL_accept(self.ssl.as_ptr()) };
                if ret > 0 {
                    return Ok(());
                }
                match ErrorCode::from_raw(unsafe { SSL_get_error(self.ssl.as_ptr(), ret) }) {
                    ErrorCode::SSL | ErrorCode::SYSCALL => Err(ErrorStack::get().into()),
                    ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(io::ErrorKind::WouldBlock.into()),
                    _ => Ok(()),
                }
            })
            .await
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream
            .async_io(Interest::READABLE, || {
                let mut readbytes = 0;
                let ret = unsafe { SSL_peek_ex(self.ssl.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), &mut readbytes) };
                if ret > 0 {
                    return Ok(readbytes);
                }
                let code = ErrorCode::from_raw(unsafe { SSL_get_error(self.ssl.as_ptr(), ret) });
                match code {
                    ErrorCode::ZERO_RETURN => Ok(0),
                    ErrorCode::SSL | ErrorCode::SYSCALL => Err(ErrorStack::get().into()),
                    ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(io::ErrorKind::WouldBlock.into()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unknown error code {}", code.as_raw()),
                    )),
                }
            })
            .await
    }

    /// Returns a shared reference to the underlying stream.
    pub fn get_ref(&self) -> &TcpStream {
        &self.stream
    }
}

impl AsyncRead for SslStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            let res = self.stream.try_io(Interest::READABLE, || {
                let mut readbytes = 0;
                let buf2 = unsafe { buf.unfilled_mut() }; // Trust OpenSSL
                let ret = unsafe { SSL_read_ex(self.ssl.as_ptr(), buf2.as_mut_ptr().cast(), buf2.len(), &mut readbytes) };
                if ret > 0 {
                    unsafe { buf.assume_init(readbytes) };
                    buf.advance(readbytes);
                    return Ok(());
                }
                let code = ErrorCode::from_raw(unsafe { SSL_get_error(self.ssl.as_ptr(), ret) });
                match code {
                    ErrorCode::ZERO_RETURN => Ok(()),
                    ErrorCode::SSL | ErrorCode::SYSCALL => Err(ErrorStack::get().into()),
                    ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(io::ErrorKind::WouldBlock.into()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unknown error code {}", code.as_raw()),
                    )),
                }
            });

            if res.is_err() && res.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock {
                ready!(self.stream.poll_read_ready(cx))?;
            } else {
                return Ready(res);
            }
        }
    }
}

impl AsyncWrite for SslStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        loop {
            let res = self.stream.try_io(Interest::WRITABLE, || {
                let mut written = 0;
                let ret = unsafe { SSL_write_ex(self.ssl.as_ptr(), buf.as_ptr().cast(), buf.len(), &mut written) };
                if ret > 0 {
                    return Ok(written);
                }
                let code = ErrorCode::from_raw(unsafe { SSL_get_error(self.ssl.as_ptr(), ret) });
                match code {
                    ErrorCode::ZERO_RETURN => Ok(0),
                    ErrorCode::SSL | ErrorCode::SYSCALL => Err(ErrorStack::get().into()),
                    ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(io::ErrorKind::WouldBlock.into()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unknown error code {}", code.as_raw()),
                    )),
                }
            });

            if res.is_err() && res.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock {
                ready!(self.stream.poll_write_ready(cx))?;
            } else {
                return Ready(res);
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let bio = unsafe { SSL_get_wbio(self.ssl.as_ptr()) }; // wbio and rbio is same

        loop {
            let res = self.stream.try_io(Interest::WRITABLE, || {
                let ret = unsafe { BIO_ctrl(bio, BIO_CTRL_FLUSH, 0, ptr::null_mut()) } as i32;
                if ret == 1 {
                    return Ok(());
                }
                let code = ErrorCode::from_raw(unsafe { SSL_get_error(self.ssl.as_ptr(), ret) });
                match code {
                    ErrorCode::ZERO_RETURN => Ok(()),
                    ErrorCode::SSL | ErrorCode::SYSCALL => Err(ErrorStack::get().into()),
                    ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(io::ErrorKind::WouldBlock.into()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unknown error code {}", code.as_raw()),
                    )),
                }
            });

            if res.is_err() && res.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock {
                ready!(self.stream.poll_write_ready(cx))?;
            } else {
                return Ready(res);
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        loop {
            let res = self.stream.try_io(Interest::WRITABLE, || {
                let ret = unsafe { SSL_shutdown(self.ssl.as_ptr()) };
                if ret >= 0 {
                    return Ok(());
                }
                let code = ErrorCode::from_raw(unsafe { SSL_get_error(self.ssl.as_ptr(), ret) });
                match code {
                    ErrorCode::ZERO_RETURN => Ok(()),
                    ErrorCode::SSL | ErrorCode::SYSCALL => Err(ErrorStack::get().into()),
                    ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(io::ErrorKind::WouldBlock.into()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unknown error code {}", code.as_raw()),
                    )),
                }
            });

            if res.is_err() && res.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock {
                ready!(self.stream.poll_write_ready(cx))?;
            } else {
                if res.is_ok() {
                    ready!(pin!(self.get_mut().stream.shutdown()).poll(cx))?;
                }
                return Ready(res);
            }
        }
    }
}

#[inline]
fn cvt_p<T>(r: *mut T) -> Result<*mut T, ErrorStack> {
    if r.is_null() {
        Err(ErrorStack::get())
    } else {
        Ok(r)
    }
}
