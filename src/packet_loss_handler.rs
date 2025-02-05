use crate::packet::Packet;

/// Trait defining the behaviour for handling packet loss in uTP
pub trait PacketLossHandler {
    /// Handles packet loss detected through the duplicate acknowledgements or timeouts
    ///
    /// # Arguments
    /// * `lost_packet_nr` - The sequence number of the lost packet
    async fn handle_packet_loss(&mut self, lost_packet: Packet);

    /// Resends a lost packet identified by its sequence number.
    ///
    /// # Arguments
    /// * `lost_packet_nr` - The sequence number of the packet to resend
    async fn resend_lost_packet(&mut self, lost_packet: Packet);

    /// Sends a fast resend request by sending three State packets in quick succession
    async fn send_fast_resend_request(&self);

    /// Advances the send windows by removing acknowledged packets
    async fn advance_send_window(&mut self);
}
