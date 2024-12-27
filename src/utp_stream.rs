use std::future::Future;
use std::io::{Error, Read};
use crate::utp_socket::UtpSocket;
use bytes::{Buf, Bytes, BytesMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use log::{debug, error, info};
use tokio::io;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use std::net::SocketAddr;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

const UDP_BUFFER_SIZE: usize = 17480;
const CHANNEL_LEN: usize = 100;

pub struct UtpStream {
    pub socket: Arc<UtpSocket>,
    handler: Option<JoinHandle<()>>,
    receiver: Arc<Mutex<Receiver<Bytes>>>,
    remaining: Option<Bytes>,
    drop: Option<Sender<Bytes>>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr
}

async fn to_socket_addr<A>(addr: A) -> io::Result<SocketAddr>
where
    A: tokio::net::ToSocketAddrs,
{
    let mut addrs = tokio::net::lookup_host(addr).await?;
    addrs.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Could not resolve remote address",
        )
    })
}

impl UtpStream {
    pub async fn bind<A>(addr: A) -> Result<UtpStream, tokio::io::Error>
    where
        A: tokio::net::ToSocketAddrs + Clone
    {
        debug!("Starting UTP stream server");
        let socket = UtpSocket::bind(addr.clone()).await?;
        let peer_addr = to_socket_addr(addr).await?;
        Self::from_tokio(socket, peer_addr).await
    }

    pub async fn connect<A>(addr: A) -> Result<UtpStream, tokio::io::Error>
    where
        A: tokio::net::ToSocketAddrs + Clone
    {
        debug!("Starting UTP stream server connection");
        let peer_addr = to_socket_addr(addr).await?;
        let socket = UtpSocket::connect(peer_addr).await?;
        Self::from_tokio(socket, peer_addr).await

    }

    pub async fn from_tokio(socket: UtpSocket, peer_addr: SocketAddr) -> Result<UtpStream, tokio::io::Error>
    {
        let socket = Arc::new(socket);
        let local_addr = socket.local_addr()?;
        let socket_inner = socket.clone();
        let (child_tx, child_rx) = mpsc::channel(CHANNEL_LEN);

        let handler = tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(UDP_BUFFER_SIZE);
            // let mut established_peer = None;

            loop {
                match socket_inner.recv_from(buf.as_mut()).await {
                    Ok((len, received_addr)) => {
                        let received_data = String::from_utf8_lossy(&buf[..len]);
                        info!("------------------------------- {} and data is  {} and len is {}", received_addr, received_data, len);
                        // if established_peer.is_none() {
                        //     log::info!("Establishing peer address: {}", received_addr);
                        //     established_peer = Some(received_addr);
                        // }

                        // if Some(received_addr) != established_peer {
                        //     let received_data = String::from_utf8_lossy(&buf[..len]);
                        //     log::info!("Received data: '{}' from unexpected peer: {}", received_data, received_addr);
                        //     continue;
                        // }

                        if child_tx.send(buf.split_to(len).freeze()).await.is_err() {
                            log::warn!("Receiver channel closed");
                            break;
                        }

                        // if buf.capacity() < UDP_BUFFER_SIZE {
                        //     buf.reserve(UDP_BUFFER_SIZE);
                        // }
                    }
                    Err(e) => {
                        log::error!("Error receiving data: {}", e);
                        break;
                    }
                }
            }
        });



        Ok(
            UtpStream {
                local_addr,
                peer_addr,
                receiver: Arc::new(Mutex::new(child_rx)),
                socket: socket.clone(),
                handler: Some(handler),
                drop: None,
                remaining: None,
            }
        )
    }

    pub fn peer_addr(&self) -> Result<SocketAddr, tokio::io::Error> {

        Ok(self.peer_addr)

    }

    pub fn local_addr(&self) -> Result<SocketAddr, tokio::io::Error> {
        Ok(self.local_addr)
    }

}

impl AsyncRead for UtpStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        info!("found data ReadBuf {{ filled: {}, initialized: {}, capacity: {} }}",
            buf.filled().len(),
            buf.initialized().len(),
            buf.capacity()
        );
        if let Some(remaining) = self.remaining.as_mut() {

            if buf.remaining() < remaining.len() {

                buf.put_slice(&remaining.split_to(buf.remaining())[..]);

            }else{
                buf.put_slice(&remaining[..]);
                self.remaining = None;
            }
            return Poll::Ready(Ok(()));
        }
        let receiver = self.receiver.clone();

        let mut socket = match Pin::new(&mut Box::pin(receiver.lock())).poll(cx){
            Poll::Ready(socket) => socket,
            Poll::Pending => return Poll::Pending,
        };

        match socket.poll_recv(cx) {
            Poll::Ready(Some(mut inner_buf)) => {
                if buf.remaining() < inner_buf.len(){

                    self.remaining = Some(inner_buf.split_off(buf.remaining()));

                };

                buf.put_slice(&inner_buf[..]);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
            Poll::Pending => Poll::Pending,
        }

    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8]
    ) -> Poll<Result<usize, io::Error>> {
        // Log what we're trying to write
        info!("Attempting to write {} bytes", buf.len());
        if let Ok(data) = String::from_utf8(buf.to_vec()) {
            info!("Writing data: '{}'", data);
        }

        match self.socket.poll_send_to(cx, buf, self.peer_addr) {
            Poll::Ready(Ok(n)) => {
                info!("Successfully wrote {} bytes", n);
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => {
                info!("Write error: {}", e);
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // UDP is connectionless, no need to flush
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // If we have a drop channel, send the shutdown signal
        if let Some(tx) = &self.drop {
            if tx.is_closed() {
                return Poll::Ready(Ok(()));
            }
        }

        // Clean up the handler if it exists
        if let Some(handler) = self.get_mut().handler.take() {
            handler.abort();
        }

        Poll::Ready(Ok(()))
    }
}