use crate::constants::{CHAFF_MAX, FRAG_HEADER_LEN};
use flate2::Compression;
use morsh_proto::transport::Instruction as TransportInstruction;
use prost::Message;
use std::io::{Read, Write};

/// A single fragment of a larger instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub id: u64,
    pub final_flag: bool,
    pub fragment_num: u16,
    pub payload: Vec<u8>,
}

impl Fragment {
    /// Serialize this fragment to bytes for transmission.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FRAG_HEADER_LEN + self.payload.len());
        buf.extend_from_slice(&self.id.to_be_bytes());
        // Stock mosh convention: bit 15 = final_flag (1 = final/only fragment, 0 = more fragments follow)
        let combined = (self.final_flag as u16) << 15 | (self.fragment_num & 0x7FFF);
        buf.extend_from_slice(&combined.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Parse a fragment from raw bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < FRAG_HEADER_LEN {
            return Err("Fragment too short".into());
        }
        let id = u64::from_be_bytes(data[0..8].try_into().unwrap());
        let combined = u16::from_be_bytes(data[8..10].try_into().unwrap());
        // Stock mosh: bit 15 = final_flag (1 = final/only fragment, 0 = more fragments follow)
        let final_flag = (combined >> 15) & 1 == 1;
        let fragment_num = combined & 0x7FFF;
        let payload = data[FRAG_HEADER_LEN..].to_vec();
        Ok(Self { id, final_flag, fragment_num, payload })
    }
}

/// Compresses an Instruction protobuf and splits it into MTU-sized fragments.
pub struct Fragmenter {
    next_id: u64,
}

impl Default for Fragmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Fragmenter {
    pub fn new() -> Self {
        log::info!("Using raw deflate compression (stock mosh compatible)");
        Self { next_id: 1 }
    }
    /// Compress and fragment an Instruction, returning the fragments.
    ///
    /// `max_payload` is the maximum fragment payload size (after the 8-byte
    /// fragment header).
    pub fn make_fragments(
        &mut self,
        inst: &TransportInstruction,
        max_payload: usize,
    ) -> Result<Vec<Fragment>, String> {
        let new_num = inst.new_num.unwrap_or(0);
        let inst_bytes = encode_instruction(inst);
        let compressed = compress(&inst_bytes)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // Stock mosh prepends a 4-byte header: [new_num BE 2B] [flags BE 2B].
        // Single-fragment messages use flags=0x0000 (complete).
        // Multi-fragment first fragment uses flags=0x8000 (no_complete)
        // so the receiver waits for more fragments.
        if 4 + compressed.len() <= max_payload {
            let payload = add_stock_mosh_header(compressed, new_num, false);
            return Ok(vec![Fragment {
                id,
                final_flag: true,
                fragment_num: 0,
                payload,
            }]);
        }

        // Doesn't fit in one fragment — add header with no_complete flag
        // and split into chunks.
        let payload = add_stock_mosh_header(compressed, new_num, true);
        let mut fragments = Vec::new();
        for (i, chunk) in payload.chunks(max_payload).enumerate() {
            fragments.push(Fragment {
                id,
                final_flag: i == (payload.len() - 1) / max_payload,
                fragment_num: i as u16,
                payload: chunk.to_vec(),
            });
        }
        Ok(fragments)
    }

    /// Same as `make_fragments` but reuses the same ID for identical instructions.
    /// This allows the receiver to deduplicate retransmissions.
    pub fn make_fragments_with_id(
        &mut self,
        inst: &TransportInstruction,
        max_payload: usize,
        id: u64,
    ) -> Result<Vec<Fragment>, String> {
        let new_num = inst.new_num.unwrap_or(0);
        let inst_bytes = encode_instruction(inst);
        let compressed = compress(&inst_bytes)?;

        if 4 + compressed.len() <= max_payload {
            let payload = add_stock_mosh_header(compressed, new_num, false);
            return Ok(vec![Fragment {
                id,
                final_flag: true,
                fragment_num: 0,
                payload,
            }]);
        }

        let payload = add_stock_mosh_header(compressed, new_num, true);
        let total = payload.len().div_ceil(max_payload);
        let mut fragments = Vec::new();
        for (i, chunk) in payload.chunks(max_payload).enumerate() {
            fragments.push(Fragment {
                id,
                final_flag: i == total - 1,
                fragment_num: i as u16,
                payload: chunk.to_vec(),
            });
        }
        Ok(fragments)
    }
}
/// Reassembles fragments into a complete Instruction.
pub struct FragmentAssembly {
    pending: std::collections::HashMap<u64, std::collections::HashMap<u16, Vec<u8>>>,
    final_flags: std::collections::HashMap<u64, u16>,
}

impl Default for FragmentAssembly {
    fn default() -> Self {
        Self::new()
    }
}

impl FragmentAssembly {
    pub fn new() -> Self {
        Self {
            pending: std::collections::HashMap::new(),
            final_flags: std::collections::HashMap::new(),
        }
    }

    /// Add a fragment. Returns a complete Instruction if all fragments have arrived.
    pub fn add_fragment(&mut self, frag: Fragment) -> Result<Option<TransportInstruction>, String> {
        log::debug!("add_fragment: id={}, frag_num={}, final={}, payload_len={}",
            frag.id, frag.fragment_num, frag.final_flag, frag.payload.len());

        if frag.final_flag {
            self.final_flags.insert(frag.id, frag.fragment_num);
        }

        let entry = self.pending.entry(frag.id).or_default();
        entry.insert(frag.fragment_num, frag.payload);

        if let Some(&last_frag_num) = self.final_flags.get(&frag.id) {
            let expected_count = (last_frag_num as usize) + 1;
            if entry.len() == expected_count {
                // All fragments received — assemble
                let mut data = Vec::new();
                for i in 0..=last_frag_num {
                    data.extend_from_slice(entry.get(&i).ok_or("Missing fragment")?);
                }
                self.pending.remove(&frag.id);
                self.final_flags.remove(&frag.id);
                log::debug!("Assembled {} fragments, {} compressed bytes", expected_count, data.len());
                let decompressed = decompress(&data)?;
                log::debug!("Decompressed to {} bytes, decoding protobuf", decompressed.len());
                let inst = TransportInstruction::decode(decompressed.as_slice())
                    .map_err(|e| format!("Protobuf decode error: {e}"))?;
                log::debug!("Decoded TransportInstruction: old={:?} new={:?} ack={:?} diff_len={:?}",
                    inst.old_num, inst.new_num, inst.ack_num, inst.diff.as_ref().map(|d| d.len()));
                return Ok(Some(inst));
            } else {
                log::debug!("Waiting for more fragments: have {}, need {}", entry.len(), expected_count);
            }
        } else {
            log::debug!("No final flag yet for id={}", frag.id);
        }

        Ok(None)
    }

    /// Clear stale assemblies older than a given ID threshold.
    pub fn prune(&mut self, min_id: u64) {
        self.pending.retain(|&id, _| id >= min_id);
        self.final_flags.retain(|&id, _| id >= min_id);
    }
}

fn encode_instruction(inst: &TransportInstruction) -> Vec<u8> {
    let mut buf = Vec::with_capacity(inst.encoded_len());
    inst.encode(&mut buf).unwrap();
    buf
}

/// Stock mosh payload format: [2-byte new_num BE] [2-byte flags] [zlib compressed data]
/// The 4-byte header is plaintext; the rest is zlib-compressed TransportInstruction.
fn compress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).map_err(|e| format!("Compression error: {e}"))?;
    let compressed = encoder.finish().map_err(|e| format!("Compression finish error: {e}"))?;
    log::debug!("Compressed {} -> {} bytes (zlib)", data.len(), compressed.len());
    Ok(compressed)
}

fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    // Stock mosh prepends a 4-byte prefix before the zlib stream:
    //   [new_num 2B BE] [flags 2B BE] [zlib compressed protobuf]
    // Our own old format has no prefix.
    //
    // Detect the prefix: zlib always starts with 0x78 (deflate, level 1-9),
    // so if byte at offset 4 is 0x78, there's a 4-byte header.
    // For data < 5 bytes there can't be a header.
    let zlib_start = if data.len() >= 5 && data[4] == 0x78 {
        let state = u16::from_be_bytes([data[0], data[1]]);
        let flags = u16::from_be_bytes([data[2], data[3]]);
        log::debug!("Detected 4-byte stock mosh prefix: new_num={}, flags=0x{:04x}", state, flags);
        4
    } else if data.len() >= 4 {
        // Fallback: also check old heuristic (flags high bit) for edge cases
        let maybe_state = u16::from_be_bytes([data[0], data[1]]);
        let maybe_flags = u16::from_be_bytes([data[2], data[3]]);
        if maybe_flags == 0x8000 && maybe_state < 1024 {
            log::debug!("Detected 4-byte stock mosh prefix via fallback heuristic");
            4
        } else {
            0
        }
    } else {
        0
    };

    let compressed = &data[zlib_start..];
    if !data.is_empty() {
        let first_bytes: Vec<String> = compressed.iter().take(8).map(|b| format!("{:02x}", b)).collect();
        log::debug!("Compressed data first {} bytes: [{}]", first_bytes.len(), first_bytes.join(" "));
    }

    let mut decoder = flate2::read::ZlibDecoder::new(compressed);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)
        .map_err(|e| format!("Zlib decompression error: {e} (data_len={}, header_skip={})", data.len(), zlib_start))?;
    log::debug!("Decompressed {} -> {} bytes (zlib, skip={} header)", data.len(), output.len(), zlib_start);
    Ok(output)
}

/// Add stock mosh 4-byte header prefix: [new_num 2B BE] [flags 2B BE].
/// `no_complete` sets the INST_HEADER_NO_COMPLETE flag (bit 15),
/// indicating more fragments follow for the same instruction.
/// Single-fragment (complete) messages use `no_complete = false` → flags = 0x0000.
fn add_stock_mosh_header(payload: Vec<u8>, new_num: u64, no_complete: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(new_num as u16).to_be_bytes());
    let flags: u16 = if no_complete { 0x8000 } else { 0x0000 };
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Add random chaff bytes to an Instruction's chaff field.
pub fn add_chaff(inst: &mut TransportInstruction, prng_byte: u8) {
    let chaff_len = (prng_byte as usize) % (CHAFF_MAX + 1);
    if chaff_len > 0 {
        let mut chaff = vec![0u8; chaff_len];
        // Use simple PRNG for chaff content (not security-critical)
        for (i, byte) in chaff.iter_mut().enumerate() {
            *byte = prng_byte.wrapping_add(i as u8).wrapping_mul(0x9E);
        }
        inst.chaff = Some(chaff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morsh_proto::transport::Instruction as TI;
    use crate::constants::MORSH_PROTOCOL_VERSION;

    #[test]
    fn fragment_roundtrip() {
        let frag = Fragment {
            id: 42,
            final_flag: true,
            fragment_num: 0,
            payload: vec![1, 2, 3, 4],
        };
        let bytes = frag.to_bytes();
        let parsed = Fragment::from_bytes(&bytes).unwrap();
        assert_eq!(frag, parsed);
    }

    #[test]
    fn fragment_final_flag_encoding() {
        let frag = Fragment {
            id: 1,
            final_flag: true,
            fragment_num: 5,
            payload: vec![],
        };
        let bytes = frag.to_bytes();
        let parsed = Fragment::from_bytes(&bytes).unwrap();
        assert!(parsed.final_flag);
        assert_eq!(parsed.fragment_num, 5);
    }

    #[test]
    fn fragmenter_single_fragment() {
        let mut fragger = Fragmenter::new();
        let inst = TI {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(0),
            new_num: Some(1),
            ack_num: Some(0),
            throwaway_num: Some(0),
            diff: Some(vec![1, 2, 3]),
            chaff: None,
        };
        let frags = fragger.make_fragments(&inst, 1000).unwrap();
        assert_eq!(frags.len(), 1);
        assert!(frags[0].final_flag);
    }
    #[test]
    fn fragmenter_multi_fragment() {
        // 2000 bytes of variably incrementing data to force fragmentation
        let big_diff: Vec<u8> = (0..2000).map(|i| (i % 200) as u8).collect();
        let mut fragger = Fragmenter::new();
        let inst = TI {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(0),
            new_num: Some(1),
            ack_num: Some(0),
            throwaway_num: Some(0),
            diff: Some(big_diff),
            chaff: None,
        };
        let frags = fragger.make_fragments(&inst, 100).unwrap();
        assert!(frags.len() > 1, "Expected >1 fragments, got {}", frags.len());
        assert!(!frags[0].final_flag);
        assert!(frags.last().unwrap().final_flag);
    }

    #[test]
    fn assembly_roundtrip() {
        let mut fragger = Fragmenter::new();
        let inst = TI {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(10),
            new_num: Some(20),
            ack_num: Some(5),
            throwaway_num: Some(3),
            diff: Some(b"hello world".to_vec()),
            chaff: None,
        };
        let frags = fragger.make_fragments(&inst, 200).unwrap();

        let mut assembler = FragmentAssembly::new();
        for frag in frags {
            let result = assembler.add_fragment(frag).unwrap();
            if let Some(reassembled) = result {
                assert_eq!(reassembled.protocol_version, Some(MORSH_PROTOCOL_VERSION));
                assert_eq!(reassembled.old_num, Some(10));
                assert_eq!(reassembled.new_num, Some(20));
                assert_eq!(reassembled.diff, Some(b"hello world".to_vec()));
                return;
            }
        }
        panic!("Assembly did not complete");
    }

    #[test]
    fn add_chaff_works() {
        let mut inst = TI::default();
        add_chaff(&mut inst, 0xAB);
        assert!(inst.chaff.is_some());
        let chaff = inst.chaff.unwrap();
        assert!(!chaff.is_empty());
        assert!(chaff.len() <= CHAFF_MAX);
    }
}
