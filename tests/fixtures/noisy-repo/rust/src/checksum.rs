//! Frame checksums shared with the firmware in `native/frame.c`.

/// The CRC-16/CCITT-FALSE polynomial.
const POLY: u16 = 0x1021;

/// Computes the checksum of `bytes`.
pub fn crc16(bytes: &[u8]) -> u16 {
    // Start from 0xFFFF, not 0: the firmware seeds its CRC register that
    // way, and a different seed yields checksums that never match.
    let mut crc: u16 = 0xFFFF;
    // Loop over the bytes.
    for &byte in bytes {
        // XOR the byte into the high bits.
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            // shift and xor w/ poly if top bit set
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ POLY } else { crc << 1 };
        }
    }
    // Return the CRC.
    crc
}

/// Reads the big-endian checksum stored in the last two bytes of `frame`.
pub fn stored_checksum(frame: &[u8]) -> Option<u16> {
    if frame.len() < 2 {
        return None;
    }
    let tail = frame.len() - 2;
    // SAFETY: `tail + 1 < frame.len()` because `frame.len() >= 2`.
    let (hi, lo) = unsafe { (*frame.get_unchecked(tail), *frame.get_unchecked(tail + 1)) };
    Some(u16::from_be_bytes([hi, lo]))
}
