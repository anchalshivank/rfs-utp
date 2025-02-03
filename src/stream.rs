use crate::socket::UtpSocket;
use futures::AsyncWrite;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::ToSocketAddrs;

pub struct UtpStream {
    socket: UtpSocket
}

impl UtpStream {
    pub async fn bind(addr: Option<SocketAddr>) -> UtpStream {
        let socket = UtpSocket::bind(addr).await;
        Self { socket }
    }

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {

        self.socket.connect(addr).await

    }

}

impl AsyncRead for UtpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        this.socket.recv_packets_from(cx, buf).map(|_| Ok(()))
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        println!("writing --------- {:?}", buf);
        let  this = self.get_mut();

        if let Err(error) = this.socket.send_packets(cx, buf) {
            return Poll::Ready(Err(error));
        }

        Poll::Ready(Ok(buf.len()))

    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
