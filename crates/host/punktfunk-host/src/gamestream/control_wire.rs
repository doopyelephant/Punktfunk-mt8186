//! The codec half of the GameStream control stream: the AES-GCM scheme sweep that opens a
//! client's packets, the host-side seal, and the parsers for the loss-recovery messages.
//! `std` and `aes-gcm` only, with no host state, so a fuzz target includes this file by path;
//! every byte here arrives from a LAN peer under the session key alone. What a message does
//! to the session is [`super::control`]'s.

/// Lost-frame range from invalidate-reference-frames (0x0301): two LE `i64`
/// (firstFrame, lastFrame) after `[u16 type][u16 length]`, matching
/// `IDX_INVALIDATE_REF_FRAMES`. `None` if short or nonsensical → caller does a full IDR.
pub(super) fn decode_rfi_range(pt: &[u8]) -> Option<(i64, i64)> {
    if pt.len() < 20 {
        return None;
    }
    let first = i64::from_le_bytes(pt[4..12].try_into().ok()?);
    let last = i64::from_le_bytes(pt[12..20].try_into().ok()?);
    (first >= 0 && last >= first).then_some((first, last))
}

/// Data shards parity could not restore, from a per-frame FEC status (0x5502,
/// `SS_FRAME_FEC_STATUS`: 21 packed BE bytes after `[u16 type][u16 length]`). Loss FEC
/// recovered counts 0: it is FEC doing its job, not a reason to cut the bitrate.
pub(super) fn decode_fec_status_loss(pt: &[u8]) -> Option<u64> {
    let body = pt.get(4..25)?;
    let be16 = |at: usize| u64::from(u16::from_be_bytes([body[at], body[at + 1]]));
    let (total_data, received_data, received_parity) = (be16(10), be16(14), be16(16));
    Some(total_data.saturating_sub(received_data + received_parity))
}

/// How a control packet's nonce is built. Moonlight picks one from the negotiated flags.
#[derive(Clone, Copy, Debug)]
pub(super) enum NonceKind {
    /// `SS_ENC_CONTROL_V2`: 12-byte nonce, `seq` in [0..4], marker bytes at [10..12].
    V2 { seq_be: bool, marker: [u8; 2] },
    /// Legacy: 16-byte nonce, only `iv[0] = seq & 0xff` (the rest zero).
    LegacyLowByte,
    /// Legacy variant: 16-byte nonce, full `seq` in [0..4] (the rest zero).
    Legacy16Seq { seq_be: bool },
}

impl NonceKind {
    pub(super) fn nonce(&self, seq: u32) -> Vec<u8> {
        let seq_bytes = |be: bool| {
            if be {
                seq.to_be_bytes()
            } else {
                seq.to_le_bytes()
            }
        };
        match *self {
            NonceKind::V2 { seq_be, marker } => {
                let mut iv = vec![0u8; 12];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv[10] = marker[0];
                iv[11] = marker[1];
                iv
            }
            NonceKind::LegacyLowByte => {
                let mut iv = vec![0u8; 16];
                iv[0] = (seq & 0xff) as u8;
                iv
            }
            NonceKind::Legacy16Seq { seq_be } => {
                let mut iv = vec![0u8; 16];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv
            }
        }
    }
}

/// GCM scheme that opened a control packet. Locked once per connection: AES-GCM gives
/// no partial credit, so an authenticating combination is proof.
#[derive(Clone, Copy, Debug)]
pub(super) struct Scheme {
    /// `gcm_key` is byte-reversed before use (defensive; Sunshine's net effect is forward).
    pub(super) key_rev: bool,
    pub(super) nonce: NonceKind,
    /// GCM tag sits before the ciphertext (vs after).
    pub(super) tag_first: bool,
    pub(super) aad: Aad,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Aad {
    None,
    /// The 4-byte cleartext header prefix (encType + length), `d[0..4]`.
    Header4,
}

impl Scheme {
    pub(super) fn key(&self, base: &[u8; 16]) -> [u8; 16] {
        let mut k = *base;
        if self.key_rev {
            k.reverse();
        }
        k
    }
}

/// Open encrypted control packet `d` (8-byte cleartext header + `[tag?][ciphertext]`).
/// Fast path: only `detected`. Otherwise sweep nonce × key order × tag position × AAD
/// and return the combination whose GCM tag authenticates.
pub(super) fn decrypt_control(
    key: &[u8; 16],
    d: &[u8],
    detected: &Option<Scheme>,
) -> Option<(Scheme, Vec<u8>)> {
    let seq = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    let payload = &d[8..];
    if payload.len() < 16 {
        return None;
    }

    let attempt = |s: Scheme| -> Option<Vec<u8>> {
        // aes-gcm wants `ciphertext || tag`; reassemble from whichever wire order this is.
        let (ct, tag) = if s.tag_first {
            (&payload[16..], &payload[..16])
        } else {
            (
                &payload[..payload.len() - 16],
                &payload[payload.len() - 16..],
            )
        };
        let mut ct_tag = Vec::with_capacity(ct.len() + 16);
        ct_tag.extend_from_slice(ct);
        ct_tag.extend_from_slice(tag);
        let aad: &[u8] = match s.aad {
            Aad::None => &[],
            Aad::Header4 => &d[0..4],
        };
        gcm_open(&s.key(key), &s.nonce.nonce(seq), &ct_tag, aad)
    };

    if let Some(s) = *detected {
        return attempt(s).map(|pt| (s, pt));
    }

    // Candidate nonce constructions, most-likely first.
    const MARKERS: [[u8; 2]; 3] = [*b"CC", *b"HC", *b"CH"];
    let mut kinds: Vec<NonceKind> = vec![NonceKind::LegacyLowByte];
    for seq_be in [false, true] {
        for marker in MARKERS {
            kinds.push(NonceKind::V2 { seq_be, marker });
        }
        kinds.push(NonceKind::Legacy16Seq { seq_be });
    }

    for &nonce in &kinds {
        for key_rev in [false, true] {
            for tag_first in [true, false] {
                for aad in [Aad::None, Aad::Header4] {
                    let s = Scheme {
                        key_rev,
                        nonce,
                        tag_first,
                        aad,
                    };
                    if let Some(pt) = attempt(s) {
                        return Some((s, pt));
                    }
                }
            }
        }
    }
    None
}

/// Seal a host→client control message on the client's `detected` scheme, direction
/// flipped: V2 markers `H?` instead of `C?`; legacy keeps its construction with our
/// independent `seq`. Wire: `[0x0001][length][seq][tag|ct per scheme.tag_first]`.
pub(super) fn encrypt_control(key: &[u8; 16], scheme: &Scheme, seq: u32, pt: &[u8]) -> Vec<u8> {
    let nonce_kind = match scheme.nonce {
        NonceKind::V2 { seq_be, marker } => NonceKind::V2 {
            seq_be,
            marker: [b'H', marker[1]],
        },
        other => other,
    };
    let length = (4 + 16 + pt.len()) as u16;
    let mut wire = Vec::with_capacity(8 + 16 + pt.len());
    wire.extend_from_slice(&0x0001u16.to_le_bytes());
    wire.extend_from_slice(&length.to_le_bytes());
    wire.extend_from_slice(&seq.to_le_bytes());
    let aad: Vec<u8> = match scheme.aad {
        Aad::None => Vec::new(),
        Aad::Header4 => wire[0..4].to_vec(),
    };
    let ct_tag = gcm_seal(&scheme.key(key), &nonce_kind.nonce(seq), pt, &aad);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    if scheme.tag_first {
        wire.extend_from_slice(tag);
        wire.extend_from_slice(ct);
    } else {
        wire.extend_from_slice(ct);
        wire.extend_from_slice(tag);
    }
    wire
}

/// AES-128-GCM seal (companion to [`gcm_open`]); returns `ciphertext || tag`. Shared
/// with the RTSP plane under the same session key.
pub(super) fn gcm_seal(key: &[u8; 16], nonce: &[u8], pt: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: pt, aad };
    // Each arm's `try_into` is guarded by the length it matched on.
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("12-byte nonce"), p)
            .expect("GCM seal"),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("16-byte nonce"), p)
            .expect("GCM seal"),
        _ => unreachable!("nonce length"),
    }
}

/// AES-128-GCM open, 12- or 16-byte nonce, explicit AAD. Plaintext iff the tag
/// authenticates. `ct_tag` is `ciphertext || tag` (aes-gcm's order).
pub(super) fn gcm_open(key: &[u8; 16], nonce: &[u8], ct_tag: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: ct_tag, aad };
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        _ => None,
    }
}
