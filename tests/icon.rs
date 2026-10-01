//! The Windows icon is committed, not generated at build time: check it holds every size.

#[test]
fn the_ico_holds_the_six_sizes() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/icon/clipscribe.ico");
    let bytes = std::fs::read(path).expect("docs/icon/clipscribe.ico");
    assert_eq!(&bytes[..4], [0, 0, 1, 0], "an ICO header");
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut sizes: Vec<u32> = (0..count)
        .map(|i| {
            let entry = &bytes[6 + 16 * i..6 + 16 * (i + 1)];
            let side = if entry[0] == 0 {
                256
            } else {
                u32::from(entry[0])
            };
            let len = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
            let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
            assert_eq!(
                &bytes[offset..offset + 4],
                b"\x89PNG",
                "PNG data for {side} px"
            );
            assert!(offset + len <= bytes.len());
            side
        })
        .collect();
    sizes.sort_unstable();
    assert_eq!(sizes, [16, 32, 48, 64, 128, 256]);
}
