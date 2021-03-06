use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    lock::Mutex,
    stream::FusedStream,
    FutureExt, SinkExt, StreamExt,
};
use std::{
    fmt::{Debug, Formatter},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{io::Error, prelude::*};

pub struct ClientSslPackets {
    incoming_reader: IncomingReader, // read here to decrypt request
    outgoing_writer: OutgoingWriter, // write here to send encrypted request
}

impl Debug for ClientSslPackets {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            r#"ClientSslPackets
            incoming_reader {:?}
            outgoing_writer {:?}"#,
            self.incoming_reader, self.outgoing_writer
        )
    }
}

#[derive(Debug, Clone)]
pub struct ClientSslPacketsChannels {
    pub incoming_writer: Arc<Mutex<IncomingWriter>>,
    pub outgoing_reader: Arc<Mutex<OutgoingReader>>,
}

pub type IncomingWriter = UnboundedSender<Vec<u8>>;
pub type IncomingReader = UnboundedReceiver<Vec<u8>>;

pub type OutgoingReader = UnboundedReceiver<Vec<u8>>;
pub type OutgoingWriter = UnboundedSender<Vec<u8>>;

impl ClientSslPackets {
    pub fn new() -> (ClientSslPackets, ClientSslPacketsChannels) {
        let (incoming_writer, incoming_reader): (IncomingWriter, IncomingReader) = unbounded();
        let (outgoing_writer, outgoing_reader): (OutgoingWriter, OutgoingReader) = unbounded();

        let ssl_stream = ClientSslPackets {
            incoming_reader,
            outgoing_writer,
        };

        let incoming_writer = Arc::new(Mutex::new(incoming_writer));
        let outgoing_reader = Arc::new(Mutex::new(outgoing_reader));
        let ssl_channel = ClientSslPacketsChannels {
            incoming_writer,
            outgoing_reader,
        };

        (ssl_stream, ssl_channel)
    }
}

impl AsyncRead for ClientSslPackets {
    fn poll_read<'a>(
        mut self: Pin<&'a mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.incoming_reader.is_terminated() {
            return Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into()));
        }

        match self.incoming_reader.poll_next_unpin(cx) {
            Poll::Ready(Some(message)) => {
                if buf.len() < message.len() {
                    return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()));
                }
                buf[0..message.len()].copy_from_slice(&message);
                Poll::Ready(Ok(message.len()))
            }
            Poll::Ready(None) => Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for ClientSslPackets {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self
            .get_mut()
            .outgoing_writer
            .send(buf.to_vec())
            .poll_unpin(cx)
        {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::ErrorKind::WriteZero.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut()
            .outgoing_writer
            .poll_flush_unpin(cx)
            .map_err(|e| {
                if e.is_disconnected() {
                    std::io::ErrorKind::ConnectionAborted.into()
                } else {
                    std::io::ErrorKind::Other.into()
                }
            })
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.get_mut()
            .outgoing_writer
            .poll_close_unpin(cx)
            .map_err(|e| {
                if e.is_disconnected() {
                    std::io::ErrorKind::ConnectionAborted.into()
                } else {
                    std::io::ErrorKind::Other.into()
                }
            })
    }
}
