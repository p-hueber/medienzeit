//! SNTP packet codec (RFC 4330).
//!
//! Pure: builds the 48 bytes to send and reads the 48 that come back. The socket is
//! the firmware's problem. Small enough that a dependency would cost more than it
//! saves, and this way the epoch arithmetic — the only part that is actually easy to
//! get wrong — is tested on the host.

/// SNTP messages are always exactly 48 bytes.
pub const PACKET_LEN: usize = 48;

/// Seconds between the NTP epoch (1900-01-01) and the unix epoch (1970-01-01).
///
/// 70 years including 17 leap days.
pub const NTP_TO_UNIX: u64 = 2_208_988_800;

/// Standard NTP port.
pub const PORT: u16 = 123;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not 48 bytes.
    BadLength,
    /// The server set Leap Indicator = 3, meaning its own clock is not synchronised.
    NotSynchronised,
    /// Mode was not 4 (server); we asked as a client, so anything else is bogus.
    NotAServerReply,
    /// Transmit timestamp was zero, or older than the unix epoch.
    NoTimestamp,
}

/// A client request: LI = 0, VN = 4, Mode = 3, everything else zero.
pub fn request() -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = (0 << 6) | (4 << 3) | 3;
    p
}

/// Extract the server's transmit timestamp as a unix timestamp in seconds.
///
/// The fractional part is deliberately discarded — the ledger ticks at 1 Hz and the
/// day boundary is an hour-scale concept, so sub-second precision buys nothing.
pub fn unix_seconds(packet: &[u8]) -> Result<i64, Error> {
    if packet.len() != PACKET_LEN {
        return Err(Error::BadLength);
    }

    let li = packet[0] >> 6;
    if li == 3 {
        return Err(Error::NotSynchronised);
    }
    if packet[0] & 0b111 != 4 {
        return Err(Error::NotAServerReply);
    }

    // Transmit timestamp: seconds at bytes 40..44, fraction at 44..48.
    let secs = u32::from_be_bytes([packet[40], packet[41], packet[42], packet[43]]) as u64;
    if secs == 0 || secs < NTP_TO_UNIX {
        return Err(Error::NoTimestamp);
    }
    Ok((secs - NTP_TO_UNIX) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(ntp_secs: u32, li: u8, mode: u8) -> [u8; PACKET_LEN] {
        let mut p = [0u8; PACKET_LEN];
        p[0] = (li << 6) | (4 << 3) | mode;
        p[40..44].copy_from_slice(&ntp_secs.to_be_bytes());
        p
    }

    #[test]
    fn request_is_a_client_mode_v4_packet() {
        let r = request();
        assert_eq!(r.len(), PACKET_LEN);
        assert_eq!(r[0] >> 6, 0, "leap indicator");
        assert_eq!((r[0] >> 3) & 0b111, 4, "version");
        assert_eq!(r[0] & 0b111, 3, "mode: client");
        assert!(r[1..].iter().all(|b| *b == 0));
    }

    #[test]
    fn converts_the_ntp_epoch_to_unix() {
        // NTP seconds for 1970-01-01T00:00:00Z is exactly the offset.
        assert_eq!(unix_seconds(&reply(NTP_TO_UNIX as u32 + 1, 0, 4)), Ok(1));
        // 2026-08-03T16:00:00Z = unix 1785945600.
        let ntp = (1_785_945_600u64 + NTP_TO_UNIX) as u32;
        assert_eq!(unix_seconds(&reply(ntp, 0, 4)), Ok(1_785_945_600));
    }

    #[test]
    fn rejects_an_unsynchronised_server() {
        // LI = 3 means the server's own clock is not set. Trusting it would hand the
        // ledger a wrong day boundary, which is worse than having no time at all.
        assert_eq!(unix_seconds(&reply(0xE0000000, 3, 4)), Err(Error::NotSynchronised));
    }

    #[test]
    fn rejects_a_non_server_reply() {
        assert_eq!(unix_seconds(&reply(0xE0000000, 0, 3)), Err(Error::NotAServerReply));
    }

    #[test]
    fn rejects_missing_or_pre_epoch_timestamps() {
        assert_eq!(unix_seconds(&reply(0, 0, 4)), Err(Error::NoTimestamp));
        assert_eq!(unix_seconds(&reply(1_000, 0, 4)), Err(Error::NoTimestamp));
    }

    #[test]
    fn rejects_a_short_packet() {
        assert_eq!(unix_seconds(&[0u8; 20]), Err(Error::BadLength));
        assert_eq!(unix_seconds(&[]), Err(Error::BadLength));
    }

    #[test]
    fn survives_the_2036_rollover_boundary_as_far_as_it_can() {
        // NTP era 0 ends in 2036; u32 seconds wrap. We do not handle eras, and this
        // pins that limitation rather than pretending otherwise: a timestamp past the
        // wrap decodes as a 1900s date and is rejected as pre-epoch.
        assert_eq!(unix_seconds(&reply(1_000_000, 0, 4)), Err(Error::NoTimestamp));
    }
}
