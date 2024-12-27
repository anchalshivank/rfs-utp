use std::io;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use tokio::net::ToSocketAddrs;
use log::info;

pub struct UtpSocket {
    socket: tokio::net::UdpSocket
}

impl UtpSocket {
    pub async fn bind<A>(addr: A) -> Result<Self, tokio::io::Error>
    where A: ToSocketAddrs {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        Ok(Self { socket })
    }

    pub async fn connect<A>(addr: A) -> Result<Self, tokio::io::Error>
    where A: ToSocketAddrs {
        let udp_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let remote_addr = tokio::net::lookup_host(addr)
            .await?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "could not resolve to any address"))?;

        udp_socket.connect(remote_addr).await?;
        info!("Connected to {} and my addr is {}", remote_addr, udp_socket.local_addr()?);

        let socket = UtpSocket { socket: udp_socket };
        let s = "hell my name is shivgank";
        socket.send_to(s.as_bytes(), remote_addr).await?;

        Ok(socket)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        match self.socket.send_to(buf, target).await {
            Ok(bytes_sent) => {
                log::info!("Successfully sent {} bytes to {}", bytes_sent, target);
                Ok(bytes_sent)
            }
            Err(e) => {
                log::error!("Failed to send data to {}: {}", target, e);
                Err(e)
            }
        }
    }


    pub fn poll_send_to(&self, ctx: &mut Context, mut buf: &[u8], peer_addr: SocketAddr) -> Poll<io::Result<usize>> {

        self.socket.poll_send_to(ctx, buf, peer_addr)

    }
}