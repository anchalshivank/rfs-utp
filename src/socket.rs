use std::io;
use std::net::{SocketAddr};
use std::str::FromStr;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;
use tokio::net::{ToSocketAddrs, UdpSocket};
use crate::packet::Packet;
use crate::packet_loss_handler::PacketLossHandler;
use crate::time::{Delay, Timestamp};

const BUF_SIZE: usize = 1500;
const GAIN: f64 = 1.0;
const ALLOWED_INCREASE: u32 = 1;
const TARGET: f64 = 100_000.0;
const MSS: u32 = 1400;
const MIN_CWND: u32 = 2;
const INIT_CWND: u32 = 2;
const INITIAL_CONGESTION_TIMEOUT: u64 = 500;
const MIN_CONGESTION_TIMEOUT: u64 = 500;
const MAX_CONGESTION_TIMEOUT: u64 = 60_000;
const BASE_HISTORY: usize = 10;//base delay history size
const MAX_SYN_RETRIES: usize = 5;
const MAX_RETRANSMISSION_RETRIES: u32 = 5;
const WINDOW_SIZE: u32 = 1024*1024;

// Maximum time (in microseconds) to wait for incoming packets when the send window is full
const PRE_SEND_TIMEOUT: u32 = 500_000;

// Maximum age of base delay sample (60 seconds)
const MAX_BASE_DELAY_AGE: Delay = Delay(60_000_000);


#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum SocketState{
    New,
    Connected,
    SynSent,
    FinSent,
    ResetReceived,
    Closed
}

struct DelayDifferenceSample{
    received_at: Timestamp,
    difference: Delay
}
pub struct UtpSocket {

    udp: UdpSocket

}

impl UtpSocket {

    pub async fn bind(addr: Option<SocketAddr>) -> Self{

        let addr = addr.unwrap_or(SocketAddr::from_str("127.0.0.2:0").unwrap());

        let udp = UdpSocket::bind(addr).await.unwrap();
        Self { udp }

    }

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {

        self.udp.connect(addr).await

    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.udp.send(buf).await
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.udp.send_to(buf, addr).await
    }

    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.udp.poll_send(cx, buf)
    }

    pub fn poll_send_to(&self, cx: &mut Context<'_>, buf: &[u8], addr: SocketAddr) -> Poll<io::Result<usize>> {
        self.udp.poll_send_to(cx, buf, addr)
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.udp.recv(buf).await
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.udp.recv_from(buf).await
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.udp.poll_recv(cx, buf)
    }

    pub fn poll_recv_from(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<SocketAddr>> {
        self.udp.poll_recv_from(cx, buf)
    }


    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.udp.peer_addr()
    }
}

impl PacketLossHandler for UtpSocket {
    async fn handle_packet_loss(&mut self, lost_packet: Packet) {
        todo!()
    }

    async fn resend_lost_packet(&mut self, lost_packet: Packet) {
        todo!()
    }

    async fn send_fast_resend_request(&self) {
        todo!()
    }

    async fn advance_send_window(&mut self) {
        todo!()
    }
}