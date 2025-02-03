use std::cmp::{max, min};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::{SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use futures::future::err;
use log::debug;
use tokio::io::ReadBuf;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::time::Instant;
use crate::error::SocketError;
use crate::packet;
use crate::packet::{Packet, PacketType, HEADER_SIZE};
use crate::packet_loss_handler::PacketLossHandler;
use crate::time::{now_microseconds, Delay, Timestamp};
use crate::util::{abs_diff, generate_sequential_identifiers};

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

    udp: UdpSocket,
    sender_connection_id: u16,
    receiver_connection_id: u16,
    seq_nr: u16,
    ack_nr: u16,
    state: SocketState,
    incoming_buffer: Arc<Mutex<VecDeque<Vec<u8>>>>,
    send_window: Vec<Packet>,
    unsent_queue: VecDeque<Packet>,
    duplicate_ack_count: u32,
    last_acked: u16,
    last_acked_timestamp: Timestamp,
    last_dropped: u16,
    rtt: i32,
    rtt_variance: i32,
    pending_data: Vec<u8>,
    curr_window: u32,
    remote_wnd_size: u32,
    base_delays: VecDeque<Delay>,
    current_delays: Vec<DelayDifferenceSample>,
    their_delay: Delay,
    last_rollover: Timestamp,
    congestion_timeout: u64,
    cwnd: u32,
    max_retransmission_retries: u32

}

impl UtpSocket {

    pub async fn bind(addr: Option<SocketAddr>) -> Self{

        let addr = addr.unwrap_or(SocketAddr::from_str("127.0.0.2:0").unwrap());
        let (receiver_id, sender_id) = generate_sequential_identifiers();

        let udp = UdpSocket::bind(addr).await.unwrap();
        Self {
            udp,
            receiver_connection_id: receiver_id,
            sender_connection_id: sender_id,
            seq_nr: 1,
            ack_nr: 0,
            state: SocketState::New,
            incoming_buffer: Arc::new(Mutex::new(VecDeque::new())),
            send_window: Vec::new(),
            unsent_queue: VecDeque::new(),
            duplicate_ack_count: 0,
            last_acked: 0,
            last_acked_timestamp: Timestamp::default(),
            last_dropped: 0,
            rtt: 0,
            rtt_variance: 0,
            pending_data: Vec::new(),
            curr_window: 0,
            remote_wnd_size: 0,
            current_delays: Vec::new(),
            base_delays: VecDeque::with_capacity(BASE_HISTORY),
            their_delay: Delay::default(),
            last_rollover: Timestamp::default(),
            congestion_timeout: INITIAL_CONGESTION_TIMEOUT,
            cwnd: INIT_CWND * MSS,
            max_retransmission_retries: MAX_RETRANSMISSION_RETRIES,

        }

    }

    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let mut packet = Packet::new();
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(self.receiver_connection_id);
        packet.set_seq_nr(self.seq_nr);


        match self.udp.connect(addr).await{
            Ok(..) => {
                //Now we will send the Sync packet
                match self.send(packet.as_ref()).await {
                    Ok(len) => {
                        println!("sent {} bytes", len);
                        let mut buf = [0; BUF_SIZE];
                        //once I am able to send the message
                        //I need to also wait for acknowledgement

                            // match self.recv_from(&mut buf).await{
                            //     Ok((read, src)) => {
                            //         println!(" ----->> read {} bytes from {:?}", read, src);
                            //
                            //     }
                            //     Err(err) => {
                            //         println!("error reading from {:?}", err);
                            //     }
                            // }
                        Ok(())
                    }
                    Err(err) => {
                        eprintln!("error sending packet: {}", err);
                        Err(err)
                    }
                }
            }
            Err(error) => {
                println!("Could not connect to the server {:?}", error);
                Err(error)
            }
        }

    }

    pub async fn start_receiver(self: Arc<Self>){

        let mut buf = [0; BUF_SIZE];

        loop {

            match self.udp.recv_from(&mut buf).await{
                Ok((len, addr)) => {
                    let payload = buf[..len].to_vec();
                    let mut buffer = self.incoming_buffer.lock().unwrap();
                    buffer.push_back(payload);

                }
                Err(error) => {
                    eprintln!("Error receiving data: {:?}", error);
                }
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
    pub fn send_packets(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> io::Result<()> {

        if self.state == SocketState::Closed{
            return Err(SocketError::ConnectionClosed.into());
        }

        let total_len = buf.len();
        //even if the file is very big
        //it shall divide the bytes into 25 byte size
        //it shall be something 20 byte is a header size and 5 byte is the data
        for chunk in buf.chunks(MSS as usize ){
            let mut packet = Packet::with_payload(chunk);
            packet.set_seq_nr(self.seq_nr);
            packet.set_ack_nr(self.ack_nr);
            packet.set_connection_id(self.sender_connection_id);

            self.unsent_queue.push_back(packet);

            self.seq_nr = self.seq_nr.wrapping_add(1);

        }

        while let Some(mut packet) = self.unsent_queue.pop_front() {
            //need to apply some congestion logic here shall do after some time
            match self.poll_send(cx, packet.as_ref()){
                Poll::Ready(val) => {
                    println!("Send {}", val?);
                }
                Poll::Pending => {
                    println!("Pending");
                }
            };

        }

        //after send packets we also need to have the acknowledgement




        Ok(())

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

    pub fn recv_packets_from(&mut self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<SocketAddr>> {
        // Poll the socket to receive data

        match self.poll_recv_from(cx, buf) {
            Poll::Ready(Ok(addr)) => {
                // Attempt to parse the packet from the buffer
                println!("--------->> {:?} received", addr);

                match <Packet as packet::TryFrom<&mut ReadBuf>>::try_from(buf) {
                    Ok(packet) => {


                        //we need to handle this packet now depending upon its type and all the other thing
                        if let Ok(Some(rsp)) = self.handle_packet(&packet){
                            match self.poll_send_to(cx, rsp.as_ref(), addr){
                                Poll::Ready(len) => {
                                    println!("Syn packet sent to {:?} of len  {}", addr, len.unwrap());

                                }
                                Poll::Pending => {
                                    eprintln!("Syn packet not sent to {:?} ", addr);
                                }
                            }
                        }
                        Poll::Ready(Ok(addr)) // Return the address if successful
                    }
                    Err(e) => {
                        // Convert the ParseError into an io::Error
                        let io_error = io::Error::new(io::ErrorKind::InvalidData, e.to_string());
                        Poll::Ready(Err(io_error))
                    }
                }
            }
            Poll::Ready(Err(e)) => {
                // Propagate the IO error if poll_recv_from fails
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                // If the socket is not ready, return Pending
                Poll::Pending
            }
        }
    }


    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.udp.peer_addr()
    }
    pub async fn send_packet(&mut self, data: &[u8]) -> io::Result<()> {

        let seq_nr = self.seq_nr;
        let time_stamp = now_microseconds();
        let mut packet = Packet::with_payload(b"Hello");
        packet.set_seq_nr(seq_nr);
        packet.set_timestamp(time_stamp);
        packet.set_type(PacketType::Syn);

        self.unsent_queue.push_back(packet);

        Ok(())



    }

    fn handle_packet(&mut self, packet: &Packet) -> io::Result<Option<Packet>> {
        debug!("({:?}, {:?})", self.state, packet.get_type());

        //Now this response shall depend on the socket state and packet type

        match (self.state, packet.get_type()) {
            //if the socket is just started and I am getting a connection request
            (SocketState::New, PacketType::Syn) => self.handle_new_state(packet),
            //In case I get a sync packet, I will just reset the socket
            (_, PacketType::Syn) => {
                Ok(Some(self.prepare_reply(packet, PacketType::Reset)))
            },
            //when packet has been sent, socket is in syn sent state, it waits for acknowledgement, and it gets packet_type == State
            (SocketState::SynSent, PacketType::State) =>{
                self.ack_nr = packet.ack_nr();
                self.seq_nr +=1;
                self.state = SocketState::Connected;
                self.last_acked = packet.ack_nr();
                self.last_acked_timestamp = now_microseconds();
                Ok(None)
            }
            //if the socket is in syn state, any packet_type other than state is invalid
            (SocketState::SynSent, _) => Err(SocketError::InvalidReply.into()),

            //if socket is in connected state .. or waiting for reply after sending finish packet
            (SocketState::Connected, PacketType::Data)
            | (SocketState::FinSent, PacketType::Data)=> {

                let expected_packet_type = if self.state == SocketState::FinSent {
                    PacketType::Fin
                }else{
                    PacketType::State
                };
                let mut reply = self.prepare_reply(packet, expected_packet_type);
                if packet.seq_nr().wrapping_sub(self.ack_nr)>1{
                    println!("current ack_nr ({}) is behind packet seq_nr ({})", self.ack_nr, packet.seq_nr());
                    //set sack extension payload if the packet is not in order
                    // todo!()

                }
                Ok(Some(reply))
            },
            //I think this state if acknowledgement
            (SocketState::Connected, PacketType::State )=> {

                if packet.ack_nr() == self.last_acked {
                    self.duplicate_ack_count +=1;
                }else{

                    self.last_acked = packet.ack_nr();
                    self.last_acked_timestamp = now_microseconds();
                    self.duplicate_ack_count = 1;

                }
                //we need to update the congestion window here
                // todo!()
                Ok(None)
            },
            //If I am waiting for Fin packet ... in connected state or FinSent State
            //Here I need to close the connection
            (SocketState::Connected, PacketType::Fin)
            | (SocketState::FinSent, PacketType::Fin)=> {
                println!("Fin received but there are missing acknowledgments for sent packets");

                if packet.ack_nr() < self.seq_nr {

                    println!("Fin received but there are missing acknowledgments for sent packets");

                }

                let mut reply = self.prepare_reply(packet, PacketType::State);

                if packet.seq_nr().wrapping_sub(self.ack_nr)>1{

                    println!("current ack_nr ({}) is behind received packet seq_nr ({})", self.ack_nr, packet.seq_nr());

                }

                //We need to set SACK extension payload if the packet is not in order

                self.state = SocketState::Closed;
                Ok(Some(reply))

            },
            //I think if the SocketState is closed ... it shall not handle any packet
            (SocketState::Closed, PacketType::Fin )=> {

                Ok(Some(self.prepare_reply(packet, PacketType::State)))
            },
            //After sending Fin Packet , waiting for the response
            (SocketState::FinSent, PacketType::State )=> {
                if packet.ack_nr() == self.seq_nr {
                    self.state = SocketState::Closed;
                } else {
                    //this means we have lost some packet

                    if packet.ack_nr() == self.last_acked {
                        self.duplicate_ack_count += 1;
                    }else{

                        self.last_acked = packet.ack_nr();
                        self.last_acked_timestamp = now_microseconds();
                        self.duplicate_ack_count = 1;
                    }

                    //we need to update the congestion window



                }
                Ok(None)
            },
            //For any SocketState, if I receive a reset packet
            (_, PacketType::Reset) => {
                self.state = SocketState::ResetReceived;
                Err(SocketError::ConnectionReset.into())
            }
            (state, ty) => {
                let message = format!("Unimplemented handling for ({:?},{:?})", state, ty);
                debug!("{}", message);
                Err(SocketError::Other(message).into())
            }
        }


    }

    ///when new connection is established
    fn handle_new_state(&mut self, packet: &Packet) -> io::Result<Option<Packet>> {
            //this means our socket state is new and it only accepts connection requestion
            self.ack_nr = packet.ack_nr();
            self.seq_nr = 0;
            self.receiver_connection_id = packet.connection_id() +1;
            self.sender_connection_id = packet.connection_id();
            self.state = SocketState::Connected;
            self.last_dropped = self.ack_nr;

            Ok(Some(self.prepare_reply(packet, PacketType::State)))


    }

    fn prepare_reply(&self, original: &Packet, t: PacketType) -> Packet {
        let mut resp = Packet::new();
        resp.set_type(t);
        let self_t_micro = now_microseconds();
        let other_t_micro = original.timestamp();
        let time_difference: Delay = abs_diff(self_t_micro, other_t_micro);
        resp.set_timestamp(self_t_micro);
        resp.set_timestamp_difference(time_difference);
        resp.set_connection_id(self.sender_connection_id);
        resp.set_seq_nr(self.seq_nr);
        resp.set_ack_nr(self.ack_nr);

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