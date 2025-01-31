use crate::time::{Delay, Timestamp};

/// Trait defining the behavior for congestion control in uTP (Micro Transport Protocol).
///
/// This trait encapsulates the core logic for managing congestion control in uTP, which is
/// essential for ensuring efficient and fair usage of network bandwidth while avoiding congestion.
/// The implementation is based on the [LEDBAT (Low Extra Delay Background Transport)](https://tools.ietf.org/html/rfc6817)
/// algorithm, which aims to utilize available bandwidth without causing significant delays or
/// interfering with other traffic.
pub trait CongestionControl {
    /// Updates the base delay, which is the minimum observed delay between peers.
    ///
    /// The base delay is used to estimate the queuing delay by comparing it with the current delay.
    /// It helps distinguish between network propagation delay and queuing delay, which is critical
    /// for congestion control.
    ///
    /// # Arguments
    /// * `our_delay` - The current delay measurement between the sender and receiver.
    /// * `now` - The timestamp of the current delay measurement.
    async fn update_base_delay(&mut self, our_delay: Delay, now: Timestamp);

    /// Updates the current delay, which tracks recent delays between peers.
    ///
    /// The current delay is used to calculate the filtered current delay, which reflects the
    /// recent queuing behavior of the network. This function ensures that only delays within the
    /// last RTT are considered.
    ///
    /// # Arguments
    /// * `our_delay` - The current delay measurement between the sender and receiver.
    /// * `now` - The timestamp of the current delay measurement.
    async fn update_current_delay(&mut self, our_delay: Delay, now: Timestamp);

    /// Updates the congestion timeout based on the round-trip time (RTT).
    ///
    /// The congestion timeout determines how long the socket waits for an acknowledgment before
    /// retransmitting a packet. It is dynamically adjusted based on the measured RTT and its variance.
    ///
    /// Formula:
    /// \[
    /// \text{congestion\_timeout} = \max(\text{RTT} + 4 \times \text{RTT\_variance}, \text{MIN\_CONGESTION\_TIMEOUT})
    /// \]
    ///
    /// # Arguments
    /// * `rtt` - The measured round-trip time in milliseconds.
    async fn update_congestion_timeout(&mut self, rtt: i32);

    /// Updates the congestion window size based on the off-target value and newly acknowledged bytes.
    ///
    /// This is the core of the LEDBAT congestion control algorithm. The congestion window (`cwnd`)
    /// is adjusted to increase or decrease the sending rate based on the difference between the
    /// current queuing delay and the target delay.
    ///
    /// Formula:
    /// \[
    /// \text{off\_target} = \frac{\text{TARGET} - \text{queuing\_delay}}{\text{TARGET}}
    /// \]
    /// \[
    /// \text{cwnd\_increase} = \text{GAIN} \times \text{off\_target} \times \text{bytes\_newly\_acked}
    /// \]
    /// \[
    /// \text{cwnd} = \text{cwnd} + \text{cwnd\_increase}
    /// \]
    ///
    /// The window is clamped to ensure it stays within acceptable bounds:
    /// - Minimum: `MIN_CWND * MSS`
    /// - Maximum: `flightsize + ALLOWED_INCREASE * MSS`
    ///
    /// # Arguments
    /// * `off_target` - A normalized value representing the difference between the current queuing
    ///   delay and the target delay. It ranges between -1.0 and 1.0:
    ///     - A positive value indicates the current delay is below the target, allowing the window to grow.
    ///     - A negative value indicates the current delay exceeds the target, requiring the window to shrink.
    /// * `bytes_newly_acked` - The number of bytes acknowledged by the latest inbound acknowledgment.
    ///   This includes both explicitly and implicitly acknowledged packets.
    async fn update_congestion_window(&mut self, off_target: f64, bytes_newly_acked: u32);

    /// Calculates the minimum base delay observed in the current window.
    ///
    /// The base delay is the smallest delay observed over a rolling window of measurements. It is
    /// used to estimate the network's propagation delay without queuing effects.
    ///
    /// # Returns
    /// The smallest base delay in the current window.
    async fn min_base_delay(&self) -> Delay;

    /// Calculates the filtered current delay in the current window.
    ///
    /// The current delay is calculated using an exponential weighted moving average (EWMA) filter
    /// with a smoothing factor of 0.333. This provides a smoothed representation of recent delays,
    /// which helps in estimating the queuing delay.
    ///
    /// Formula:
    /// \[
    /// \text{filtered\_current\_delay} = \alpha \times \text{current\_delay} + (1 - \alpha) \times \text{previous\_filtered\_delay}
    /// \]
    /// Where:
    /// - \(\alpha = 0.333\)
    ///
    /// # Returns
    /// The filtered current delay.
    fn filtered_current_delay(&self) -> Delay;

    /// Calculates the queuing delay.
    ///
    /// The queuing delay is the difference between the filtered current delay and the minimum base
    /// delay. It represents the additional delay caused by queuing in the network, which is used
    /// to adjust the congestion window.
    ///
    /// Formula:
    /// \[
    /// \text{queuing\_delay} = \text{filtered\_current\_delay} - \text{min\_base\_delay}
    /// \]
    ///
    /// # Returns
    /// The estimated queuing delay.
    fn queuing_delay(&self) -> Delay;
}