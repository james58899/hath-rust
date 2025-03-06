use std::{
    fs::File,
    io::{Error, ErrorKind, Read, Result},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures::{FutureExt, Stream, ready};
use tokio::{runtime::Handle, task::JoinHandle};

type TaskResult = (File, BytesMut);
type TaskError = (Error, File, BytesMut);

async fn read_file(mut file: File, mut buf: BytesMut, capacity: usize) -> std::result::Result<TaskResult, TaskError> {
    // Init buffer
    buf.clear();
    buf.reserve(capacity);
    unsafe { buf.set_len(capacity) }; // SAFETY: File::read() only write buffer

    match file.read(&mut buf) {
        Ok(size) => {
            buf.truncate(size); // Set buffer to real size
            Ok((file, buf))
        }
        Err(err) => Err((err, file, buf)),
    }
}

pub struct FileReader {
    state: ReaderState,
    capacity: usize,
    io_pool: Handle,
}

enum ReaderState {
    State(Option<TaskResult>),
    Future(JoinHandle<std::result::Result<TaskResult, TaskError>>),
}

impl FileReader {
    pub fn new(io_pool: Handle, file: File, capacity: usize) -> Self {
        Self {
            state: ReaderState::State(Some((file, BytesMut::with_capacity(capacity)))),
            capacity,
            io_pool,
        }
    }

    pub async fn read(&mut self) -> Result<Bytes> {
        // Take state
        let ReaderState::State(ref mut state) = self.state else {
            return Err(Error::new(ErrorKind::ResourceBusy, "Double read"));
        };
        let (file, buf) = state.take().ok_or_else(|| Error::new(ErrorKind::ResourceBusy, "Double read"))?;
        let capacity = self.capacity;

        match self.io_pool.spawn(read_file(file, buf, capacity)).await? {
            Ok((file, mut buf)) => {
                let bytes = buf.split().freeze();
                state.replace((file, buf));
                Ok(bytes)
            }
            Err((err, file, buf)) => {
                state.replace((file, buf));
                Err(err)
            }
        }
    }
}

impl Stream for FileReader {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.state {
                ReaderState::State(ref mut state) => {
                    // Take state and spawn task
                    let (file, buf) = state.take().ok_or_else(|| Error::new(ErrorKind::ResourceBusy, "Double read"))?;
                    let capacity = self.capacity;
                    let task = self.io_pool.spawn(read_file(file, buf, capacity));
                    self.state = ReaderState::Future(task); // Move state to wait background task finish
                }
                ReaderState::Future(ref mut task) => match ready!(task.poll_unpin(cx)) {
                    Ok(Ok((file, mut buf))) => {
                        let bytes = buf.split().freeze();
                        self.state = ReaderState::State(Some((file, buf))); // Reset state
                        if bytes.is_empty() {
                            return Poll::Ready(None); // EOF
                        }
                        return Poll::Ready(Some(Ok(bytes))); // Success
                    }
                    Ok(Err((err, file, buf))) => {
                        self.state = ReaderState::State(Some((file, buf))); // Reset state
                        return Poll::Ready(Some(Err(err))); // Read error
                    }
                    Err(err) => return Poll::Ready(Some(Err(err.into()))),
                },
            }
        }
    }
}
