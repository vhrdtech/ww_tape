use super::*;

#[test]
fn a_seal_reads_as_written() {
    let seal = Seal {
        first: ItemNum::new(7),
        len: 1 << 33,
        hash: [0xa5; 32],
    };
    let bytes = seal.to_bytes();
    assert_eq!(&bytes[..8], &7u64.to_le_bytes());
    assert_eq!(&bytes[8..16], &(1u64 << 33).to_le_bytes());
    assert_eq!(Seal::read(&bytes), Some(seal));
    assert_eq!(Seal::read(&bytes[..47]), None);
}
