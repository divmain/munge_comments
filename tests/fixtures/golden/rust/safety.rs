/// Reads the first byte.
pub fn first(bytes: &[u8]) -> u8 {
    assert!(!bytes.is_empty());
    // SAFETY: the slice is non-empty, checked just above.
    unsafe { *bytes.get_unchecked(0) }
}

// A plain comment mentioning safety is prose.
pub fn second(bytes: &[u8]) -> Option<u8> {
    bytes.get(1).copied()
}
