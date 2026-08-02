//! Small byte<->integer helpers shared by the MMIO exit handlers (LAPIC/IOAPIC
//! register reads/writes hand back/take raw little-endian byte slices).

pub(crate) fn read_u32_le(data: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    for (i, dst) in b.iter_mut().enumerate() {
        if i < data.len() {
            *dst = data[i];
        }
    }
    u32::from_le_bytes(b)
}

pub(crate) fn write_u32_le(data: &mut [u8], val: u32) {
    let b = val.to_le_bytes();
    for (i, dst) in data.iter_mut().enumerate() {
        if i < 4 {
            *dst = b[i];
        }
    }
}
