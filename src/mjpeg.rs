//! Fix-ups for MJPEG frames coming out of UVC cameras.
//!
//! Most UVC cameras emit JPEG frames without a DHT (Define Huffman Table)
//! segment, relying on the decoder to supply the standard tables from
//! ITU-T T.81 Annex K.3. Not every decoder does, so we insert the standard
//! tables ourselves when they are missing.

const SOI: u8 = 0xD8;
const SOS: u8 = 0xDA;
const DHT: u8 = 0xC4;

// Tables from ITU-T T.81 Annex K.3 (identical to libjpeg's defaults).
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMA_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// Builds a single DHT segment (marker included) holding the four standard tables.
pub fn standard_dht_segment() -> Vec<u8> {
    let tables: [(u8, &[u8], &[u8]); 4] = [
        (0x00, &DC_LUMA_BITS, &DC_LUMA_VALS),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALS),
        (0x01, &DC_CHROMA_BITS, &DC_CHROMA_VALS),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS),
    ];
    let payload_len: usize = 2 + tables
        .iter()
        .map(|(_, b, v)| 1 + b.len() + v.len())
        .sum::<usize>();
    let mut seg = Vec::with_capacity(payload_len + 2);
    seg.extend_from_slice(&[0xFF, DHT]);
    seg.extend_from_slice(&(payload_len as u16).to_be_bytes());
    for (class_id, bits, vals) in tables {
        seg.push(class_id);
        seg.extend_from_slice(bits);
        seg.extend_from_slice(vals);
    }
    seg
}

/// Returns the frame unchanged when it already carries a DHT segment, otherwise
/// returns a copy with the standard tables inserted right before the first SOS.
pub fn ensure_dht(frame: &[u8]) -> Vec<u8> {
    if frame.len() < 4 || frame[0] != 0xFF || frame[1] != SOI {
        return frame.to_vec();
    }
    let mut i = 2;
    while i + 4 <= frame.len() {
        if frame[i] != 0xFF {
            // Not at a marker boundary: give up and hand the bytes back as-is.
            return frame.to_vec();
        }
        let marker = frame[i + 1];
        match marker {
            0xFF => {
                i += 1; // fill byte
                continue;
            }
            DHT => return frame.to_vec(),
            SOS => {
                let dht = standard_dht_segment();
                let mut out = Vec::with_capacity(frame.len() + dht.len());
                out.extend_from_slice(&frame[..i]);
                out.extend_from_slice(&dht);
                out.extend_from_slice(&frame[i..]);
                return out;
            }
            SOI | 0x01 | 0xD0..=0xD7 => {
                i += 2; // standalone markers carry no length
                continue;
            }
            _ => {
                let len = u16::from_be_bytes([frame[i + 2], frame[i + 3]]) as usize;
                i += 2 + len;
            }
        }
    }
    frame.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_sizes_are_consistent() {
        assert_eq!(
            DC_LUMA_BITS.iter().map(|&b| b as usize).sum::<usize>(),
            DC_LUMA_VALS.len()
        );
        assert_eq!(
            DC_CHROMA_BITS.iter().map(|&b| b as usize).sum::<usize>(),
            DC_CHROMA_VALS.len()
        );
        assert_eq!(
            AC_LUMA_BITS.iter().map(|&b| b as usize).sum::<usize>(),
            AC_LUMA_VALS.len()
        );
        assert_eq!(
            AC_CHROMA_BITS.iter().map(|&b| b as usize).sum::<usize>(),
            AC_CHROMA_VALS.len()
        );
        // The well-known MJPEG DHT blob is 420 bytes including the marker.
        assert_eq!(standard_dht_segment().len(), 420);
    }

    /// Strips every DHT segment from a baseline JPEG.
    fn strip_dht(jpeg: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, SOI];
        let mut i = 2;
        loop {
            let marker = jpeg[i + 1];
            if marker == SOS {
                out.extend_from_slice(&jpeg[i..]);
                return out;
            }
            let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            if marker != DHT {
                out.extend_from_slice(&jpeg[i..i + 2 + len]);
            }
            i += 2 + len;
        }
    }

    #[test]
    fn reinserted_tables_decode_to_the_same_pixels() {
        // Encode a small gradient with the default (standard) tables.
        let (w, h) = (24u16, 16u16);
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[(x * 10) as u8, (y * 15) as u8, 128]);
            }
        }
        let mut original = Vec::new();
        jpeg_encoder::Encoder::new(&mut original, 90)
            .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
            .unwrap();

        let stripped = strip_dht(&original);
        assert!(stripped.len() < original.len());
        assert_eq!(
            ensure_dht(&original),
            original,
            "frames with DHT must pass through untouched"
        );

        let fixed = ensure_dht(&stripped);
        assert!(fixed.len() > stripped.len());

        let decode = |bytes: &[u8]| {
            image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
                .expect("decodable jpeg")
                .to_rgb8()
                .into_raw()
        };
        assert_eq!(decode(&fixed), decode(&original));
    }
}
