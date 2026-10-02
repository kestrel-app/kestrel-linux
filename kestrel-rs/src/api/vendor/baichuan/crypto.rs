//! The small ciphers the Baichuan P2P protocol is built from.
//!
//! Three primitives, each self-contained and each sourced from the official
//! Reolink app's `libBCSDKWrapper.so` (version 4.63.0.3, arm64) rather than from
//! any third-party implementation:
//!
//! - [`xml_crypt`] — the stream cipher that wraps the UDP *discovery* XML. A
//!   repeating 32-byte keystream, the eight key words offset by the packet's
//!   transmission id. All eight constants in [`XML_KEY`] were read straight out
//!   of the shipped binary (each appears there exactly once).
//! - [`bc_crc`] — the CRC the discovery header carries over its encrypted
//!   payload. It is CRC-32/ISO-HDLC with the final xor undone, which is what the
//!   device checks against.
//! - [`md5`] — a from-scratch MD5, which the device login digest uses (the reply's
//!   `<type>` is literally `md5`). [`sha256`] is also here for completeness. Both are
//!   in-tree so the protocol pulls in no crypto dependency, and both are pinned to
//!   standard vectors below.
//!
//! The control-channel body cipher is [`bc_encrypt`] ("BCEncrypt"), an xor keyed by
//! [`BC_ENCRYPT_KEY`] — *measured* by decrypting a real device's login reply, which
//! corrected an earlier wrong guess.
//!
//! What is *not* here is the modern AES-128-CFB body cipher (key derived from the
//! login nonce, IV `0123456789abcdef`). It is only needed after login, has not been
//! exercised against a device, and is deliberately absent rather than guessed — see
//! `docs/untested.md`.

/// The eight 32-bit words the discovery cipher is keyed from.
///
/// Read directly from `libBCSDKWrapper.so`; each constant occurs once in the
/// shipped binary. The keystream is these words, each advanced by the packet
/// transmission id, emitted little-endian and repeated.
pub const XML_KEY: [u32; 8] = [
    0x1f2d_3c4b,
    0x5a6c_7f8d,
    0x3817_2e4b,
    0x8271_635a,
    0x863f_1a2b,
    0xa5c6_f7d8,
    0x8371_e1b4,
    0x17f2_d3a5,
];

/// Encrypt or decrypt a discovery payload. The cipher is its own inverse, so one
/// function serves both directions.
///
/// `offset` is the packet's transmission id (`tid`); it shifts every key word,
/// which is what makes two packets with the same body encrypt differently.
pub fn xml_crypt(offset: u32, buf: &[u8]) -> Vec<u8> {
    let keystream: Vec<u8> = XML_KEY
        .iter()
        .flat_map(|word| word.wrapping_add(offset).to_le_bytes())
        .collect();
    buf.iter()
        .zip(keystream.iter().cycle())
        .map(|(byte, key)| byte ^ key)
        .collect()
}

/// The CRC the discovery header stores over its encrypted payload.
///
/// It is the reflected CRC-32 machine (polynomial `0xedb8_8320`) run with an
/// initial value of **zero** and **no** final output complement — not the
/// `0xffff_ffff` init that ordinary CRC-32 uses. This is *measured*, not assumed:
/// it reproduces the checksum in real discovery packets exactly (see the test
/// below and the live registrar round-trip that confirmed it), and the usual
/// init made the registrars silently drop every packet.
pub fn bc_crc(payload: &[u8]) -> u32 {
    let mut crc: u32 = 0x0000_0000;
    for &byte in payload {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// The eight-byte key the control-channel "BCEncrypt" cipher uses.
///
/// This is *measured*, not guessed: decrypting a real device's login reply with
/// it (at offset 0) yields valid `<Encryption><nonce>…` XML. The earlier guess —
/// xoring against the shipped `!shenzhenbaichuan.com@…!` string — was wrong; that
/// string is key material for something else.
pub const BC_ENCRYPT_KEY: [u8; 8] = [0x1f, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0xff];

/// The legacy "BCEncrypt" control-channel body cipher, which is its own inverse.
///
/// Each byte is xored with the key byte at `(offset + index) mod 8` and then with
/// the low byte of `offset`. `offset` is the message's encryption offset; the
/// device's first login reply uses offset 0. Newer firmware can negotiate AES-CFB
/// instead (keyed from the login nonce), which is not implemented yet.
pub fn bc_encrypt(offset: u32, buf: &[u8]) -> Vec<u8> {
    let off_byte = (offset & 0xff) as u8;
    buf.iter()
        .enumerate()
        .map(|(i, byte)| {
            let k = BC_ENCRYPT_KEY[(offset as usize + i) % BC_ENCRYPT_KEY.len()];
            byte ^ k ^ off_byte
        })
        .collect()
}

// ------------------------------------------------------------------- SHA-256

/// SHA-256 of a message, from scratch.
///
/// Present so the protocol needs no crypto crate; the modern login digest and
/// the proof-of-work solver both run through it. Correctness is pinned by the
/// NIST known-answer tests below.
pub fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1,
        0x923f_82a4, 0xab1c_5ed5, 0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3,
        0x72be_5d74, 0x80de_b1fe, 0x9bdc_06a7, 0xc19b_f174, 0xe49b_69c1, 0xefbe_4786,
        0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f, 0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da,
        0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7, 0xc6e0_0bf3, 0xd5a7_9147,
        0x06ca_6351, 0x1429_2967, 0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc, 0x5338_0d13,
        0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85, 0xa2bf_e8a1, 0xa81a_664b,
        0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070,
        0x19a4_c116, 0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a,
        0x5b9c_ca4f, 0x682e_6ff3, 0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208,
        0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7, 0xc671_78f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c,
        0x1f83_d9ab, 0x5be0_cd19,
    ];

    // Pad: a 0x80 byte, zeros, then the 64-bit bit length.
    let bit_len = (message.len() as u64) * 8;
    let mut data = message.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (hi, vi) in h.iter_mut().zip(v.iter()) {
            *hi = hi.wrapping_add(*vi);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// SHA-256 rendered as lowercase hex.
pub fn sha256_hex(message: &[u8]) -> String {
    hex(&sha256(message))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

// --------------------------------------------------------------------- MD5

/// MD5 of a message, from scratch.
///
/// The device login salts the password with a nonce and hashes it with MD5 (the
/// reply's `<type>` is literally `md5`), so the protocol needs it; kept in-tree to
/// avoid a crypto dependency. MD5 is cryptographically broken and is used here only
/// because the device protocol specifies it — never for anything new.
pub fn md5(message: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613,
        0xfd46_9501, 0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193,
        0xa679_438e, 0x49b4_0821, 0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d,
        0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, 0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
        0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, 0xfffa_3942, 0x8771_f681, 0x6d9d_6122,
        0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, 0x289b_7ec6, 0xeaa1_27fa,
        0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, 0xf429_2244,
        0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
        0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb,
        0xeb86_d391,
    ];

    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut data = message.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in data.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

/// MD5 as lowercase hex.
pub fn md5_hex(message: &[u8]) -> String {
    hex(&md5(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The discovery cipher is symmetric: encrypting twice returns the input,
    /// at any offset.
    #[test]
    fn xml_crypt_is_its_own_inverse() {
        let plain = b"<P2P><C2M_Q><uid>ABC</uid></C2M_Q></P2P>";
        for offset in [0u32, 1, 87, 0x1234, u32::MAX] {
            let once = xml_crypt(offset, plain);
            let twice = xml_crypt(offset, &once);
            assert_eq!(twice, plain, "offset {offset}");
            if offset != 0 || true {
                assert_ne!(once, plain, "offset {offset} must actually scramble");
            }
        }
    }

    /// The offset changes the keystream, so the same body under two tids must
    /// differ — this is the property that makes the tid load-bearing.
    #[test]
    fn xml_crypt_offset_changes_the_keystream() {
        let plain = [0u8; 48];
        assert_ne!(xml_crypt(0, &plain), xml_crypt(1, &plain));
    }

    /// First 32 keystream bytes at offset 0 are just the key words, little-endian.
    /// A fixed anchor so a careless edit to the key or the byte order is caught.
    #[test]
    fn xml_crypt_keystream_anchor() {
        let out = xml_crypt(0, &[0u8; 32]);
        let mut expected = Vec::new();
        for w in XML_KEY {
            expected.extend_from_slice(&w.to_le_bytes());
        }
        assert_eq!(out, expected);
    }

    /// Anchored to a real captured discovery packet: a `C2D_DISC` with tid 96 that
    /// carried the checksum 0x42824554 over its encrypted payload. Encrypting the
    /// packet's plaintext and CRC'ing the result must reproduce that exact value —
    /// this is what the registrars actually check, and it is how the init=0 (not
    /// 0xffffffff) CRC was pinned after the wrong init made every packet get
    /// silently dropped.
    #[test]
    fn bc_crc_matches_a_real_captured_packet() {
        let plaintext = "<P2P>\n<C2D_DISC>\n<cid>82000</cid>\n<did>80</did>\n</C2D_DISC>\n</P2P>\n";
        let encrypted = xml_crypt(96, plaintext.as_bytes());
        assert_eq!(bc_crc(&encrypted), 0x4282_4554);
    }

    /// A plain known-answer so the machine itself is pinned: init=0, no final xor.
    #[test]
    fn bc_crc_is_init_zero_no_final_xor() {
        assert_eq!(bc_crc(b"123456789"), 0x2dfd_2d88);
        assert_eq!(bc_crc(b""), 0);
    }

    #[test]
    fn bc_encrypt_round_trips() {
        let plain = b"<body><cmd>login</cmd></body>";
        for offset in [0u32, 7, 30, 255, 1000] {
            assert_eq!(bc_encrypt(offset, &bc_encrypt(offset, plain)), plain.to_vec());
        }
    }

    /// Measured anchor: a real device's login reply decrypts at offset 0, and its
    /// plaintext begins with the XML prolog. Encrypting that prolog at offset 0 is
    /// what must have produced the bytes on the wire, so this pins key + formula to
    /// the behaviour actually observed.
    #[test]
    fn bc_encrypt_offset_zero_matches_the_observed_login_reply() {
        let prolog = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>";
        // The first 39 body bytes of a real device's login (nonce) reply.
        let cipher: [u8; 39] = [
            0x23, 0x12, 0x44, 0x26, 0x36, 0x49, 0x0e, 0x9a, 0x6d, 0x5e, 0x55, 0x24, 0x34, 0x54,
            0x5a, 0xce, 0x31, 0x1d, 0x1e, 0x6b, 0x3f, 0x07, 0x1b, 0x90, 0x7b, 0x44, 0x52, 0x2c,
            0x67, 0x4b, 0x2d, 0xab, 0x59, 0x00, 0x04, 0x69, 0x7a, 0x56, 0x46,
        ];
        assert_eq!(bc_encrypt(0, &cipher), prolog.to_vec());
    }

    #[test]
    fn md5_known_answers() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    /// NIST known-answer vectors. If these pass, the implementation is SHA-256.
    #[test]
    fn sha256_known_answers() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// A message long enough to span two padded blocks, to exercise the block
    /// loop rather than only the single-block path.
    #[test]
    fn sha256_spans_multiple_blocks() {
        let input = vec![b'a'; 1000];
        assert_eq!(
            sha256_hex(&input),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }
}
