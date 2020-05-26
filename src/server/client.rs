use bytes::BytesMut;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::io::Error;
use futures::lock::Mutex;
use futures::task::{Context, Poll};
use futures::{FutureExt, SinkExt, Stream, StreamExt};
use log::warn;
use openssl::error::ErrorStack;
use openssl::ssl::{SslAcceptor, SslRef};
use srtp::{CryptoPolicy, Srtp, SsrcType};
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::macros::support::Pin;
use tokio::time::{timeout, Duration};
use tokio_openssl::accept;

#[derive(Debug)]
pub struct Client {
    pub addr: SocketAddr,
    pub ssl_state: ClientSslState,
    pub channels: ClientSslPacketsChannels,
}

impl Client {
    pub fn new(addr: SocketAddr, handshake: Vec<u8>) -> Client {
        let (ssl_state, channels) = ClientSslPackets::new();
        let ssl_state = ClientSslState::Empty(ssl_state, handshake);

        Client {
            addr,
            channels,
            ssl_state,
        }
    }
}

pub async fn connect(
    ssl_state: ClientSslPackets,
    handshake: Vec<u8>,
    mut incoming_writer: IncomingWriter,
    ssl_acceptor: Arc<SslAcceptor>,
) -> std::io::Result<impl Stream<Item = Vec<u8>>> {
    match incoming_writer.send(handshake).await {
        Ok(()) => {}
        Err(err) => warn!("{:?}", err),
    }

    let ssl_stream = timeout(Duration::from_secs(10), accept(&ssl_acceptor, ssl_state)).await?;

    let ssl_stream = match ssl_stream {
        Ok(s) => s,
        Err(e) => {
            warn!("handshake error: {:?}", e);
            return Err(std::io::ErrorKind::ConnectionAborted.into());
        }
    };

    let (srtp_reader, srtp_writer) = get_srtp(ssl_stream.ssl()).unwrap();

    warn!("end of handshake");

    Ok(futures::stream::unfold(
        (ssl_stream, srtp_reader, srtp_writer),
        |(mut ssl_stream, mut srtp_reader, srtp_writer)| async move {
            let mut buf = vec![0; 0x10000];

            match ssl_stream.get_mut().read(&mut buf).await {
                Ok(n) => {
                    if n == 0 {
                        return None;
                    }
                    buf.truncate(n);

                    let mut buf = BytesMut::from(buf.as_slice());

                    println!("{:?}", srtp_reader.unprotect(&mut buf));

                    Some((buf.to_vec(), (ssl_stream, srtp_reader, srtp_writer)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    warn!("long message");
                    None
                }
                Err(_) => None,
            }
        },
    ))
}

#[derive(Debug)]
pub enum ClientSslState {
    Connected,
    Shutdown,
    Empty(ClientSslPackets, Vec<u8>),
}

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

#[derive(Debug)]
pub struct ClientSslPacketsChannels {
    pub incoming_writer: IncomingWriter,
    pub outgoing_reader: Arc<Mutex<OutgoingReader>>,
}

pub type IncomingWriter = UnboundedSender<Vec<u8>>;
pub type IncomingReader = UnboundedReceiver<Vec<u8>>;

pub type OutgoingReader = UnboundedReceiver<Vec<u8>>;
pub type OutgoingWriter = UnboundedSender<Vec<u8>>;

impl ClientSslPackets {
    fn new() -> (ClientSslPackets, ClientSslPacketsChannels) {
        let (incoming_writer, incoming_reader): (IncomingWriter, IncomingReader) = unbounded();
        let (outgoing_writer, outgoing_reader): (OutgoingWriter, OutgoingReader) = unbounded();

        let ssl_stream = ClientSslPackets {
            incoming_reader,
            outgoing_writer,
        };

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
        match self.get_mut().outgoing_writer.flush().poll_unpin(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.get_mut().outgoing_writer.close().poll_unpin(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn get_srtp(ssl: &SslRef) -> Result<(Srtp, Srtp), ErrorStack> {
    let rtp_policy = CryptoPolicy::AesCm128HmacSha1Bit80;
    let rtcp_policy = CryptoPolicy::AesCm128HmacSha1Bit80;

    println!("{}", ssl.selected_srtp_profile().unwrap().name());

    let mut dtls_buf = vec![0; rtp_policy.master_len() * 2];
    ssl.export_keying_material(dtls_buf.as_mut_slice(), "EXTRACTOR-dtls_srtp", None)?;

    let pair = rtp_policy.extract_keying_material(dtls_buf.as_mut_slice());

    let srtp_incoming =
        Srtp::new(SsrcType::AnyInbound, rtp_policy, rtcp_policy, pair.client).unwrap();
    let srtp_outcoming =
        Srtp::new(SsrcType::AnyOutbound, rtp_policy, rtcp_policy, pair.server).unwrap();

    Ok((srtp_incoming, srtp_outcoming))
}
