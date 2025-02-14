use crate::error::{ParseError, SocketError};
use crate::packet;
use crate::packet::{ExtensionType, Packet, PacketType};
use crate::packet_loss_handler::PacketLossHandler;
use crate::time::{now_microseconds, Delay, Timestamp};
use crate::util::{abs_diff, generate_sequential_identifiers, generate_u16_from_uuid_v4};
use dashmap::DashMap;
use log::{debug, error, info, warn};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use rand::Rng;
use tokio::io::{Interest, ReadBuf, Ready};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::Instant;

const BUF_SIZE: usize = 6;
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
    pub incoming_buffer: VecDeque<Packet>,
    pub sent_window: BTreeMap<u16, Packet>,           // Mutable data -> I think this shall be a HashSet
    pub unsent_queue: VecDeque<Packet>,     // I think this is required
    pub remote_wnd_size: u32,
    pub base_delays: VecDeque<Delay>, // Mutable data
    pub current_delays: VecDeque<DelayDifferenceSample>, // Mutable data
    pub their_delay: Delay,
    pub last_rollover: Timestamp,
    pub congestion_timeout: u64,
    pub connected_to: Option<SocketAddr>,
    pub sender_id: u16,
    pub out_of_order: BTreeMap<u16, Packet>
}

impl UtpSocketState {
    pub fn new(sender_id: u16) -> Self {
        Self {
            state: SocketState::New,
            seq_nr: 1,
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
            sent_window: BTreeMap::new(),
            unsent_queue: VecDeque::new(),
            remote_wnd_size: 0,
            base_delays: VecDeque::with_capacity(BASE_HISTORY),
            current_delays: VecDeque::new(),
            their_delay: Delay::default(),
            last_rollover: Timestamp::default(),
            congestion_timeout: INITIAL_CONGESTION_TIMEOUT,
            connected_to: None,
            sender_id,
            out_of_order: BTreeMap::new()
        }
    }
}

pub struct UtpSocket {
    udp: Arc<UdpSocket>,
    peers: Arc<DashMap<SocketAddr, Arc<Mutex<UtpSocketState>>>>,
    sender: Sender<Packet>,
    id: u16,
    drop: bool,
    client: bool
}

impl Clone for UtpSocket {
    fn clone(&self) -> Self {
        Self {
            udp: self.udp.clone(), // Only the Arc is cloned
            peers: self.peers.clone(),
            sender: self.sender.clone(),
            id: self.id.clone(),
            drop: self.drop.clone(),
            client: self.client.clone()
        }
    }
}

impl UtpSocket {
    pub async fn bind(addr: Option<SocketAddr>, sender: Option<Sender<Packet>>, drop: bool, client: bool) -> Self {
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
            drop,
            client
        }
    }

    pub async fn start_receiving(&self) -> io::Result<()> {
        info!("Start receiving the task");

        let mut buf = [0; 26];

        loop {
            // Handle the result of recv_packets_from
            info!("------------------------------------------------");
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

            info!("({:?}, {:?})", locked_peer_state.state, packet.get_type());
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

    /// This is done from client side ... and we need to also receive the acknowledgement ... os I think there shall also be a recv thing open
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

        let mut it = peer.sent_window.iter();


        while let Some((_, packet)) = it.next() {
            if self.client{

                info!(">>>>>>>>>>>>>>Sending packet with seq_nr {}", packet.seq_nr());
            }

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
                info!("Got somethign -------- {} {:?}", len, addr);

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

        info!("Receiving >>>>>>");

        // Receive data from the UDP socket
        let (len, addr) = self.recv_from(buf).await?;
        info!("Got somethign 2-------- {} {:?}", len, addr);

        let received_data = &buf[..len];


        //here we can add the logic of packet loss



        // Try to parse the received data into a packet
        let packet = Self::to_packet(received_data).map_err(|err| {
            error!("got this error {:?}", err);
            io::Error::new(io::ErrorKind::InvalidData, err)
        })?;


        info!("got this packet {:?}", packet.get_type());


        if self.drop && packet.seq_nr() !=1 {
            if rand::thread_rng().gen_bool(0.6) {
                info!("Dropping packet from {} (Simulated packet loss) and it's sequence number is {}", addr, packet.seq_nr());
                return Ok(addr);
            }
        }


        let peer_state =  self.peers.entry(addr).or_insert_with( ||{
            //this is the new state ... this might be a new connection request

            let mut utp_socket_state = UtpSocketState::new(packet.connection_id());
            utp_socket_state.connected_to = Some(addr);
            let new_state = Arc::new(Mutex::new(utp_socket_state));

            new_state
        });


        // Lock the peer state for modification
        let mut state = peer_state.lock().await;

        if !self.client{


            info!("<<<<<<<<<<<<Receiving  packet (seq_nr {}, last_ack {})", packet.seq_nr(), state.last_acked);


        }else{

            info!("<<<<<<<<<<<<Receiving  packet (seq_nr {},  ack_nr  {})", packet.seq_nr(), packet.ack_nr());


        }
        // Add the received data to the peer's incoming buffer

        let packet = <Packet as packet::TryFrom<&[u8]>>::try_from(buf)?;


        state.incoming_buffer.push_back(packet.clone());

        // Handle the packet and send a response if needed
        if let Ok(Some(rsp)) = self.handle_packet(&packet, &mut state) {
            //sender cares about the last_ack ...
            info!(">>>>>>>>>>>>> Returning packet with seq_nr {} , ack_nr {}", rsp.seq_nr(), rsp.ack_nr());
            // info!("sending packet with extension_end {}", rsp.ex);
            match self.send_to(rsp.as_ref(), addr).await {
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
        //receiver only takes care of the ack_nr -> how many it has acknowledged and seq_nr

        info!("Current state is [{:?} {:?}]", packet.get_type(), peer.state);


        let p = match (peer.state, packet.get_type()) {
            // Handling a new connection request (SYN packet received in the New state)
            (SocketState::New, PacketType::Syn) => {
                peer.last_acked = packet.seq_nr(); // Acknowledge the SYN
                peer.state = SocketState::Connected; // Transition to Connected state
                Ok(Some(self.prepare_reply(packet, PacketType::State, packet.seq_nr(), packet.seq_nr())))
            }

            // Unexpected SYN received in any state -> Send a Reset packet
            (_, PacketType::Syn) => {
                Ok(Some(self.prepare_reply(packet, PacketType::Reset, packet.seq_nr(), packet.seq_nr())))
            }

            // Handling an ACK for a previously sent SYN (client transitions to Connected state)
            (SocketState::SynSent, PacketType::State) => {
                peer.ack_nr = packet.seq_nr(); // Store acknowledged sequence number
                peer.seq_nr = peer.seq_nr.wrapping_add(1); // Increment sequence number for next packet
                peer.state = SocketState::Connected; // Transition to Connected state
                peer.last_acked = packet.ack_nr(); // Store last acknowledged sequence number
                peer.last_acked_timestamp = now_microseconds(); // Update acknowledgment timestamp
                peer.sent_window.remove(&packet.seq_nr());
                Ok(None) // No immediate response required
            }


            // it is confirmed we are waiting on sender side as it waits for acknowledgement
            (SocketState::Connected, PacketType::Ack) | (SocketState::FinSent, PacketType::Ack) => {
                let mut reply = None;
                // we need to understand that
                // we start with seq_nr 1 on both side .. and last_ack 0 on both side
                // 1 is send and received on server .. so 1 == 0+1 // makes sense
                // now this 1 acknowledges as 1+1 = 2 , but this 2 is not equal to 0+1 on sender side
                //How to fix this???!!!!!!
                if packet.ack_nr() == peer.last_acked {
                    //means packet.ack_nr() + 1 has not been received
                    //duplicate Ack detected
                    info!("duplicate found");
                    peer.duplicate_ack_count +=1;


                }else if packet.ack_nr() == peer.last_acked+1{
                    info!("------------------------>>>");
                    peer.last_acked = packet.ack_nr();
                    peer.last_acked_timestamp = now_microseconds();
                    peer.duplicate_ack_count = 1;
                    peer.sent_window.remove(&packet.ack_nr());

                }

                if peer.duplicate_ack_count >=3 {
                    //we need to send that packet back
                    let packet = peer.sent_window.get(&(packet.ack_nr() +1)).unwrap().clone();
                    reply = Some(packet);

                }

                // we can check if this acknowledgement packet is sack?

                if packet.get_extension_type() == ExtensionType::SelectiveAck {

                    //we need to resend this packet

                    if let Some(sack) = packet.get_sack(){
                        let lost_packets = self.get_lost_seq_nr(sack, packet.ack_nr(), peer.seq_nr);
                        info!("Lost packets are {:?}", lost_packets);

                    }
                }


                info!("On client side last_ack is {}", peer.last_acked);
                Ok(reply)
            }

            // it is confirmed that we are on receiver side . as only receiver will receive data ... even incase the sender is sending data .. the packetType will cahnge to Data.. it works universally
            (SocketState::Connected, PacketType::Data) | (SocketState::FinSent, PacketType::Data) => {
                // Determine the appropriate reply type (State packet normally, but Fin if in FinSent state)
                let expected_packet_type = if peer.state == SocketState::FinSent {
                    PacketType::Fin
                } else {
                    PacketType::State
                };

                //Assume we are on receiver side
                //we need to define the things
                //seq_nr means the seq no of any packet
                //last_ack nr means the last continuous acknowledged number means ... if it is 5 it means all the packet until 5 has been acknowledged
                let mut sack = Vec::new();
                if packet.seq_nr() == peer.last_acked + 1 {

                    peer.last_acked = packet.seq_nr();
                    peer.seq_nr +=1;


                    let mut it = peer.out_of_order.iter();
                    let mut seq_nr_to_remove = Vec::new();
                    while let Some((&seq, _)) = it.next(){


                        if seq == peer.last_acked + 1   {


                            peer.last_acked = seq;
                            seq_nr_to_remove.push(seq);

                        }else {
                            break;
                        }

                    }
                    // eprintln!("Final last ack is {}", peer.last_acked);
                    for seq in seq_nr_to_remove {
                        peer.out_of_order.remove(&seq);
                    }

                    info!("correct and last ack is {}", peer.last_acked);


                    //as soon as we receive the expected packet we shall pop the top most packet if it is next in order to be acknowledged

                }
                // Detect packet loss: If received packet is ahead of expected sequence number
                else if packet.seq_nr()  > peer.last_acked {
                    info!("Packet loss detected! Current ack_nr ({}) is behind packet seq_nr ({})", peer.last_acked, packet.seq_nr());

                    //Now we will also keep a buffer of out of order packet that have been acknowledged
                    // for eg we were expecting 4 ... but we got 5,6,7
                    //it will be futile to waste these packets ... we will just store them in buffer ... I think it is a good idea to put it in min heap,
                    // and we will not require the ack of these packets
                    // TODO: Implement Selective Acknowledgment (SACK) to request missing packets
                    // so we will just send the ack for last_ack
                    peer.out_of_order.insert(packet.seq_nr(), packet.clone());
                    //reply.set_ack_nr(peer.last_acked); //I think this is not required as sender will know that it has to send the duplicate packet
                    //we know the packet has been lost
                    sack = self.build_selective_ack(peer);


                }else{

                    //this is the case ... in which I got a packet ... that has already been acknowledged
                    //it shall never happen but ... in case it happens we ignore the packet
                    info!("You are fucked");


                }

                info!("Peer last_ack is {}", peer.last_acked);
                let mut reply = self.prepare_reply(packet, PacketType::Ack, peer.seq_nr, peer.last_acked);
                if sack.len()!=0{
                    info!("Sack is {:?}", sack);
                    reply.set_sack(sack);

                }
                info!("---->>>> seq_nr {}, ack_nr {}", peer.seq_nr, reply.ack_nr());
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
                let packet_loss_detected = !peer.sent_window.is_empty() || peer.duplicate_ack_count == 3 ;

                let mut reply = None;

                if packet_loss_detected {
                    // TODO: Identify and retransmit the lost packet
                    //Oh,  as this is the receiver side ... we shall only
                    let packet  = peer.sent_window.get(&(packet.ack_nr() - 1)).unwrap().clone();
                    reply = Some(packet);
                }
                Ok(reply) // No response required immediately
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
                let reply = self.prepare_reply(packet, PacketType::State, packet.seq_nr(), packet.seq_nr());
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
        };
        p
    }

    fn build_selective_ack(&self, peer: &mut UtpSocketState) -> Vec<u8> {
        let mut sack = Vec::new();
        let mut max_byte_index = 0;

        for (seq_nr, packet) in &peer.out_of_order {


            if *seq_nr > peer.last_acked {
                let diff = (seq_nr - peer.last_acked) as usize - 1;
                let byte = diff / 8;
                let bit = diff % 8;

                // Ensure SACK vector is large enough
                if byte >= sack.len() {
                    max_byte_index = byte;
                    sack.resize(max_byte_index + 1, 0);
                }

                sack[byte] |= 1 << bit;

                // eprintln!("{:08b}", sack[byte]);


            }
        }

        // Ensure SACK length is multiple of 4 for compatibility
        while sack.len() % 4 != 0 {
            sack.push(0);
        }
        sack
    }


    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        self.udp.ready(interest).await
    }


    fn prepare_reply(&self, packet: &Packet, t: PacketType, seq_nr: u16, ack_nr: u16) -> Packet {
        info!("ack_nr foubnd is {}", ack_nr);
        let mut resp = Packet::new();
        resp.set_type(t);
        let self_t_micro = now_microseconds();
        let other_t_micro = packet.timestamp();
        let time_difference: Delay = abs_diff(self_t_micro, other_t_micro);
        resp.set_timestamp(self_t_micro);
        resp.set_timestamp_difference(time_difference);
        //I think this shall be the id of sender ... that is the current socket
        resp.set_connection_id(self.id);
        resp.set_seq_nr(seq_nr);
        resp.set_ack_nr(ack_nr);
        info!("ack_nr input is {} , packet ack_nr is {}",ack_nr ,resp.ack_nr());
        resp
    }

    fn register_peer(&mut self, addr: SocketAddr, peer_state : UtpSocketState) {

        self.peers.insert(addr, Arc::new(Mutex::new(peer_state)));

    }

    fn get_peer(&self, addr: SocketAddr)  -> Option<Arc<Mutex<UtpSocketState>>>{

        self.peers.get(&addr).map(|entry| Arc::clone(entry.value()))

    }

    fn get_lost_seq_nr(&self, sack: Vec<u8>, ack_nr: u16, limit_seq: u16) -> Vec<u16> {

        let mut result = Vec::new();
        for (index, val) in sack.iter().enumerate() {
            for bit in 0..8 {
                //if this x is zero it means the packet is missing
                let x = (val>>bit & 1u8) == 0u8;
                if x  {
                    // eprintln!("last ack is {}", ack_nr);
                    let seq_nr = 8*(index as u16) + bit +1 + ack_nr;
                    if limit_seq > seq_nr {
                        result.push(seq_nr);
                    }else{
                        return result;
                    }
                }
            }
        }

        result

    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;
    use tokio::sync::mpsc;

    fn create_test_packet(
        packet_type: PacketType,
        seq_nr: u16,
        ack_nr: u16,
        connection_id: u16
    ) -> Packet {

        let mut packet = Packet::new();
        packet.set_type(packet_type);
        packet.set_seq_nr(seq_nr);
        packet.set_ack_nr(ack_nr);
        packet.set_connection_id(connection_id);
        packet

    }

    async fn create_socket(addr: SocketAddr) -> UtpSocket {


        let ch = mpsc::channel(100);
        let mut socket = UtpSocket::bind(None , Some(ch.0), false, false).await;
        let mut peer_state = UtpSocketState::new(1);
        peer_state.state = SocketState::Connected;
        socket.register_peer(addr, peer_state);
        socket

    }

    #[tokio::test]
    async fn test_packet_transfer(){

        let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();
        let server = create_socket(server_addr).await;
        let client_addr = SocketAddr::from_str("127.0.0.1:8081").unwrap();
        let client = create_socket(client_addr).await;
        let c_peer = client.get_peer(client_addr).unwrap();
        let mut client_peer = c_peer.lock().await;
        let s_peer = server.get_peer(server_addr).unwrap();
        let mut server_peer = s_peer.lock().await;

        // Simulate sending packets : 0,1,2,3,4,5,6,8
        //we need to access that peer
        for seq_nr in 0..8 {

            let data_packet = create_test_packet(PacketType::Data, seq_nr, client_peer.last_acked, seq_nr);
            //we also need to update the peer state that has been sent
            //as this message is being sent from client to server . so
            client_peer.sent_window.insert(seq_nr, data_packet.clone());
            //peer state shall be of server _ peer
            let _ = server.handle_packet(&data_packet, &mut server_peer);

        };
        eprintln!("{:?}", server_peer);
        // Simulate Ack reception
        let ack_packets = vec![
            create_test_packet(PacketType::Ack, 0, 1, 1),
            create_test_packet(PacketType::Ack, 1, 2, 1),
            create_test_packet(PacketType::Ack, 2, 3, 1),
            create_test_packet(PacketType::Ack, 3, 4, 1),
            create_test_packet(PacketType::Ack, 4, 5, 1),
            create_test_packet(PacketType::Ack, 5, 6, 1),
            create_test_packet(PacketType::Ack, 6, 7, 1),
            create_test_packet(PacketType::Ack, 7, 8, 1),
        ];


        //we are getting ack back on client from server
        for ack in ack_packets {
            let _ = client.handle_packet(&ack, &mut client_peer);
        }

        println!("{:?}", client_peer);
        // The sender should detect missing packets 3, 4, 5 and retransmit them
        assert!(!client_peer.sent_window.contains_key(&3));
        assert!(!client_peer.sent_window.contains_key(&4));
        assert!(!client_peer.sent_window.contains_key(&5));
        assert!(!client_peer.sent_window.contains_key(&6));


    }


    #[tokio::test]
    async fn test_packet_loss(){

        let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();
        let client_addr = SocketAddr::from_str("127.0.0.1:8081").unwrap();

        let client = create_socket(client_addr).await;
        let server = create_socket(server_addr).await;

        let c_peer = client.get_peer(client_addr).unwrap();
        let s_peer = server.get_peer(server_addr).unwrap();

        let mut server_peer = s_peer.lock().await;
        let mut client_peer = c_peer.lock().await;


        //first we will send from client

        let mut send_packets = |range: Range<u16>|{

            for seq_nr in range{

                let data_packet = create_test_packet(PacketType::Data, seq_nr, client_peer.last_acked, client_peer.sender_id);

                client_peer.sent_window.insert(seq_nr, data_packet.clone());
                server_peer.incoming_buffer.push_back(data_packet.clone());
                let _ = server.handle_packet(&data_packet, &mut server_peer);


            }

        };

        send_packets(0..4);
        send_packets(5..10);
        send_packets(15..23);

        let sack = server.build_selective_ack(&mut server_peer);

        assert_eq!(3, sack.len());

        eprintln!("{:?}", server_peer);

        //Now server will receive them
        //let us create ack for each packet
        let ack_packets = vec![
            create_test_packet(PacketType::Ack, 0, 1, 1),
            create_test_packet(PacketType::Ack, 1, 2, 1),
            create_test_packet(PacketType::Ack, 2, 3, 1),
            create_test_packet(PacketType::Ack, 3, 4, 1),

            //for eg this packet number 4 is lost
            //once this thing happens ... we will send the last acknowledge packet
            create_test_packet(PacketType::Ack, 5, 6, 1),
            create_test_packet(PacketType::Ack, 6, 7, 1),
            create_test_packet(PacketType::Ack, 7, 8, 1),
        ];

        for ack in ack_packets {

            //Now client will receive them
            let _ = client.handle_packet(&ack, &mut client_peer);

        }

        // assert!(client_peer.sent_window.contains_key(&3));
        assert!(client_peer.sent_window.contains_key(&4));
        assert!(client_peer.sent_window.contains_key(&5));
        assert!(client_peer.sent_window.contains_key(&6));

    }

    #[tokio::test]
    async fn test_build_selective_ack() {
        let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();
        let client_addr = SocketAddr::from_str("127.0.0.1:8081").unwrap();

        let client = create_socket(client_addr).await;
        let server = create_socket(server_addr).await;

        let s_peer = server.get_peer(server_addr).unwrap();
        let mut server_peer = s_peer.lock().await;

        server_peer.ack_nr = 3;
        let last_ack = 3;

        // Closure to send packets
        let mut send_packets = |range: Range<u16>| {
            for seq_nr in range {
                let data_packet = create_test_packet(PacketType::Data, seq_nr, last_ack, 1);
                server_peer.incoming_buffer.push_back(data_packet.clone());
                let _ = server.handle_packet(&data_packet, &mut server_peer);
            }
        };

        // Simulate missing packets by skipping 4, 10-14
        send_packets(0..4);
        send_packets(5..10);
        send_packets(15..23);

        // Generate SACK
        let sack = server.build_selective_ack(&mut server_peer);
        eprintln!("Generated SACK: {:?}", sack);
        assert_eq!(4, sack.len()); // Ensure correct SACK length

        // Test that the SACK is properly stored in the packet
        let mut packet = create_test_packet(PacketType::Ack, 0, 1, 1);
        packet.set_sack(sack.clone());

        // Retrieve SACK from packet and verify
        let retrieved_sack = packet.get_sack();
        assert_eq!(retrieved_sack, Some(sack.clone()));

        // Detect lost packets
        let lost_packets = server.get_lost_seq_nr(sack, last_ack, 23);
        eprintln!("Lost packets detected: {:?}", lost_packets);
        assert_eq!(lost_packets, vec![4, 10, 11, 12, 13, 14]); // Expected missing packets
    }
    // #[tokio::test]
    // async fn test_bidirectional_packet_loss() {
    //     // Setup test addresses and sockets
    //     let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();
    //     let client_addr = SocketAddr::from_str("127.0.0.1:8081").unwrap();
    //
    //     let server = create_socket(server_addr).await;
    //     let client = create_socket(client_addr).await;
    //
    //     let s_peer = server.get_peer(server_addr).unwrap();
    //     let c_peer = client.get_peer(client_addr).unwrap();
    //
    //     let mut server_peer = s_peer.lock().await;
    //     let mut client_peer = c_peer.lock().await;
    //
    //     // Part 1: Simulate normal packet flow
    //     eprintln!("=== Testing normal packet flow ===");
    //
    //     for seq_nr in 0..10{
    //         let data_packet = create_test_packet(
    //             PacketType::Data,
    //             seq_nr,
    //             client_peer.last_acked,
    //             client_peer.sender_id
    //         );
    //
    //         client_peer.sent_window.insert(seq_nr, data_packet.clone());
    //         client_peer.seq_nr +=1;
    //
    //     }
    //
    //     // Send packets 0-3 (successful transmission)
    //     for seq_nr in 0..=4 {
    //         // Client sends data packet
    //         let data_packet = create_test_packet(
    //             PacketType::Data,
    //             seq_nr,
    //             client_peer.last_acked,
    //             client_peer.sender_id
    //         );
    //         eprintln!("Client sending data packet seq_nr: {}", seq_nr);
    //
    //         // Client records sent packet
    //         // client_peer.sent_window.insert(seq_nr, data_packet.clone());
    //         server_peer.incoming_buffer.push_back(data_packet.clone());
    //         // Server receives and processes data packet
    //         let ack_response = server.handle_packet(&data_packet, &mut server_peer).unwrap();
    //
    //         // Server sends ACK back
    //         if let Some(ack_packet) = ack_response {
    //             eprintln!("Server sending ACK for seq_nr: {}", seq_nr);
    //             assert_eq!(ack_packet.get_type(), PacketType::Ack);
    //             assert_eq!(ack_packet.ack_nr(), seq_nr+1);
    //
    //             // Client processes ACK
    //
    //             client.handle_packet(&ack_packet, &mut client_peer).unwrap();
    //             // Verify client's sent_window is updated
    //             assert!(!client_peer.sent_window.contains_key(&seq_nr),
    //                     "Client should remove acknowledged packet {} from sent_window", seq_nr);
    //         }
    //     }
    //
    //     // Part 2: Simulate packet loss
    //     eprintln!("=== Testing packet loss scenario ===");
    //
    //     // Send packets with gaps (simulating loss of 4, 5, 6)
    //     for seq_nr in vec![7, 8, 9, 10] {
    //         // Client sends data packet
    //         let data_packet = create_test_packet(
    //             PacketType::Data,
    //             seq_nr,
    //             client_peer.last_acked,
    //             client_peer.sender_id
    //         );
    //         eprintln!("Client sending data packet seq_nr: {}", seq_nr);
    //
    //         // Client records sent packet
    //         // client_peer.sent_window.insert(seq_nr, data_packet.clone());
    //
    //         // Server receives and processes out-of-order packet
    //         let ack_response = server.handle_packet(&data_packet, &mut server_peer).unwrap();
    //
    //         // Server sends SACK
    //         if let Some(ack_packet) = ack_response {
    //             eprintln!("Server sending SACK for out-of-order seq_nr: {}", seq_nr);
    //             assert_eq!(ack_packet.get_type(), PacketType::Ack);
    //             assert!(ack_packet.get_sack().is_some(), "Server should include SACK for out-of-order packets");
    //
    //             // Client processes SACK
    //             client.handle_packet(&ack_packet, &mut client_peer).unwrap();
    //         }
    //     }
    //
    //     // Part 3: Retransmission of lost packets
    //     eprintln!("=== Testing retransmission ===");
    //
    //     for seq_nr in 5..=6 {
    //         // Client retransmits lost packet
    //         let retransmit_packet = create_test_packet(
    //             PacketType::Data,
    //             seq_nr,
    //             client_peer.last_acked,
    //             client_peer.sender_id
    //         );
    //         eprintln!("Client retransmitting packet seq_nr: {}", seq_nr);
    //
    //         // Server receives retransmitted packet
    //         let ack_response = server.handle_packet(&retransmit_packet, &mut server_peer).unwrap();
    //
    //         // Server sends ACK for retransmitted packet
    //         if let Some(ack_packet) = ack_response {
    //             eprintln!("Server acknowledging retransmitted packet seq_nr: {}", seq_nr);
    //
    //             // Client processes ACK
    //             client.handle_packet(&ack_packet, &mut client_peer).unwrap();
    //
    //             // Verify retransmitted packet is acknowledged
    //             // assert!(!client_peer.sent_window.contains_key(&seq_nr),
    //             //         "Client should remove retransmitted packet {} after ACK", seq_nr);
    //
    //         }
    //     }
    //
    //     // Print final statistics
    //     eprintln!("=== Final Statistics ===");
    //     eprintln!("Server last_acked: {}", server_peer.last_acked);
    //     eprintln!("Client sent_window size: {}", client_peer.sent_window.len());
    //     eprintln!("Server out_of_order size: {}", server_peer.out_of_order.len());
    // }


}
