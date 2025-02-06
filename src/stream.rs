use crate::socket::UtpSocket;
use futures::AsyncWrite;
use futures::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, Interest, ReadBuf, Ready};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
//Wrapper over UtpStream
pub struct UtpStream {
    socket: UtpSocket,
    receiver: Receiver<Vec<u8>>,
}

impl UtpStream {
    pub async fn bind(addr: Option<SocketAddr>) -> UtpStream {
        let (sender, receiver) = mpsc::channel(100);
        let socket = UtpSocket::bind(addr, Some(sender)).await;
        let r = socket.clone();

        tokio::spawn(async move {
            r.start_receiving().await;
        });
        Self { socket, receiver }
    }

    pub async fn connect(&mut self, addr: SocketAddr) -> io::Result<()> {
        self.socket.connect(addr).await
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        self.socket.ready(interest).await
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.receiver.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let amt = std::cmp::min(buf.remaining(), data.len());
                buf.put_slice(&data[..amt]);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        // Create a pinned future
        let mut send_future = Box::pin(this.socket.send_packets(buf, None));

        // Poll the future
        match send_future.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
