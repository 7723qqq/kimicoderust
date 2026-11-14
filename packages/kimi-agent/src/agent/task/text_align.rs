pub fn align_to_character_start(buffer: &[u8], offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut index = offset;
    while index < buffer.len() && (buffer[index] & 0b1100_0000) == 0b1000_0000 {
        index += 1;
    }
    index
}
