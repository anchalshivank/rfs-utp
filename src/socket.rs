use crate::error::{ParseError, SocketError};
use crate::packet;
use crate::packet::{Packet, PacketType};
use crate::packet_loss_handler::PacketLossHandler;
use crate::time::{now_microseconds, Delay, Timestamp};
use crate::util::{abs_diff, generate_sequential_identifiers, generate_u16_from_uuid_v4};
use dashmap::DashMap;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::Hash;
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
//this state is of the peer connected
#[derive(Debug, Clone)]
pub struct UtpSocketState {
    pub state: SocketState,
    pub seq_nr: u16,
    pub ack_nr: u16,
    pub duplicate_ack_count: u16,
    pub last_acked: u16, // this is required as this marks the last ack received from this peer
    pub last_acked_timestamp: Timestamp,
    pub rtt: i32,
    pub rtt_variance: i32,
    pub curr_window: u32,
    pub cwnd: u32,
    pub max_retransmission_retries: u32,
    pub incoming_buffer: VecDeque<Vec<u8>>,
    pub sent_window: HashMap<u16, Packet>,           // Mutable data -> I think this shall be a HashSet
    pub unsent_queue: VecDeque<Packet>,     // I think this is required
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
            duplicate_ack_count: 0,
            last_acked: 0,
            last_acked_timestamp: Timestamp::default(),
            rtt: 0,
            rtt_variance: 0,
            curr_window: 0,
            cwnd: INIT_CWND * MSS,
            max_retransmission_retries: MAX_RETRANSMISSION_RETRIES,
            incoming_buffer: VecDeque::new(),
            sent_window: HashMap::new(),
            unsent_queue: VecDeque::new(),
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
            println!("---------");
            self.recv_packets_from(&mut buf).await?;
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

        let id = generate_u16_from_uuid_v4();
        // Create a new state for the peer
        let peer_state = Arc::new(Mutex::new(UtpSocketState::new(id)));

        // Create a new Syn packet
        let mut packet = Packet::new();
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(self.id);
        // Lock the state to access mutable fields
        {
            let mut locked_peer_state = peer_state.lock().await;
            packet.set_seq_nr(locked_peer_state.seq_nr);
            locked_peer_state.state = SocketState::SynSent;
            // Connect to the remote address
            locked_peer_state.connected_to = Some(addr);
            locked_peer_state.sent_window.insert(packet.seq_nr(), packet.clone());

        }

        self.peers.insert(addr, peer_state);
        self.udp.connect(&addr).await?;
        self.send(packet.as_ref()).await?;


        //now we must have retry mechanism

        // for _ in 0..MAX_RETRANSMISSION_RETRIES {
        //     let mut buf = [0;25];
        //     match self.recv_from(&mut buf).await{
        //         Ok((len , addr)) => {
        //
        //             //we found the recv
        //             let packet = Self::to_packet(&buf[..len])?;
        //
        //             if packet.ack_nr() == locked_peer_state.last_acked + 1{
        //                 //we have acknowledged the packet
        //                 locked_peer_state.last_acked +=1;
        //
        //                 locked_peer_state.sent_window.remove(&packet.seq_nr());
        //
        //                 //here I think the sent_window shall be empty
        //                 locked_peer_state.sent_window.clear();
        //             }
        //
        //         }
        //         Err(err) => {error!("Error receiving data: {}", err); return Err(err);}
        //     }
        //
        // }

        Ok(())
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

            peer.sent_window.insert(packet.seq_nr(), packet);

            peer.seq_nr = peer.seq_nr.wrapping_add(1);
        }

        while let Some((_, mut packet)) = peer.sent_window.iter().next() {
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
        match self.udp.recv_from(buf).await{
            Ok((len, addr)) => {
                Ok((len, addr))
            }
            Err(err) => {error!("{:?}", err);
            Err(err)}
        }
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
        let (len, addr) = self.recv_from(buf).await?;
        let received_data = &buf[..len];

        // Try to parse the received data into a packet
        let packet = Self::to_packet(received_data)?;

        info!("Received packet is {:?}", packet);

        // Check if the peer exists in the `peers` DashMap


        let peer_state =  self.peers.entry(addr).or_insert_with( ||{
            //this is the new state ... this might be a new connection request


            let mut utp_socket_state = UtpSocketState::new(packet.connection_id());
            utp_socket_state.connected_to = Some(addr);
            let new_state = Arc::new(Mutex::new(utp_socket_state));

            new_state
        });

        // Lock the peer state for modification
        let mut state = peer_state.lock().await;

        // Add the received data to the peer's incoming buffer
        state.incoming_buffer.push_back(received_data.to_vec());

        // Handle the packet and send a response if needed
        if let Ok(Some(rsp)) = self.handle_packet(&packet, &mut state) {

            match self.udp.send_to(rsp.as_ref(), addr).await {
                Ok(_) => {
                },
                Err(error) => {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()));
                },
            }
        }

        while let Some(data) = state.incoming_buffer.pop_front() {
            if let Err(e) = self.sender.send(data).await {
                return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
            }
        }

        // Return the address of the peer
        Ok(addr)
    }

    fn to_packet(received_data: &[u8]) -> Result<Packet, ParseError> {
        <Packet as packet::TryFrom<&[u8]>>::try_from(received_data)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.udp.peer_addr()
    }

    fn handle_packet(&self, packet: &Packet, peer: &mut UtpSocketState) -> io::Result<Option<Packet>> {
        info!("({:?}, {:?})", peer.state, packet.get_type());

        match (peer.state, packet.get_type()) {
            // Handling a new connection request (SYN packet received in the New state)
            (SocketState::New, PacketType::Syn) => {
                peer.ack_nr = packet.seq_nr(); // Acknowledge the SYN
                peer.seq_nr = 0; // Initialize sequence number
                peer.state = SocketState::Connected; // Transition to Connected state
                Ok(Some(self.prepare_reply(packet, PacketType::State, peer)))
            }

            // Unexpected SYN received in any state -> Send a Reset packet
            (_, PacketType::Syn) => {
                Ok(Some(self.prepare_reply(packet, PacketType::Reset, peer)))
            }

            // Handling an ACK for a previously sent SYN (client transitions to Connected state)
            (SocketState::SynSent, PacketType::State) => {
                peer.ack_nr = packet.seq_nr(); // Store acknowledged sequence number
                peer.seq_nr = peer.seq_nr.wrapping_add(1); // Increment sequence number for next packet
                peer.state = SocketState::Connected; // Transition to Connected state
                peer.last_acked = packet.ack_nr(); // Store last acknowledged sequence number
                peer.last_acked_timestamp = now_microseconds(); // Update acknowledgment timestamp
                Ok(None) // No immediate response required
            }

            // Handling Data Packets (Detect Packet Loss)
            (SocketState::Connected, PacketType::Data) | (SocketState::FinSent, PacketType::Data) => {
                // Determine the appropriate reply type (State packet normally, but Fin if in FinSent state)
                let expected_packet_type = if peer.state == SocketState::FinSent {
                    PacketType::Fin
                } else {
                    PacketType::State
                };

                let reply = self.prepare_reply(packet, expected_packet_type, peer);

                // Detect packet loss: If received packet is ahead of expected sequence number
                if packet.seq_nr().wrapping_sub(peer.ack_nr) > 1 {
                    info!(
                    "Packet loss detected! Current ack_nr ({}) is behind packet seq_nr ({})",
                    peer.ack_nr, packet.seq_nr()
                );
                    // TODO: Implement Selective Acknowledgment (SACK) to request missing packets
                }

                Ok(Some(reply))
            }

            // Handling ACK packets during data transmission
            (SocketState::Connected, PacketType::State) => {
                if packet.ack_nr() == peer.last_acked {
                    // Duplicate ACK detected
                    peer.duplicate_ack_count += 1;
                } else {
                    // New ACK received -> Update state
                    peer.last_acked = packet.ack_nr();
                    peer.last_acked_timestamp = now_microseconds();
                    peer.duplicate_ack_count = 1;
                }

                // Fast retransmit: If three duplicate ACKs are received, assume packet loss
                let packet_loss_detected = !peer.sent_window.is_empty() && peer.duplicate_ack_count == 3;

                if packet_loss_detected {
                    // TODO: Identify and retransmit the lost packet
                }

                Ok(None) // No response required immediately
            }

            // Handling FIN packet (Closing Connection)
            (SocketState::Connected, PacketType::Fin) | (SocketState::FinSent, PacketType::Fin) => {
                // Check if some packets are missing before closing
                if packet.seq_nr().wrapping_sub(peer.ack_nr) > 1 {
                    info!("Fin received but missing ACKs for some packets");
                    // TODO: Request missing ACKs before closing the connection
                }

                // If we are waiting for SACK, should we close the socket immediately?
                // Closing while waiting for SACK might cause data loss.
                let reply = self.prepare_reply(packet, PacketType::State, peer);
                peer.state = SocketState::Closed; // Transition to Closed state
                Ok(Some(reply))
            }

            // Handling Reset Packet (Terminates connection in any state)
            (_, PacketType::Reset) => {
                peer.state = SocketState::ResetReceived;
                Err(SocketError::ConnectionReset.into())
            }

            // Catch unhandled states
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


    fn prepare_reply(&self, packet: &Packet, t: PacketType, peer: &mut UtpSocketState) -> Packet {
        let mut resp = Packet::new();
        resp.set_type(t);
        let self_t_micro = now_microseconds();
        let other_t_micro = packet.timestamp();
        let time_difference: Delay = abs_diff(self_t_micro, other_t_micro);
        resp.set_timestamp(self_t_micro);
        resp.set_timestamp_difference(time_difference);
        //I think this shall be the id of sender ... that is the current socket
        resp.set_connection_id(self.id);
        resp.set_seq_nr(packet.seq_nr());
        resp.set_ack_nr(packet.seq_nr());
        resp
    }
}

