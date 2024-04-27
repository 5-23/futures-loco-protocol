pub mod secure;
pub mod session;

pub use loco_protocol;

use futures_core::Future;
use futures_io::{AsyncRead, AsyncWrite};
use loco_protocol::command::{
    client::{LocoSink, LocoStream, StreamState},
    BoxedCommand, Command, Header, Method,
};
use std::{
    future::poll_fn,
    io::{self, ErrorKind},
    mem,
    pin::Pin,
    task::{ready, Context, Poll},
};

pin_project_lite::pin_project!(
    #[derive(Debug, Clone)]
    pub struct LocoClient<T: Clone> {
        current_id: u32,

        sink: LocoSink,
        stream: LocoStream,

        read_state: ReadState,

        #[pin]
        inner: T,
    }
);

impl<T: Clone> LocoClient<T> {
    pub const MAX_READ_SIZE: u64 = 16 * 1024 * 1024;

    pub const fn new(inner: T) -> Self {
        Self {
            current_id: 0,

            sink: LocoSink::new(),
            stream: LocoStream::new(),

            read_state: ReadState::Pending,

            inner,
        }
    }

    pub const fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn inner_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        self.project().inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: AsyncRead + Clone> LocoClient<T> {
    pub async fn read(&mut self) -> io::Result<BoxedCommand>
    where
        T: Unpin,
    {
        let mut this = Pin::new(self);

        poll_fn(|cx| this.as_mut().poll_read(cx)).await
    }

    pub fn poll_read(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<BoxedCommand>> {
        let mut this = self.project();

        let mut buffer = [0_u8; 1024];
        loop {
            match mem::replace(this.read_state, ReadState::Corrupted) {
                ReadState::Pending => match this.stream.read() {
                    Some(command) => {
                        *this.read_state = ReadState::Pending;
                        break Poll::Ready(Ok(command));
                    }

                    None => {
                        if let StreamState::Header(header) = this.stream.state() {
                            if header.data_size as u64 > Self::MAX_READ_SIZE {
                                *this.read_state = ReadState::PacketTooLarge;
                                continue;
                            }
                        }

                        *this.read_state = ReadState::Pending;

                        let read = ready!(this.inner.as_mut().poll_read(cx, &mut buffer))?;
                        if read == 0 {
                            *this.read_state = ReadState::Done;
                            continue;
                        }

                        this.stream.read_buffer.extend(&buffer[..read]);
                    }
                },

                ReadState::PacketTooLarge => {
                    *this.read_state = ReadState::PacketTooLarge;

                    break Poll::Ready(Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "packet is too large",
                    )));
                }

                ReadState::Done => break Poll::Ready(Err(ErrorKind::UnexpectedEof.into())),

                ReadState::Corrupted => unreachable!(),
            }
        }
    }
}

impl<T: AsyncWrite + Clone> LocoClient<T> {
    pub async fn send(&mut self, method: Method, data: &[u8]) -> io::Result<u32>
    where
        T: Unpin,
    {
        let mut this = Pin::new(self);

        let id = this.as_mut().write(method, data);

        poll_fn(|cx| this.as_mut().poll_flush(cx)).await?;

        Ok(id)
    }

    pub fn write(self: Pin<&mut Self>, method: Method, data: &[u8]) -> u32 {
        let this = self.project();

        let id = {
            *this.current_id += 1;

            *this.current_id
        };

        this.sink.send(Command {
            header: Header {
                id,
                status: 0,
                method,
                data_type: 0,
            },
            data,
        });

        id
    }

    pub fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let mut this = self.project();

        while !this.sink.write_buffer.is_empty() {
            let written = ready!(this.inner.as_mut().poll_write(cx, {
                let slices = this.sink.write_buffer.as_slices();

                if !slices.0.is_empty() {
                    slices.0
                } else {
                    slices.1
                }
            }))?;

            this.sink.write_buffer.drain(..written);
        }

        ready!(this.inner.poll_flush(cx))?;

        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Clone> LocoClient<T> {
    pub async fn request(
        &mut self,
        method: Method,
        data: &[u8],
    ) -> io::Result<impl Future<Output = io::Result<BoxedCommand>> + '_> {
        let mut this = Pin::new(self);

        let id = this.as_mut().write(method, data);

        poll_fn(|cx| this.as_mut().poll_flush(cx)).await?;

        let read_task = async move {
            Ok(loop {
                let read = poll_fn(|cx| this.as_mut().poll_read(cx)).await?;

                if read.header.id == id {
                    break read;
                }
            })
        };

        Ok(read_task)
    }
}

#[derive(Debug, Clone)]
enum ReadState {
    Pending,
    PacketTooLarge,
    Done,
    Corrupted,
}
