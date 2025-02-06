use crate::error::SocketError;
use crate::packet;
use crate::packet::{Packet, PacketType};
use crate::packet_loss_handler::PacketLossHandler;
use crate::time::{now_microseconds, Delay, Timestamp};
use crate::util::{abs_diff, generate_sequential_identifiers, generate_u16_from_uuid_v4};
use dashmap::DashMap;
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{Interest, ReadBuf, Ready};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::Instant;

const BUF_SIZE: usize = 5;
const GAIN: f64 = 1.0;
const ALLOWED_INCREASE: u32 = 1;
const TARGET: f64 = 100_000.0;
const MSS: u32 = 5;
const MIN_CWND: u32 = 2;
const INIT_CWND: u32 = 2;
const INITIAL_CONGESTION_TIMEOUT: u64 = 500;
const MIN_CONGESTION_TIMEOUT: u64 = 500;
const MAX_CONGESTION_TIMEOUT: u64 = 60_000;
const BASE_HISTORY: usize = 10; //base delay history size
const MAX_SYN_RETRIES: usize = 5;
const MAX_RETRANSMISSION_RETRIES: u32 = 5;
const WINDOW_SIZE: u32 = 1024 * 1024;

// Maximum time (in microseconds) to wait for incoming packets when the send window is full
const PRE_SEND_TIMEOUT: u32 = 500_000;

// Maximum age of base delay sample (60 seconds)
const MAX_BASE_DELAY_AGE: Delay = Delay(60_000_000);

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum SocketState {
    New,
    Connected,
    SynSent,
    FinSent,
    ResetReceived,
    Closed,
}

#[derive(Debug, Clone)]
struct DelayDifferenceSample {
    received_at: Timestamp,
    difference: Delay,
}

#[derive(Debug, Clone)]
pub struct UtpSocketState {
    pub state: SocketState,
    pub seq_nr: u16,
    pub ack_nr: u16,
    pub last_acked: u16,
    pub last_acked_timestamp: Timestamp,
    pub duplicate_ack_count: u32,
    pub last_dropped: u16,
    pub rtt: i32,
    pub rtt_variance: i32,
    pub curr_window: u32,
    pub cwnd: u32,
    pub max_retransmission_retries: u32,
    pub incoming_buffer: VecDeque<Vec<u8>>, // Mutable data
    pub send_window: Vec<Packet>,           // Mutable data
    pub unsent_queue: VecDeque<Packet>,     // Mutable data
    pub pending_data: Vec<u8>,              // Mutable data
    pub remote_wnd_size: u32,
    pub base_delays: VecDeque<Delay>, // Mutable data
    pub current_delays: VecDeque<DelayDifferenceSample>, // Mutable data
    pub their_delay: Delay,
    pub last_rollover: Timestamp,
    pub congestion_timeout: u64,
    pub connected_to: Option<SocketAddr>,
    pub sender_id: u16
}

impl UtpSocketState {
    pub fn new(sender_id: u16) -> Self {
        Self {
            state: SocketState::New,
            seq_nr: 0,
            ack_nr: 0,
            last_acked: 0,
            last_acked_timestamp: Timestamp::default(),
            duplicate_ack_count: 0,
            last_dropped: 0,
            rtt: 0,
            rtt_variance: 0,
            curr_window: 0,
            cwnd: INIT_CWND * MSS,
            max_retransmission_retries: MAX_RETRANSMISSION_RETRIES,
            incoming_buffer: VecDeque::new(),
            send_window: Vec::new(),
            unsent_queue: VecDeque::new(),
            pending_data: Vec::new(),
            remote_wnd_size: 0,
            base_delays: VecDeque::with_capacity(BASE_HISTORY),
            current_delays: VecDeque::new(),
            their_delay: Delay::default(),
            last_rollover: Timestamp::default(),
            congestion_timeout: INITIAL_CONGESTION_TIMEOUT,
            connected_to: None,
            sender_id,
        }
    }
}

pub struct UtpSocket {
    udp: Arc<UdpSocket>,
    peers: Arc<DashMap<SocketAddr, Arc<Mutex<UtpSocketState>>>>,
    sender: Sender<Vec<u8>>,
    id: u16,
}

impl Clone for UtpSocket {
    fn clone(&self) -> Self {
        Self {
            udp: self.udp.clone(), // Only the Arc is cloned
            peers: self.peers.clone(),
            sender: self.sender.clone(),
            id: self.id.clone(),
        }
    }
}

impl UtpSocket {
    pub async fn bind(addr: Option<SocketAddr>, sender: Option<Sender<Vec<u8>>>) -> Self {
        let addr = addr.unwrap_or_else(|| SocketAddr::from_str("127.0.0.1:0").unwrap());
        let udp = Arc::new(UdpSocket::bind(addr).await.unwrap());
        let id = generate_u16_from_uuid_v4();
        let peers = Arc::new(DashMap::new());

        // Now return the socket with the properly initialized state
        UtpSocket {
            udp,
            peers,
            sender: sender.unwrap(),
            id,
        }
    }

    pub async fn start_receiving(&self) -> io::Result<()> {
        info!("Start receiving the task");

        let mut buf = [0; 25];

        loop {
            // Handle the result of recv_packets_from
            match self.recv_packets_from(&mut buf).await {
                Ok(_) => {
                    // Handle the case where the packet is successfully received
                    // No need for 'continue', it will automatically loop again
                }
                Err(e) => {
                    // Log and return the error if something goes wrong
                    info!("Error receiving the message: {}", e);
                    return Err(e); // Return the error if recv_packets_from fails
                }
            }
        }
    }


    pub async fn connect(&mut self, addr: SocketAddr) -> io::Result<()> {
        // Check if the peer is already connected
        if self.peers.contains_key(&addr) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Already connected to this peer",
            ));
        }

        let (r, s) = generate_sequential_identifiers();
        // Create a new state for the peer
        let peer_state = Arc::new(Mutex::new(UtpSocketState::new(r)));

        // Create a new Syn packet
        let mut packet = Packet::new();
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(self.id);

        // Lock the state to access mutable fields
        let mut locked_peer_state = peer_state.lock().await;
        packet.set_seq_nr(locked_peer_state.seq_nr);

        // Connect to the remote address
        match self.udp.connect(&addr).await {
            Ok(..) => {
                // Send the Syn packet
                match self.send(packet.as_ref()).await {
                    Ok(len) => {
                        // Once the packet is sent, await an acknowledgment
                        let mut buf = [0; 25];

                        match self.recv_from(&mut buf).await {
                            Ok((read, src)) => {
                                let received_packet = <Packet as packet::TryFrom<&[u8]>>::try_from(&buf[..read]);
                                info!("Packet received is  {:?}", received_packet);
                                // Update the state after receiving acknowledgment
                                locked_peer_state.connected_to = Some(addr);
                                // Insert the state into the `peers` DashMap
                                self.peers.insert(addr, peer_state.clone());

                                Ok(())
                            }
                            Err(err) => {
                                Err(err)
                            }
                        }
                    }
                    Err(err) => {
                        Err(err)
                    }
                }
            }
            Err(error) => {
                Err(error)
            }
        }
    }
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.udp.send(buf).await
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.udp.send_to(buf, addr).await
    }

    /// This is done from client side .. and we need to also receive the acknowledgement .. os I think there shall also be a recv thing open
    pub async fn send_packets(
        &mut self,
        buf: &[u8],
        addr: Option<SocketAddr>,
    ) -> io::Result<()> {

        let (addr, peer_ref) =  if let Some(addr) = addr{
            self.peers
                .get(&addr)
                .map(|peer| (Some(addr), Arc::clone(peer.value())))
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No peer"))?
        }else{
            self.peers.
                    iter().
                    next().
                    map(|peer| (Some(*peer.key()), Arc::clone(peer.value()))).
                    ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No peers"))?
        };


        let mut peer = peer_ref.lock().await;

        if peer.state == SocketState::Closed {
            return Err(SocketError::ConnectionClosed.into());
        }

        for chunk in buf.chunks(BUF_SIZE) {
            let mut packet = Packet::with_payload(chunk);
            packet.set_seq_nr(peer.seq_nr);
            packet.set_ack_nr(peer.ack_nr);
            packet.set_connection_id(self.id);

            peer.unsent_queue.push_back(packet);

            peer.seq_nr = peer.seq_nr.wrapping_add(1);
        }

        while let Some(mut packet) = peer.unsent_queue.pop_front() {
            match self.send_to(packet.as_ref(), addr.unwrap()).await{
                Ok(_) => (),
                Err(error) => {
                    return Err(io::Error::new(io::ErrorKind::NetworkUnreachable, error).into());
                }
            }
        }
        Ok(())
    }

    pub fn poll_send(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.udp.poll_send(cx, buf)
    }

    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        addr: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        self.udp.poll_send_to(cx, buf, addr)
    }

    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        self.udp.try_send(buf)
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.udp.recv(buf).await
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.udp.recv_from(buf).await
    }

    //this only works to
    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.udp.try_recv(buf)
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.udp.poll_recv(cx, buf)
    }

    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        self.udp.poll_recv_from(cx, buf)
    }

    pub async fn recv_packets_from(&self, buf: &mut [u8]) -> io::Result<SocketAddr> {
        // Receive data from the UDP socket
        let (len, addr) = self.udp.recv_from(buf).await?;
        let received_data = &buf[..len];

        // Try to parse the received data into a packet
        let packet = match <Packet as packet::TryFrom<&[u8]>>::try_from(received_data) {
            Ok(packet) => packet,
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
            }
        };

        info!("Received packet is {:?}", packet);

        // Check if the peer exists in the `peers` DashMap
        let peer_state =  self.peers.entry(addr).or_insert_with( ||{
            let new_state = Arc::new(Mutex::new(UtpSocketState::new(packet.connection_id())));
            new_state
        });

        // Lock the peer state for modification
        let mut state = peer_state.lock().await;

        // Add the received data to the peer's incoming buffer
        state.incoming_buffer.push_back(received_data.to_vec());

        // Handle the packet and send a response if needed
        if let Ok(Some(rsp)) = self.handle_packet(&packet, &mut state) {
            match self.udp.send_to(rsp.as_ref(), addr).await {
                Ok(sent_len) => {
                    // Process the incoming buffer
                    while let Some(data) = state.incoming_buffer.pop_front() {
                        if let Err(e) = self.sender.send(data).await {
                            return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
                        }
                    }
                },
                Err(error) => {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()));
                },
            }
        }

        // Return the address of the peer
        Ok(addr)
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.udp.peer_addr()
    }
    pub async fn send_packet(&mut self, data: &[u8], peer: &mut UtpSocketState) -> io::Result<()> {
        let seq_nr = peer.seq_nr;
        let time_stamp = now_microseconds();
        let mut packet = Packet::with_payload(b"Hello");
        packet.set_seq_nr(seq_nr);
        packet.set_timestamp(time_stamp);
        packet.set_type(PacketType::Syn);

        peer.unsent_queue.push_back(packet);

        Ok(())
    }

    fn handle_packet(&self, packet: &Packet, peer: &mut UtpSocketState) -> io::Result<Option<Packet>> {
        debug!("({:?}, {:?})", peer.state, packet.get_type());

        //Now this response shall depend on the socket state and packet type
        match (peer.state, packet.get_type()) {
            //if the socket is just started and I am getting a connection request
            (SocketState::New, PacketType::Syn) => self.handle_new_state(packet, peer),
            //In case I get a sync packet, I will just reset the socket
            (_, PacketType::Syn) => Ok(Some(self.prepare_reply(packet, PacketType::Reset,peer))),
            //when packet has been sent, socket is in syn sent state, it waits for acknowledgement, and it gets packet_type == State
            (SocketState::SynSent, PacketType::State) => {
                peer.ack_nr = packet.ack_nr();
                peer.seq_nr += 1;
                peer.state = SocketState::Connected;
                peer.last_acked = packet.ack_nr();
                peer.last_acked_timestamp = now_microseconds();
                Ok(None)
            }
            //if the socket is in syn state, any packet_type other than state is invalid
            (SocketState::SynSent, _) => Err(SocketError::InvalidReply.into()),

            //if socket is in connected state .. or waiting for reply after sending finish packet
            (SocketState::Connected, PacketType::Data)
            | (SocketState::FinSent, PacketType::Data) => {
                let expected_packet_type = if peer.state == SocketState::FinSent {
                    PacketType::Fin
                } else {
                    PacketType::State
                };
                let mut reply = self.prepare_reply(packet, expected_packet_type, peer);
                if packet.seq_nr().wrapping_sub(peer.ack_nr) > 1 {
                    info!(
                        "current ack_nr ({}) is behind packet seq_nr ({})",
                        peer.ack_nr,
                        packet.seq_nr()
                    );
                    //set sack extension payload if the packet is not in order
                    // todo!()
                }
                Ok(Some(reply))
            }
            //I think this state if acknowledgement
            (SocketState::Connected, PacketType::State) => {
                if packet.ack_nr() == peer.last_acked {
                    peer.duplicate_ack_count += 1;
                } else {
                    peer.last_acked = packet.ack_nr();
                    peer.last_acked_timestamp = now_microseconds();
                    peer.duplicate_ack_count = 1;
                }
                //we need to update the congestion window here
                // todo!()
                Ok(None)
            }
            //If I am waiting for Fin packet ... in connected state or FinSent State
            //Here I need to close the connection
            (SocketState::Connected, PacketType::Fin) | (SocketState::FinSent, PacketType::Fin) => {
                info!("Fin received but there are missing acknowledgments for sent packets");

                if packet.ack_nr() < peer.seq_nr {
                    info!("Fin received but there are missing acknowledgments for sent packets");
                }

                let mut reply = self.prepare_reply(packet, PacketType::State, peer);

                if packet.seq_nr().wrapping_sub(peer.ack_nr) > 1 {
                    info!(
                        "current ack_nr ({}) is behind received packet seq_nr ({})",
                        peer.ack_nr,
                        packet.seq_nr()
                    );
                }

                //We need to set SACK extension payload if the packet is not in order

                peer.state = SocketState::Closed;
                Ok(Some(reply))
            }
            //I think if the SocketState is closed ... it shall not handle any packet
            (SocketState::Closed, PacketType::Fin) => {
                Ok(Some(self.prepare_reply(packet, PacketType::State, peer)))
            }
            //After sending Fin Packet , waiting for the response
            (SocketState::FinSent, PacketType::State) => {
                if packet.ack_nr() == peer.seq_nr {
                    peer.state = SocketState::Closed;
                } else {
                    //this means we have lost some packet

                    if packet.ack_nr() == peer.last_acked {
                        peer.duplicate_ack_count += 1;
                    } else {
                        peer.last_acked = packet.ack_nr();
                        peer.last_acked_timestamp = now_microseconds();
                        peer.duplicate_ack_count = 1;
                    }

                    //we need to update the congestion window
                }
                Ok(None)
            }
            //For any SocketState, if I receive a reset packet
            (_, PacketType::Reset) => {
                peer.state = SocketState::ResetReceived;
                Err(SocketError::ConnectionReset.into())
            }
            (state, ty) => {
                let message = format!("Unimplemented handling for ({:?},{:?})", state, ty);
                debug!("{}", message);
                Err(SocketError::Other(message).into())
            }
        }
    }

    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        self.udp.ready(interest).await
    }

    ///when new connection is established
    fn handle_new_state(&self, packet: &Packet, peer: &mut UtpSocketState) -> io::Result<Option<Packet>> {
        //this means our socket state is new, and it only accepts connection request
        peer.ack_nr = packet.ack_nr();
        peer.seq_nr = 0;
        peer.sender_id = self.id;
        peer.state = SocketState::Connected;
        peer.last_dropped = peer.ack_nr;
        Ok(Some(self.prepare_reply(packet, PacketType::State, peer)))
    }

    fn prepare_reply(&self, original: &Packet, t: PacketType, peer: &mut UtpSocketState) -> Packet {
        let mut resp = Packet::new();
        resp.set_type(t);
        let self_t_micro = now_microseconds();
        let other_t_micro = original.timestamp();
        let time_difference: Delay = abs_diff(self_t_micro, other_t_micro);
        resp.set_timestamp(self_t_micro);
        resp.set_timestamp_difference(time_difference);
        resp.set_connection_id(peer.sender_id);
        resp.set_seq_nr(peer.seq_nr);
        resp.set_ack_nr(peer.ack_nr);
        resp
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
