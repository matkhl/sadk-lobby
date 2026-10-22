//! Frame checksum: reflected CRC-32 (polynomial 0xEDB88320) with initial value
//! 0 and no final inversion. Plain zlib CRC-32 does not match the game.

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

pub fn checksum(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |crc, &b| TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_captured_handshake() {
        // Client connect payload from a recorded SADK login (S2-modders dump).
        let payload = crate::frame::tests::hex(
            "effbbadaeeffffef757365720000000064020000040000000000000064020000\
             040000000000000070617373776f720001000000",
        );
        assert_eq!(checksum(&payload), 0x3b48_3a97);
    }
}
