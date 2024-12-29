use std::cmp::{max, min};
use crate::time::*;
use crate::util::*;
use bytes::BufMut;
use log::{debug, error, info};
use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::ReadBuf;
use tokio::net::ToSocketAddrs;
use tokio::time::Instant;
use crate::error::SocketError;
use crate::packet;
use crate::packet::*;

const BUF_SIZE: usize = 1500;
const GAIN: f64 = 1.0;
const ALLOWED_INCREASE: u32 = 1;
const TARGET: f64 = 100_000.0;
const MSS: u32 = 1400;
const MIN_CWND: u32 = 2;
const INIT_CWND: u32 = 2;
const INITIAL_CONGESTION_TIMEOUT: u64 = 1000;
const MIN_CONGESTION_TIMEOUT: u64 = 500;
const MAX_CONGESTION_TIMEOUT: u64 = 60_000;
const BASE_HISTORY: usize = 10;
const MAX_SYN_RETRIES: u32 = 5;
const MAX_RETRANSMISSION_RETRIES: u32 = 5;
const WINDOW_SIZE: u32 = 1024*1024;
const PRE_SEND_TIMEOUT: u32 = 500_000;
const MAX_BASE_DELAY_AGE: Delay = Delay(60_000_000);


#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SocketState {
    New,
    Connected,
    SynSent,
    FinSent,
    ResetReceived,
    Closed
}

#[derive(Debug)]
struct DelayDifferenceSample {
    received_at: Timestamp,
    difference: Delay
}

#[derive(Debug)]
pub struct UtpSocket {
    socket: tokio::net::UdpSocket,

    ///Remote peer
    connected_to: SocketAddr,

    ///Sender connected identifier
    sender_connection_id: u16,

    ///Receiver connection identifier
    receiver_connection_id: u16,

    ///Sequence number for the next packet
    seq_nr: u16,

    ///Sequence number of the latest acknowledged packet sent by the remote peer
    ack_nr: u16,

    ///Socket state
    state: SocketState,

    ///Received but not acknowledged packets
    incoming_buffer: Vec<Packet>,

    ///Sent but not yet acknowledged packets
    send_window: Vec<Packet>,

    ///Packets not yet sent
    unsent_queue: VecDeque<Packet>,

    ///How many ACKs did the socket received for packet with sequence number equal to `ack_nr`
    duplicate_ack_count: u32,

    ///Sequence number of the latest packet the remote peer acknowledged
    last_acked: u16,

    ///Timestamp of the latest packet the remote peer acknowledged
    last_acked_timestamp: Timestamp,

    ///Sequence number of the last packet removed from the incoming buffer
    last_dropped: u16,

    ///Round-trip time to remote peer
    rtt: i32,

    ///Variance of the round_trip time to the remote peer
    rtt_variance: i32,

    ///Data from the latest packet not yet returned in `recv_from`
    pending_data: Vec<u8>,

    ///Bytes in flight
    curr_window: u32,

    ///Window size of the remote peer
    remote_wnd_size: u32,

    ///Rolling window of packet delay to remote peer
    base_delays: VecDeque<Delay>,

    ///Rolling window of the difference between sending a packet and receiving its acknowledgement
    current_delays: Vec<DelayDifferenceSample>,

    ///Difference between timestamp of the latest packet received and time of reception
    their_delay: Delay,

    ///Start of the current minute for sampling purposes
    last_rollover: Timestamp,

    ///Current congestion timeout in milliseconds
    congestion_timeout: u64,

    ///Congestion window in bytes
    cwnd: u32,

    ///Maximum retransmission retries
    max_retransmission_retries: u32

}

impl UtpSocket {

    fn from_raw_parts(socket: tokio::net::UdpSocket, src: SocketAddr) -> UtpSocket {

        let (receiver_id, sender_id) = generate_sequential_identifiers();

        UtpSocket{
            socket,
            connected_to: src,
            receiver_connection_id: receiver_id,
            sender_connection_id: sender_id,
            seq_nr: 1,
            ack_nr: 0,
            state: SocketState::New,
            incoming_buffer: Vec::new(),
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

    pub async fn bind<A>(addr: A) -> Result<Self, tokio::io::Error>
    where A: ToSocketAddrs + Copy{
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        let addr = tokio::net::lookup_host(addr)
            .await?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "could not resolve to any address"))?;
        Ok(UtpSocket::from_raw_parts(socket, addr))
    }

    pub async fn connect<A>(addr: A) -> io::Result<UtpSocket>
    where A: ToSocketAddrs {
        // let udp_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let remote_addr = tokio::net::lookup_host(addr)
            .await?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "could not resolve to any address"))?;


        let mut socket = UtpSocket::bind("0.0.0.0:0").await?;
        socket.connected_to = remote_addr;

        let mut packet = Packet::new();
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(socket.sender_connection_id);
        packet.set_seq_nr(socket.seq_nr);
        let mut len = 0;
        let mut buf = [0; BUF_SIZE];
        let mut syn_timeout = socket.congestion_timeout;
        for _ in 0..MAX_SYN_RETRIES {
            packet.set_timestamp(now_microseconds());
            info!("Connection to {}", socket.connected_to);
            socket.socket.send_to(packet.as_ref(), socket.connected_to).await?;
            socket.state = SocketState::SynSent;
            info!("send {:?}", packet);
            //validate response
            match socket.socket.recv_from(&mut buf).await{
                Ok((read, src)) => {
                    info!("validating response from {:?} and data to read is {}", src, read);
                    info!(" and the data is {:?}", &buf[..read]);
                    socket.connected_to = src;
                    len = read;
                    break;
                }
                Err(ref e)
                    if (e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut) =>{
                    error!("Connection to {} timed out", socket.connected_to);
                    syn_timeout +=2;
                    continue;
                }
                Err(e) => return Err(e),
            };
        }

        let addr = socket.connected_to;
        // let packet = Packet::try_from(&buf[..len])?;
        info!("received {:?}", buf.len());
        socket.handle_packet(&packet, addr).await?;
        info!("Connection from {}", socket.connected_to);
        Ok(socket)
    }

    async fn handle_packet(&mut self, packet: &Packet, src: SocketAddr) -> io::Result<Option<Packet>> {
        info!("({:?}, {:?})", self.state, packet.get_type());

        // Acknowledge only if the packet strictly follows the previous one
        if packet.seq_nr().wrapping_sub(self.ack_nr) == 1 {
            self.ack_nr = packet.seq_nr();
        }

        // Reset connection if connection id doesn't match and this isn't a SYN
        if packet.get_type() != PacketType::Syn
            && self.state != SocketState::SynSent
            && !(packet.connection_id() == self.sender_connection_id
            || packet.connection_id() == self.receiver_connection_id)
        {
            return Ok(Some(self.prepare_reply(packet, PacketType::Reset).await));
        }

        // Update remote window size
        self.remote_wnd_size = packet.wnd_size();
        info!("self.remote_wnd_size: {}", self.remote_wnd_size);

        // Update remote peer's delay between them sending the packet and us receiving it
        let now = now_microseconds();
        self.their_delay = abs_diff(now, packet.timestamp());
        info!("self.their_delay: {}", self.their_delay);

        match (self.state, packet.get_type()) {
            (SocketState::New, PacketType::Syn) => {
                self.connected_to = src;
                self.ack_nr = packet.seq_nr();
                self.seq_nr = rand::random();
                self.receiver_connection_id = packet.connection_id() + 1;
                self.sender_connection_id = packet.connection_id();
                self.state = SocketState::Connected;
                self.last_dropped = self.ack_nr;

                Ok(Some(self.prepare_reply(packet, PacketType::State).await))
            }
            (_, PacketType::Syn) => Ok(Some(self.prepare_reply(packet, PacketType::Reset).await)),
            (SocketState::SynSent, PacketType::State) => {
                self.connected_to = src;
                self.ack_nr = packet.seq_nr();
                self.seq_nr += 1;
                self.state = SocketState::Connected;
                self.last_acked = packet.ack_nr();
                self.last_acked_timestamp = now_microseconds();
                Ok(None)
            }
            (SocketState::SynSent, _) => Err(SocketError::InvalidReply.into()),
            (SocketState::Connected, PacketType::Data)
            | (SocketState::FinSent, PacketType::Data) => Ok(self.handle_data_packet(packet).await),
            (SocketState::Connected, PacketType::State) => {
                self.handle_state_packet(packet);
                Ok(None)
            }
            (SocketState::Connected, PacketType::Fin) | (SocketState::FinSent, PacketType::Fin) => {
                if packet.ack_nr() < self.seq_nr {
                    info!("FIN received but there are missing acknowledgements for sent packets");
                }
                let mut reply = self.prepare_reply(packet, PacketType::State).await;
                if packet.seq_nr().wrapping_sub(self.ack_nr) > 1 {
                    info!(
                        "current ack_nr ({}) is behind received packet seq_nr ({})",
                        self.ack_nr,
                        packet.seq_nr()
                    );

                    // Set SACK extension payload if the packet is not in order
                    let sack = self.build_selective_ack();

                    if !sack.is_empty() {
                        reply.set_sack(sack);
                    }
                }

                // Give up, the remote peer might not care about our missing packets
                self.state = SocketState::Closed;
                Ok(Some(reply))
            }
            (SocketState::Closed, PacketType::Fin) => {
                Ok(Some(self.prepare_reply(packet, PacketType::State).await))
            }
            (SocketState::FinSent, PacketType::State) => {
                if packet.ack_nr() == self.seq_nr {
                    self.state = SocketState::Closed;
                } else {
                    self.handle_state_packet(packet);
                }
                Ok(None)
            }
            (_, PacketType::Reset) => {
                self.state = SocketState::ResetReceived;
                Err(SocketError::ConnectionReset.into())
            }
            (state, ty) => {
                let message = format!("Unimplemented handling for ({:?},{:?})", state, ty);
                info!("{}", message);
                Err(SocketError::Other(message).into())
            }
        }
    }

    async fn handle_data_packet(&mut self, packet: &Packet) -> Option<Packet> {
        // If a FIN was previously sent, reply with a FIN packet acknowledging the received packet.
        let packet_type = if self.state == SocketState::FinSent {
            PacketType::Fin
        } else {
            PacketType::State
        };
        let mut reply = self.prepare_reply(packet, packet_type).await;

        if packet.seq_nr().wrapping_sub(self.ack_nr) > 1 {
            info!(
                "current ack_nr ({}) is behind received packet seq_nr ({})",
                self.ack_nr,
                packet.seq_nr()
            );

            // Set SACK extension payload if the packet is not in order
            let sack = self.build_selective_ack();

            if !sack.is_empty() {
                reply.set_sack(sack);
            }
        }

        Some(reply)
    }
    fn build_selective_ack(&self) -> Vec<u8> {
        let stashed = self
            .incoming_buffer
            .iter()
            .filter(|pkt| pkt.seq_nr() > self.ack_nr + 1)
            .map(|pkt| (pkt.seq_nr() - self.ack_nr - 2) as usize)
            .map(|diff| (diff / 8, diff % 8));

        let mut sack = Vec::new();
        for (byte, bit) in stashed {
            // Make sure the amount of elements in the SACK vector is a
            // multiple of 4 and enough to represent the lost packets
            while byte >= sack.len() || sack.len() % 4 != 0 {
                sack.push(0u8);
            }

            sack[byte] |= 1 << bit;
        }

        sack
    }

    fn update_current_delay(&mut self, v: Delay, now: Timestamp) {
        // Remove samples more than one RTT old
        let rtt = (self.rtt as i64 * 100).into();
        while !self.current_delays.is_empty() && now - self.current_delays[0].received_at > rtt {
            self.current_delays.remove(0);
        }

        // Insert new measurement
        self.current_delays.push(DelayDifferenceSample {
            received_at: now,
            difference: v,
        });
    }

    fn handle_state_packet(&mut self, packet: &Packet) {
        if packet.ack_nr() == self.last_acked {
            self.duplicate_ack_count += 1;
        } else {
            self.last_acked = packet.ack_nr();
            self.last_acked_timestamp = now_microseconds();
            self.duplicate_ack_count = 1;
        }

        // Update congestion window size
        if let Some(index) = self
            .send_window
            .iter()
            .position(|p| packet.ack_nr() == p.seq_nr())
        {
            // Calculate the sum of the size of every packet implicitly and explicitly acknowledged
            // by the inbound packet (i.e., every packet whose sequence number precedes the inbound
            // packet's acknowledgement number, plus the packet whose sequence number matches)
            let bytes_newly_acked = self
                .send_window
                .iter()
                .take(index + 1)
                .fold(0, |acc, p| acc + p.len());

            // Update base and current delay
            let now = now_microseconds();
            let our_delay = now - self.send_window[index].timestamp();
            info!("our_delay: {}", our_delay);
            self.update_base_delay(our_delay, now);
            self.update_current_delay(our_delay, now);

            let off_target: f64 = (TARGET - u32::from(self.queuing_delay()) as f64) / TARGET;
            info!("off_target: {}", off_target);

            self.update_congestion_window(off_target, bytes_newly_acked as u32);

            // Update congestion timeout
            let rtt = u32::from(our_delay - self.queuing_delay()) / 1000; // in milliseconds
            self.update_congestion_timeout(rtt as i32);
        }

        let mut packet_loss_detected: bool =
            !self.send_window.is_empty() && self.duplicate_ack_count == 3;

        // Process extensions, if any
        for extension in packet.extensions() {
            if extension.get_type() == ExtensionType::SelectiveAck {
                // If three or more packets are acknowledged past the implicit missing one,
                // assume it was lost.
                if extension.iter().count_ones() >= 3 {
                    self.resend_lost_packet(packet.ack_nr() + 1);
                    packet_loss_detected = true;
                }

                if let Some(last_seq_nr) = self.send_window.last().map(Packet::seq_nr) {
                    let lost_packets = extension
                        .iter()
                        .enumerate()
                        .filter(|&(_, received)| !received)
                        .map(|(idx, _)| packet.ack_nr() + 2 + idx as u16)
                        .take_while(|&seq_nr| seq_nr < last_seq_nr);

                    for seq_nr in lost_packets {
                        info!("SACK: packet {} lost", seq_nr);
                        self.resend_lost_packet(seq_nr);
                        packet_loss_detected = true;
                    }
                }
            } else {
                info!("Unknown extension {:?}, ignoring", extension.get_type());
            }
        }

        // Three duplicate ACKs mean a fast resend request. Resend the first unacknowledged packet
        // if the incoming packet doesn't have a SACK extension. If it does, the lost packets were
        // already resent.
        if !self.send_window.is_empty()
            && self.duplicate_ack_count == 3
            && !packet
            .extensions()
            .any(|ext| ext.get_type() == ExtensionType::SelectiveAck)
        {
            self.resend_lost_packet(packet.ack_nr() + 1);
        }

        // Packet lost, halve the congestion window
        if packet_loss_detected {
            info!("packet loss detected, halving congestion window");
            self.cwnd = max(self.cwnd / 2, MIN_CWND * MSS);
            info!("cwnd: {}", self.cwnd);
        }

        // Success, advance send window
        self.advance_send_window();
    }

    fn update_base_delay(&mut self, base_delay: Delay, now: Timestamp) {
        if self.base_delays.is_empty() || now - self.last_rollover > MAX_BASE_DELAY_AGE {
            // Update last rollover
            self.last_rollover = now;

            // Drop the oldest sample, if need be
            if self.base_delays.len() == BASE_HISTORY {
                self.base_delays.pop_front();
            }

            // Insert new sample
            self.base_delays.push_back(base_delay);
        } else {
            // Replace sample for the current minute if the delay is lower
            let last_idx = self.base_delays.len() - 1;
            if base_delay < self.base_delays[last_idx] {
                self.base_delays[last_idx] = base_delay;
            }
        }
    }
    fn update_congestion_window(&mut self, off_target: f64, bytes_newly_acked: u32) {
        let flightsize = self.curr_window;

        let cwnd_increase = GAIN * off_target * bytes_newly_acked as f64 * MSS as f64;
        let cwnd_increase = cwnd_increase / self.cwnd as f64;
        info!("cwnd_increase: {}", cwnd_increase);

        self.cwnd = (self.cwnd as f64 + cwnd_increase) as u32;
        let max_allowed_cwnd = flightsize + ALLOWED_INCREASE * MSS;
        self.cwnd = min(self.cwnd, max_allowed_cwnd);
        self.cwnd = max(self.cwnd, MIN_CWND * MSS);

        info!("cwnd: {}", self.cwnd);
        info!("max_allowed_cwnd: {}", max_allowed_cwnd);
    }
    fn advance_send_window(&mut self) {
        // The reason I'm not removing the first element in a loop while its sequence number is
        // smaller than `last_acked` is because of wrapping sequence numbers, which would create the
        // sequence [..., 65534, 65535, 0, 1, ...]. If `last_acked` is smaller than the first
        // packet's sequence number because of wraparound (for instance, 1), no packets would be
        // removed, as the condition `seq_nr < last_acked` would fail immediately.
        //
        // On the other hand, I can't keep removing the first packet in a loop until its sequence
        // number matches `last_acked` because it might never match, and in that case no packets
        // should be removed.
        if let Some(position) = self
            .send_window
            .iter()
            .position(|packet| packet.seq_nr() == self.last_acked)
        {
            for _ in 0..position + 1 {
                let packet = self.send_window.remove(0);
                self.curr_window -= packet.len() as u32;
            }
        }
        info!("self.curr_window: {}", self.curr_window);
    }
    fn queuing_delay(&self) -> Delay {
        let filtered_current_delay = self.filtered_current_delay();
        let min_base_delay = self.min_base_delay();
        let queuing_delay = filtered_current_delay - min_base_delay;

        info!("filtered_current_delay: {}", filtered_current_delay);
        info!("min_base_delay: {}", min_base_delay);
        info!("queuing_delay: {}", queuing_delay);

        queuing_delay
    }
    fn min_base_delay(&self) -> Delay {
        self.base_delays.iter().min().cloned().unwrap_or_default()
    }

    fn filtered_current_delay(&self) -> Delay {
        let input = self.current_delays.iter().map(|delay| &delay.difference);
        (ewma(input, 0.333) as i64).into()
    }
    fn update_congestion_timeout(&mut self, current_delay: i32) {
        let delta = self.rtt - current_delay;
        self.rtt_variance += (delta.abs() - self.rtt_variance) / 4;
        self.rtt += (current_delay - self.rtt) / 8;
        self.congestion_timeout = max(
            (self.rtt + self.rtt_variance * 4) as u64,
            MIN_CONGESTION_TIMEOUT,
        );
        self.congestion_timeout = min(self.congestion_timeout, MAX_CONGESTION_TIMEOUT);

        info!("current_delay: {}", current_delay);
        info!("delta: {}", delta);
        info!("self.rtt_variance: {}", self.rtt_variance);
        info!("self.rtt: {}", self.rtt);
        info!("self.congestion_timeout: {}", self.congestion_timeout);
    }

    fn resend_lost_packet(&mut self, lost_packet_nr: u16) {
        info!("---> resend_lost_packet({}) <---", lost_packet_nr);
        match self
            .send_window
            .iter()
            .position(|pkt| pkt.seq_nr() == lost_packet_nr)
        {
            None => info!("Packet {} not found", lost_packet_nr),
            Some(position) => {
                info!("self.send_window.len(): {}", self.send_window.len());
                info!("position: {}", position);
                let mut packet = self.send_window[position].clone();
                // FIXME: Unchecked result
                let _ = self.send_packet(&mut packet);

                // We intentionally don't increase `curr_window` because otherwise a packet's length
                // would be counted more than once
            }
        }
        info!("---> END resend_lost_packet <---");
    }

    #[inline]
    async fn send_packet(&mut self, packet: &mut Packet) -> io::Result<()> {
        info!("current window: {}", self.send_window.len());
        let max_inflight = min(self.cwnd, self.remote_wnd_size);
        let max_inflight = max(MIN_CWND * MSS, max_inflight);
        let now = now_microseconds();

        // Wait until enough in-flight packets are acknowledged for rate control purposes, but don't
        // wait more than 500 ms (PRE_SEND_TIMEOUT) before sending the packet.
        while self.curr_window >= max_inflight && now_microseconds() - now < PRE_SEND_TIMEOUT.into()
        {
            info!("self.curr_window: {}", self.curr_window);
            info!("max_inflight: {}", max_inflight);
            info!("self.duplicate_ack_count: {}", self.duplicate_ack_count);
            info!("now_microseconds() - now = {}", now_microseconds() - now);
            let mut buf = [0; BUF_SIZE];
            self.recv(&mut buf).await?;
        }
        info!(
            "out: now_microseconds() - now = {}",
            now_microseconds() - now
        );

        // Check if it still makes sense to send packet, as we might be trying to resend a lost
        // packet acknowledged in the receive loop above.
        // If there were no wrapping around of sequence numbers, we'd simply check if the packet's
        // sequence number is greater than `last_acked`.
        let distance_a = packet.seq_nr().wrapping_sub(self.last_acked);
        let distance_b = self.last_acked.wrapping_sub(packet.seq_nr());
        if distance_a > distance_b {
            info!("Packet already acknowledged, skipping...");
            return Ok(());
        }

        packet.set_timestamp(now_microseconds());
        packet.set_timestamp_difference(self.their_delay);
        self.socket.send_to(packet.as_ref(), self.connected_to).await?;
        info!("sent {:?}", packet);

        Ok(())
    }

    pub async fn close(&mut self) -> io::Result<()> {
        // Nothing to do if the socket's already closed or not connected
        if self.state == SocketState::Closed
            || self.state == SocketState::New
            || self.state == SocketState::SynSent
        {
            return Ok(());
        }

        // Flush unsent and unacknowledged packets
        self.flush().await?;

        let mut packet = Packet::new();
        packet.set_connection_id(self.sender_connection_id);
        packet.set_seq_nr(self.seq_nr);
        packet.set_ack_nr(self.ack_nr);
        packet.set_timestamp(now_microseconds());
        packet.set_type(PacketType::Fin);

        // Send FIN
        self.socket.send_to(packet.as_ref(), self.connected_to).await?;
        info!("sent {:?}", packet);
        self.state = SocketState::FinSent;

        // Receive JAKE
        let mut buf = [0; BUF_SIZE];
        while self.state != SocketState::Closed {
            self.recv(&mut buf).await?;
        }

        Ok(())
    }

    async fn prepare_reply(&self, original: &Packet, t: PacketType) -> Packet {
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

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
    fn flush_incoming_buffer(&mut self, buf: &mut [u8]) -> usize {
        fn unsafe_copy(src: &[u8], dst: &mut [u8]) -> usize {
            let max_len = min(src.len(), dst.len());
            unsafe {
                use std::ptr::copy;
                copy(src.as_ptr(), dst.as_mut_ptr(), max_len);
            }
            max_len
        }

        // Return pending data from a partially read packet
        if !self.pending_data.is_empty() {
            let flushed = unsafe_copy(&self.pending_data[..], buf);

            if flushed == self.pending_data.len() {
                self.pending_data.clear();
                self.advance_incoming_buffer();
            } else {
                self.pending_data = self.pending_data[flushed..].to_vec();
            }

            return flushed;
        }

        if !self.incoming_buffer.is_empty()
            && (self.ack_nr == self.incoming_buffer[0].seq_nr()
            || self.ack_nr + 1 == self.incoming_buffer[0].seq_nr())
        {
            let flushed = unsafe_copy(&self.incoming_buffer[0].payload(), buf);

            if flushed == self.incoming_buffer[0].payload().len() {
                self.advance_incoming_buffer();
            } else {
                self.pending_data = self.incoming_buffer[0].payload()[flushed..].to_vec();
            }

            return flushed;
        }

        0
    }
    fn advance_incoming_buffer(&mut self) -> Option<Packet> {
        if !self.incoming_buffer.is_empty() {
            let packet = self.incoming_buffer.remove(0);
            info!("Removed packet from incoming buffer: {:?}", packet);
            self.ack_nr = packet.seq_nr();
            self.last_dropped = self.ack_nr;
            Some(packet)
        } else {
            None
        }
    }
    pub async fn flush(&mut self) -> std::io::Result<()> {
        let mut buf = [0u8; BUF_SIZE];
        while !self.send_window.is_empty() {
            info!("packets in send window: {}", self.send_window.len());
            self.recv(&mut buf).await?;
        }

        Ok(())
    }

    // pub async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    //     self.socket.recv(buf).await
    // }
    // pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    //     info!("got something {:?}", buf.len());
    //     self.socket.recv_from(buf).await
    // }

    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut b = [0; BUF_SIZE + HEADER_SIZE];
        let start = Instant::now();
        let (read, src);
        let mut retries = 0;

        // Try to receive a packet and handle timeouts
        loop {
            // Abort loop if the current try exceeds the maximum number of retransmission retries.
            if retries >= self.max_retransmission_retries {
                self.state = SocketState::Closed;
                return Err(SocketError::ConnectionTimedOut.into());
            }

            let timeout = if self.state != SocketState::New {
                debug!("setting read timeout of {} ms", self.congestion_timeout);
                Some(Duration::from_millis(self.congestion_timeout))
            } else {
                None
            };
            //
            // self.socket
            //     .set_read_timeout(timeout)
            //     .expect("Error setting read timeout");
            match self.socket.recv_from(&mut b).await {
                Ok((r, s)) => {
                    read = r;
                    src = s;
                    break;
                }
                Err(ref e)
                if (e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut) =>
                    {
                        debug!("recv_from timed out");
                        self.handle_receive_timeout().await?;
                    }
                Err(e) => return Err(e),
            };

            let elapsed = start.elapsed();
            let elapsed_ms = elapsed.as_secs() * 1000 + elapsed.subsec_millis() as u64;
            debug!("{} ms elapsed", elapsed_ms);
            retries += 1;
        }

        // Decode received data into a packet
        let packet = match <Packet as packet::TryFrom<&[u8]>>::try_from(&b[..read]) {
            Ok(packet) => packet,
            Err(e) => {
                debug!("{}", e);
                debug!("Ignoring invalid packet");
                return Ok((0, self.connected_to));
            }
        };
        debug!("received {:?}", packet);

        // Process packet, including sending a reply if necessary
        if let Some(mut pkt) = self.handle_packet(&packet, src).await? {
            pkt.set_wnd_size(WINDOW_SIZE);
            self.socket.send_to(pkt.as_ref(), src).await?;
            debug!("sent {:?}", pkt);
        }

        // Insert data packet into the incoming buffer if it isn't a duplicate of a previously
        // discarded packet
        if packet.get_type() == PacketType::Data
            && packet.seq_nr().wrapping_sub(self.last_dropped) > 0
        {
            self.insert_into_buffer(packet);
        }

        // Flush incoming buffer if possible
        let read = self.flush_incoming_buffer(buf);

        Ok((read, src))
    }

    async fn send_fast_resend_request(&self) {
        for _ in 0..3 {
            let mut packet = Packet::new();
            packet.set_type(PacketType::State);
            let self_t_micro = now_microseconds();
            packet.set_timestamp(self_t_micro);
            packet.set_timestamp_difference(self.their_delay);
            packet.set_connection_id(self.sender_connection_id);
            packet.set_seq_nr(self.seq_nr);
            packet.set_ack_nr(self.ack_nr);
            let _ = self.socket.send_to(packet.as_ref(), self.connected_to).await;
        }
    }
    fn insert_into_buffer(&mut self, packet: Packet) {
        // Immediately push to the end if the packet's sequence number comes after the last
        // packet's.
        if self
            .incoming_buffer
            .last()
            .map_or(false, |p| packet.seq_nr() > p.seq_nr())
        {
            self.incoming_buffer.push(packet);
        } else {
            // Find index following the most recent packet before the one we wish to insert
            let i = self
                .incoming_buffer
                .iter()
                .filter(|p| p.seq_nr() < packet.seq_nr())
                .count();

            if self
                .incoming_buffer
                .get(i)
                .map_or(true, |p| p.seq_nr() != packet.seq_nr())
            {
                self.incoming_buffer.insert(i, packet);
            }
        }
    }
    async fn handle_receive_timeout(&mut self) -> std::io::Result<()> {
        self.congestion_timeout *= 2;
        self.cwnd = MSS;

        // There are three possible cases here:
        //
        // - If the socket is sending and waiting for acknowledgements (the send window is
        //   not empty), resend the first unacknowledged packet;
        //
        // - If the socket is not sending and it hasn't sent a FIN yet, then it's waiting
        //   for incoming packets: send a fast resend request;
        //
        // - If the socket sent a FIN previously, resend it.
        debug!(
            "self.send_window: {:?}",
            self.send_window
                .iter()
                .map(Packet::seq_nr)
                .collect::<Vec<u16>>()
        );

        if self.send_window.is_empty() {
            // The socket is trying to close, all sent packets were acknowledged, and it has
            // already sent a FIN: resend it.
            if self.state == SocketState::FinSent {
                let mut packet = Packet::new();
                packet.set_connection_id(self.sender_connection_id);
                packet.set_seq_nr(self.seq_nr);
                packet.set_ack_nr(self.ack_nr);
                packet.set_timestamp(now_microseconds());
                packet.set_type(PacketType::Fin);

                // Send FIN
                self.socket.send_to(packet.as_ref(), self.connected_to).await?;
                debug!("resent FIN: {:?}", packet);
            } else if self.state != SocketState::New {
                // The socket is waiting for incoming packets but the remote peer is silent:
                // send a fast resend request.
                debug!("sending fast resend request");
                self.send_fast_resend_request().await;
            }
        } else {
            // The socket is sending data packets but there is no reply from the remote
            // peer: resend the first unacknowledged packet with the current timestamp.
            let packet = &mut self.send_window[0];
            packet.set_timestamp(now_microseconds());
            self.socket.send_to(packet.as_ref(), self.connected_to).await?;
            debug!("resent {:?}", packet);
        }

        Ok(())
    }
    pub async fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let read = self.flush_incoming_buffer(buf);

        if read > 0 {
            return Ok((read, self.connected_to));
        }

        // If the socket received a reset packet and all data has been flushed, then it can't
        // receive anything else
        if self.state == SocketState::ResetReceived {
            return Err(SocketError::ConnectionReset.into());
        }

        loop {
            // A closed socket with no pending data can only "read" 0 new bytes.
            if self.state == SocketState::Closed {
                return Ok((0, self.connected_to));
            }

            match self.recv(buf).await {
                Ok((0, _src)) => continue,
                Ok(x) => return Ok(x),
                Err(e) => return Err(e),
            }
        }
    }

    // pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
    //     match self.socket.send_to(buf, target).await {
    //         Ok(bytes_sent) => {
    //             log::info!("Successfully sent {} bytes to {}", bytes_sent, target);
    //             Ok(bytes_sent)
    //         }
    //         Err(e) => {
    //             log::error!("Failed to send data to {}: {}", target, e);
    //             Err(e)
    //         }
    //     }
    // }

    pub async fn send_to(&mut self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        if self.state == SocketState::Closed {
            return Err(SocketError::ConnectionClosed.into());
        }

        let total_length = buf.len();

        for chunk in buf.chunks(MSS as usize - HEADER_SIZE) {
            let mut packet = Packet::with_payload(chunk);
            packet.set_seq_nr(self.seq_nr);
            packet.set_ack_nr(self.ack_nr);
            packet.set_connection_id(self.sender_connection_id);

            self.unsent_queue.push_back(packet);

            // Intentionally wrap around sequence number
            self.seq_nr = self.seq_nr.wrapping_add(1);
        }

        // Send every packet in the queue
        self.send().await?;

        Ok(total_length)
    }
    async fn send(&mut self) -> io::Result<()> {
        while let Some(mut packet) = self.unsent_queue.pop_front() {
            self.send_packet(&mut packet).await?;
            self.curr_window += packet.len() as u32;
            self.send_window.push(packet);
        }
        Ok(())
    }

    // pub fn poll_send_to(&self, ctx: &mut Context, mut buf: &[u8], peer_addr: SocketAddr) -> Poll<io::Result<usize>> {
    //     info!("poll sending something {:?}", buf.len());
    //
    //     self.socket.poll_send_to(ctx, buf, peer_addr)
    // }
    pub fn poll_send_to(
        &mut self,
        ctx: &mut Context<'_>,
        buf: &[u8],
        peer_addr: SocketAddr,
    ) -> Poll<io::Result<usize>> {

        info!("poll_send_to invoked");
        if self.state == SocketState::Closed {
            return Poll::Ready(Err(SocketError::ConnectionClosed.into()));
        }

        let total_length = buf.len();

        // If there are no packets in the queue, split the buffer into packets
        if self.unsent_queue.is_empty() {
            for chunk in buf.chunks(MSS as usize - HEADER_SIZE) {
                let mut packet = Packet::with_payload(chunk);
                packet.set_seq_nr(self.seq_nr);
                packet.set_ack_nr(self.ack_nr);
                packet.set_connection_id(self.sender_connection_id);

                self.unsent_queue.push_back(packet);

                // Increment and wrap sequence number
                self.seq_nr = self.seq_nr.wrapping_add(1);
            }
        }

        // Attempt to send packets in the unsent queue
        while let Some(mut packet) = self.unsent_queue.front_mut() {
            let max_inflight = std::cmp::min(self.cwnd, self.remote_wnd_size);
            let max_inflight = std::cmp::max(MIN_CWND * MSS, max_inflight);

            // Check if we can send more packets within the congestion window
            if self.curr_window >= max_inflight {
                // Yield to allow acknowledgment processing or congestion to resolve
                return Poll::Pending;
            }

            // Set timestamps and prepare the packet
            packet.set_timestamp(now_microseconds());
            packet.set_timestamp_difference(self.their_delay);

            // Attempt to send the packet
            match self.socket.poll_send_to(ctx, packet.as_ref(), peer_addr) {
                Poll::Ready(Ok(_)) => {
                    // Successfully sent the packet, move it to the send window
                    info!("sent {:?}", packet);
                    let sent_packet = self.unsent_queue.pop_front().unwrap();
                    self.curr_window += sent_packet.len() as u32;
                    self.send_window.push(sent_packet);
                }
                Poll::Ready(Err(e)) => {
                    // Log and return any errors encountered
                    log::error!("Error sending packet: {:?}", e);
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    // If the socket is not ready, yield
                    return Poll::Pending;
                }
            }
        }

        // If all packets have been sent, return the total length of the buffer
        Poll::Ready(Ok(total_length))
    }


    pub fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {

        self.socket.poll_recv_from(cx, buf)
    }

    pub async fn recv_buf_from<B: BufMut>(&self, buf: &mut B) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_buf_from(buf).await
    }

}