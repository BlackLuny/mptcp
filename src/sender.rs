use std::{collections::VecDeque, io};

use async_async_io::write::{AsyncAsyncWrite, PollWrite};
use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    task::JoinSet,
};

use crate::{message::Sequence, send_buf::SendStreamBuf};

pub struct Sender<W> {
    streams: VecDeque<W>,
    next: Sequence,
}

impl<W> Sender<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(streams: Vec<W>) -> Self {
        Self {
            streams: streams.into(),
            next: Sequence::new(0),
        }
    }

    pub async fn batch_send(&mut self, send_buf: &mut SendStreamBuf) -> Result<(), SendError> {
        if self.streams.is_empty() {
            return Err(SendError::NoStreamLeft);
        }

        let mut write_tasks: JoinSet<io::Result<_>> = JoinSet::new();
        let segments = send_buf.iter_unsent_segments();

        for segment in segments {
            let mut stream = match self.streams.pop_front() {
                Some(stream) => stream,
                None => break,
            };

            write_tasks.spawn(async move {
                segment.encode(&mut stream).await?;

                Ok((segment.start_sequence(), stream))
            });
        }

        let mut io_errors = vec![];
        while let Some(task) = write_tasks.join_next().await {
            let res = task.unwrap();
            match res {
                Ok((sequence, stream)) => {
                    self.streams.push_back(stream);
                    send_buf.mark_as_sent(sequence);
                }
                Err(e) => {
                    io_errors.push(e);
                }
            }
        }
        if !io_errors.is_empty() {
            return Err(SendError::Io(io_errors));
        }
        Ok(())
    }

    pub async fn batch_send_all(&mut self, data: Bytes) -> Result<(), NoStreamLeft> {
        let data_len = data.len();
        let mut send_buf = SendStreamBuf::new(data, self.next);
        send_buf.split_first_unsent_segment(self.streams.len());

        loop {
            let res = self.batch_send(&mut send_buf).await;
            match res {
                Ok(()) => (),
                Err(SendError::NoStreamLeft) => return Err(NoStreamLeft),
                _ => continue,
            }
            if send_buf.done() {
                self.next = Sequence::new(self.next.inner() + data_len as u64);
                return Ok(());
            }
        }
    }

    pub fn into_async_write(self) -> PollWrite<Self> {
        PollWrite::new(self)
    }
}

#[async_trait]
impl<W> AsyncAsyncWrite for Sender<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        {
            // SAFETY: `data` will be dropped outside of this scope
            let data = Bytes::from_static(unsafe { std::mem::transmute(buf) });
            self.batch_send_all(data)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        }
        Ok(buf.len())
    }

    async fn flush(&mut self) -> io::Result<()> {
        for stream in &mut self.streams {
            stream.flush().await?;
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        for stream in &mut self.streams {
            stream.shutdown().await?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SendError {
    #[error("No stream left")]
    NoStreamLeft,
    #[error("Stream I/O errors")]
    Io(Vec<io::Error>),
}

#[derive(Debug, Error)]
#[error("No stream left")]
pub struct NoStreamLeft;
