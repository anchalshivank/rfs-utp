use std::collections::HashMap;
use std::future::Future;
use std::io::{Error, Read};
use crate::socket::UtpSocket;
use bytes::{Buf, Bytes, BytesMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use log::{debug, error, info};
use tokio::io;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use std::net::SocketAddr;
use futures::{pin_mut, FutureExt};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

const UDP_BUFFER_SIZE: usize = 17480;
const CHANNEL_LEN: usize = 100;

pub struct UtpListener{
    handler: JoinHandle<()>,
    receiver: Arc<Mutex<mpsc::Receiver<(UtpStream, SocketAddr)>>>,
    local_addr: SocketAddr
}

impl Drop for UtpListener {

    fn drop(&mut self){
        self.handler.abort();
    }

}

impl UtpListener {
    pub async fn bind(local_addr: SocketAddr) -> io::Result<UtpListener> {

        let utp_socket = UtpSocket::bind(local_addr).await?;
        //define handler and received things
        Self::from_tokio(utp_socket).await

    }

    pub async fn from_tokio(utp_socket: UtpSocket) -> io::Result<UtpListener> {
        let (tx, rx) = mpsc::channel(CHANNEL_LEN);
        let local_addr = utp_socket.local_addr()?;

        let socket = Arc::new(Mutex::new(utp_socket));
        let handler = tokio::spawn({
            let socket = socket.clone();
            async move {
                let mut streams: HashMap<SocketAddr, mpsc::Sender<Bytes>> = HashMap::new();
                let (drop_tx, mut drop_rx) = mpsc::channel(1);
                let mut buf = BytesMut::with_capacity(UDP_BUFFER_SIZE * 3);

                loop {
                    if buf.capacity() < UDP_BUFFER_SIZE {
                        buf.reserve(UDP_BUFFER_SIZE * 3);
                    }

                    tokio::select! {
                    Some(peer_addr) = drop_rx.recv() => {
                        streams.remove(&peer_addr);
                    }

                    recv_result = async {
                        // Only hold the lock while receiving
                        let mut socket_guard = socket.lock().await;
                        socket_guard.recv_buf_from(&mut buf).await
                    } => {
                        match recv_result {
                            Ok((len, peer_addr)) => {
                                match streams.get_mut(&peer_addr) {
                                    Some(child_tx) => {
                                        if let Err(err) = child_tx.send(buf.copy_to_bytes(len)).await {
                                            error!("child_tx.send {:?}", err);
                                            child_tx.closed().await;
                                            streams.remove(&peer_addr);
                                            continue;
                                        }
                                    }
                                    None => {
                                        let (child_tx, child_rx) = mpsc::channel(CHANNEL_LEN);
                                        if let Err(err) = child_tx.send(buf.copy_to_bytes(len)).await {
                                            error!("child_tx.send {:?}", err);
                                            continue;
                                        }

                                        let utp_stream = UtpStream {
                                            local_addr,
                                            peer_addr,
                                            receiver: Arc::new(Mutex::new(child_rx)),
                                            socket: socket.clone(),
                                            handler: None,
                                            drop: Some(drop_tx.clone()),
                                            remaining: None
                                        };

                                        if let Err(err) = tx.send((utp_stream, peer_addr)).await {
                                            error!("tx.send {:?}", err);
                                            continue;
                                        }

                                        streams.insert(peer_addr, child_tx);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Socket receive error: {:?}", e);
                                // Maybe add some error handling or backoff here
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
                }
            }
        });

        Ok(Self {
            handler,
            receiver: Arc::new(Mutex::new(rx)),
            local_addr,
        })
    }

    pub async fn accept(&self) -> io::Result<(UtpStream, SocketAddr)> {

        self.receiver.lock().await.recv().await.ok_or(io::Error::from(io::ErrorKind::WouldBlock))

    }

}




pub struct UtpStream {
    pub socket: Arc<Mutex<UtpSocket>>,
    handler: Option<JoinHandle<()>>,
    receiver: Arc<Mutex<Receiver<Bytes>>>,
    remaining: Option<Bytes>,
    drop: Option<Sender<SocketAddr>>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr
}


impl Drop for UtpStream {

    fn drop(&mut self) {

        if let Some(handler) = &self.handler{
            handler.abort();
        }

        if let Some(drop) = &self.drop{
            let _ = drop.try_send(self.peer_addr);
        }

    }

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

    pub async fn connect<A>(addr: A) -> Result<UtpStream, tokio::io::Error>
    where
        A: tokio::net::ToSocketAddrs + Clone
    {
        debug!("Starting UTP stream server connection");
        let local_addr = to_socket_addr(addr).await?;
        let socket = UtpSocket::connect(local_addr).await?;
        Self::from_tokio(socket, local_addr).await

    }

    pub async fn from_tokio(socket: UtpSocket, peer_addr: SocketAddr) -> Result<UtpStream, tokio::io::Error> {
        let socket = Arc::new(Mutex::new(socket));
        let local_addr = {
            let guard = socket.lock().await;
            let addr = guard.local_addr()?;
            drop(guard); // Explicitly drop the guard
            addr
        };

        let (child_tx, child_rx) = mpsc::channel(CHANNEL_LEN);
        let socket_inner = socket.clone();

        let handler = tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(UDP_BUFFER_SIZE);

            loop {
                // Only hold the lock while receiving
                let recv_result = {
                    let mut socket_guard = socket_inner.lock().await;
                    socket_guard.recv_buf_from(&mut buf).await
                };

                match recv_result {
                    Ok((len, received_addr)) => {
                        if received_addr != peer_addr {
                            continue;
                        }

                        if child_tx.send(buf.copy_to_bytes(len)).await.is_err() {
                            info!("Channel closed, stopping receive loop");
                            break;
                        }

                        if buf.capacity() < UDP_BUFFER_SIZE {
                            buf.reserve(UDP_BUFFER_SIZE * 3);
                        }
                    }
                    Err(e) => {
                        error!("Error receiving from socket: {:?}", e);
                        // Maybe add some backoff here
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                }
            }

            info!("Receive loop terminated for peer {}", peer_addr);
        });

        Ok(UtpStream {
            local_addr,
            peer_addr,
            receiver: Arc::new(Mutex::new(child_rx)),
            socket: socket.clone(),
            handler: Some(handler),
            drop: None,
            remaining: None,
        })
    }

    async fn debug_lock_state(&self) {
        match self.socket.try_lock() {
            Ok(_) => info!("Socket lock is available"),
            Err(_) => {
                info!("Socket lock is held. Current thread: {:?}", std::thread::current().id());
                // Try to get stack trace if in debug mode
                #[cfg(debug_assertions)]
                {
                    let bt = std::backtrace::Backtrace::capture();
                    info!("Lock attempt backtrace: {:?}", bt);
                }
            }
        }
    }
    pub fn peer_addr(&self) -> Result<SocketAddr, tokio::io::Error> {

        Ok(self.peer_addr)

    }

    pub fn local_addr(&self) -> Result<SocketAddr, tokio::io::Error> {
        Ok(self.local_addr)
    }

    pub fn shutdown(&self){
        if let Some(drop) = &self.drop{
            let _ = drop.try_send(self.peer_addr);
        };
    }

}

impl AsyncRead for UtpStream {


    //this thing is not using utp_socket .. that needs to be checked
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
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        info!("Attempting to write {} bytes: {:?}", buf.len(), buf);
        let peer_addr = self.peer_addr;

        match self.debug_lock_state(){
            _ => {}
        };
        // Get a reference to the Mutex
        let socket_lock = self.socket.lock();
        pin_mut!(socket_lock);

        // Try to acquire the lock, but make it explicit that we're waiting for the future
        match socket_lock.poll_unpin(cx) {
            Poll::Ready(mut guard) => {
                // Now that we have the lock, try to send the data
                match guard.poll_send_to(cx, buf, peer_addr) {
                    Poll::Ready(Ok(n)) => {
                        info!("Successfully wrote {} bytes", n);
                        Poll::Ready(Ok(n))
                    }
                    Poll::Ready(Err(e)) => {
                        error!("Write error: {}", e);
                        Poll::Ready(Err(e))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            Poll::Pending => {
                info!("Waiting for socket lock");
                Poll::Pending
            }
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