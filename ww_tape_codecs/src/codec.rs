//! [Codec]: the id of how a column's bytes are coded, one byte, the same in every file. Every codec id is defined
//! here, once; a reader refuses an id it doesn't know with its own typed error.

/// How a column is coded. Ids other than [Codec::RAW] are assigned by the codecs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Codec(pub u8);

impl Codec {
    /// Plain little-endian values (starts and ends as i64, flags as u32, lengths as u32, samples in their element
    /// type) or plain bytes (payload).
    pub const RAW: Codec = Codec(0);
    /// Event starts: an implicit period, or a linear frame of reference per group.
    pub const STARTS: Codec = Codec(1);
    /// Event durations: per group a minimum and the rest bit-packed.
    pub const DURATIONS: Codec = Codec(2);
    /// Event flags: per group their OR and the flags bit-packed.
    pub const FLAGS: Codec = Codec(3);
    /// Payload lengths, bit-packed per group.
    pub const LENGTHS: Codec = Codec(4);
    /// Samples: the element type big endian.
    pub const BIG_ENDIAN: Codec = Codec(16);
    /// Samples: 10 bits per value, the first value in the low bits.
    pub const PACKED10_LSB: Codec = Codec(17);
    /// Samples: 10 bits per value, the first value in the high bits.
    pub const PACKED10_MSB: Codec = Codec(18);
    /// Samples: 12 bits per value, the first value in the low bits (2 values in 3 bytes).
    pub const PACKED12_LSB: Codec = Codec(19);
    /// Samples: 12 bits per value, the first value in the high bits.
    pub const PACKED12_MSB: Codec = Codec(20);
    /// Samples: 24 bits per value, little endian.
    pub const PACKED24_LSB: Codec = Codec(21);
    /// Samples: 24 bits per value, big endian.
    pub const PACKED24_MSB: Codec = Codec(22);
    /// Floats, ALP: decimals as integers bit-packed per group, other floats' front bits through a dictionary.
    pub const ALP: Codec = Codec(26);
    /// Integers, differences bit-packed per group.
    pub const DELTA: Codec = Codec(32);
    /// Integers, differences of differences bit-packed per group.
    pub const DELTA2: Codec = Codec(33);
    /// Reserved: run-coded discrete lanes (`U8`, `U16`, `U32`, `Bool`); a reader refuses it until it is built.
    pub const RUNS: Codec = Codec(36);
    /// The bytes as one lz4 block after their decoded length ([lz4](crate::lz4)).
    pub const LZ4: Codec = Codec(48);
    /// Reserved: a payloads column holding one shrink_wrap `Series<T>` of the items.
    pub const SERIES: Codec = Codec(49);
    /// Bit 7 of the codec byte: the column is outer-coded.
    pub const OUTER: u8 = 0x80;

    /// Whether the column is outer-coded: bit 7 set.
    pub const fn is_outer(self) -> bool {
        self.0 & Self::OUTER != 0
    }

    /// The codec of the column's bytes, or of the decoded bytes of an outer-coded column: bits 0..6.
    pub const fn inner(self) -> Codec {
        Codec(self.0 & !Self::OUTER)
    }
}

#[cfg(test)]
mod tests {
    use super::Codec;

    /// Every id is one of its own and leaves bit 7 free for the outer coding.
    #[test]
    fn codec_ids_are_distinct_and_inner() {
        let ids = [
            Codec::RAW,
            Codec::STARTS,
            Codec::DURATIONS,
            Codec::FLAGS,
            Codec::LENGTHS,
            Codec::BIG_ENDIAN,
            Codec::PACKED10_LSB,
            Codec::PACKED10_MSB,
            Codec::PACKED12_LSB,
            Codec::PACKED12_MSB,
            Codec::PACKED24_LSB,
            Codec::PACKED24_MSB,
            Codec::ALP,
            Codec::DELTA,
            Codec::DELTA2,
            Codec::RUNS,
            Codec::LZ4,
            Codec::SERIES,
        ];
        for (i, a) in ids.iter().enumerate() {
            assert!(!a.is_outer(), "{a:?}");
            assert_eq!(a.inner(), *a);
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
