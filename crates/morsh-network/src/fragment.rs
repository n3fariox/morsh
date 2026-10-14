use crate::constants::{CHAFF_MAX, FRAG_HEADER_LEN};
use flate2::Compression;
use morsh_crypto::Prng;
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
        Ok(Self {
            id,
            final_flag,
            fragment_num,
            payload,
        })
    }
}

/// Compresses an Instruction protobuf and splits it into MTU-sized fragments.
///
/// Mirrors mosh's `Network::Fragmenter` (`src/network/transportfragment.cc`):
/// the instruction id advances only when the instruction actually changed, so
/// a byte-identical retransmission reuses its id and the peer's assembler
/// recognises it as the packet it already holds instead of starting over.
pub struct Fragmenter {
    next_instruction_id: u64,
    last_instruction: Option<TransportInstruction>,
    last_max_payload: Option<usize>,
}

impl Default for Fragmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Fragmenter {
    pub fn new() -> Self {
        Self {
            next_instruction_id: 0,
            last_instruction: None,
            last_max_payload: None,
        }
    }

    /// Whether `inst` differs from the last instruction in any field that
    /// identifies the packet. Mosh compares everything except `diff`, because
    /// two instructions with the same numbering necessarily carry the same
    /// diff.
    fn is_new_instruction(&self, inst: &TransportInstruction, max_payload: usize) -> bool {
        let Some(last) = &self.last_instruction else {
            return true;
        };
        self.last_max_payload != Some(max_payload)
            || inst.old_num != last.old_num
            || inst.new_num != last.new_num
            || inst.ack_num != last.ack_num
            || inst.throwaway_num != last.throwaway_num
            || inst.chaff != last.chaff
            || inst.protocol_version != last.protocol_version
    }

    /// The ack number carried by the last instruction we fragmented.
    ///
    /// Mosh's `last_ack_sent()`; a value of `u64::MAX` means we have already
    /// acknowledged the counterparty's shutdown.
    pub fn last_ack_sent(&self) -> u64 {
        self.last_instruction
            .as_ref()
            .and_then(|i| i.ack_num)
            .unwrap_or(0)
    }

    /// Compress and fragment an Instruction, returning the fragments.
    ///
    /// `max_payload` is the maximum fragment payload size (after the 10-byte
    /// fragment header).
    pub fn make_fragments(
        &mut self,
        inst: &TransportInstruction,
        max_payload: usize,
    ) -> Result<Vec<Fragment>, String> {
        if max_payload == 0 {
            return Err("max_payload must be positive".into());
        }
        if self.is_new_instruction(inst, max_payload) {
            self.next_instruction_id += 1;
        }
        self.last_instruction = Some(inst.clone());
        self.last_max_payload = Some(max_payload);
        let id = self.next_instruction_id;

        let compressed = compress(&encode_instruction(inst))?;

        let total = compressed.len().div_ceil(max_payload).max(1);
        let mut fragments = Vec::with_capacity(total);
        for (i, chunk) in compressed.chunks(max_payload).enumerate() {
            fragments.push(Fragment {
                id,
                final_flag: i == total - 1,
                fragment_num: i as u16,
                payload: chunk.to_vec(),
            });
        }
        if fragments.is_empty() {
            fragments.push(Fragment {
                id,
                final_flag: true,
                fragment_num: 0,
                payload: Vec::new(),
            });
        }
        Ok(fragments)
    }
}

/// Reassembles fragments into a complete Instruction.
///
/// Mosh's model: exactly one instruction is under assembly at a time. A
/// fragment carrying a different id abandons whatever was in progress. That
/// makes the assembler O(1) in memory — the previous `HashMap` keyed by id
/// grew without bound whenever a fragmented packet was lost.
pub struct FragmentAssembly {
    fragments: Vec<Option<Fragment>>,
    current_id: Option<u64>,
    fragments_arrived: usize,
    fragments_total: Option<usize>,
}

impl Default for FragmentAssembly {
    fn default() -> Self {
        Self::new()
    }
}

impl FragmentAssembly {
    pub fn new() -> Self {
        Self {
            fragments: Vec::new(),
            current_id: None,
            fragments_arrived: 0,
            fragments_total: None,
        }
    }

    /// Add a fragment. Returns a complete Instruction if all fragments have arrived.
    pub fn add_fragment(&mut self, frag: Fragment) -> Result<Option<TransportInstruction>, String> {
        let index = frag.fragment_num as usize;

        if self.current_id != Some(frag.id) {
            // A totally new packet: whatever was in progress is abandoned.
            self.fragments.clear();
            self.fragments.resize(index + 1, None);
            self.fragments_arrived = 1;
            self.fragments_total = None;
            self.current_id = Some(frag.id);
            if frag.final_flag {
                self.fragments_total = Some(index + 1);
            }
            self.fragments[index] = Some(frag);
        } else {
            let already_have = self
                .fragments
                .get(index)
                .map(|f| f.is_some())
                .unwrap_or(false);
            if already_have {
                // A duplicate. Mosh asserts the two copies agree; we simply
                // ignore the second one rather than aborting on a packet an
                // attacker controls.
                if frag.final_flag {
                    self.fragments_total = Some(index + 1);
                }
            } else {
                if self.fragments.len() < index + 1 {
                    self.fragments.resize(index + 1, None);
                }
                if frag.final_flag {
                    self.fragments_total = Some(index + 1);
                }
                self.fragments[index] = Some(frag);
                self.fragments_arrived += 1;
            }
        }

        if let Some(total) = self.fragments_total {
            if self.fragments.len() > total {
                return Err(format!(
                    "Fragment {} arrived after the final fragment {}",
                    self.fragments.len() - 1,
                    total - 1
                ));
            }
            if self.fragments_arrived == total {
                return self.assemble(total).map(Some);
            }
        }

        Ok(None)
    }

    fn assemble(&mut self, total: usize) -> Result<TransportInstruction, String> {
        let mut data = Vec::new();
        for slot in self.fragments.iter().take(total) {
            let frag = slot.as_ref().ok_or("Missing fragment")?;
            data.extend_from_slice(&frag.payload);
        }
        self.fragments.clear();
        self.fragments_arrived = 0;
        self.fragments_total = None;

        let decompressed = decompress(&data)?;
        TransportInstruction::decode(decompressed.as_slice())
            .map_err(|e| format!("Protobuf decode error: {e}"))
    }
}

fn encode_instruction(inst: &TransportInstruction) -> Vec<u8> {
    let mut buf = Vec::with_capacity(inst.encoded_len());
    inst.encode(&mut buf).unwrap();
    buf
}

/// Stock mosh payload format: a zlib stream holding the serialized
/// TransportInstruction, with no extra header.
fn compress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(data)
        .map_err(|e| format!("Compression error: {e}"))?;
    encoder
        .finish()
        .map_err(|e| format!("Compression finish error: {e}"))
}

fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .map_err(|e| format!("Zlib decompression error: {e} (data_len={})", data.len()))?;
    Ok(output)
}

/// Random padding that disguises the length of an instruction.
///
/// Mosh's `TransportSender::make_chaff`: a CSPRNG-chosen length in
/// `0..=CHAFF_MAX` of CSPRNG bytes. Deriving either from the state number —
/// as this used to — makes the padding perfectly predictable and defeats the
/// point of sending it.
pub fn make_chaff() -> Vec<u8> {
    let mut prng = Prng;
    let len = (prng.u8() as usize) % (CHAFF_MAX + 1);
    let mut chaff = vec![0u8; len];
    if len > 0 {
        Prng::fill(&mut chaff);
    }
    chaff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::MORSH_PROTOCOL_VERSION;
    use morsh_proto::transport::Instruction as TI;

    fn inst(old: u64, new: u64, diff: Vec<u8>) -> TI {
        TI {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(old),
            new_num: Some(new),
            ack_num: Some(0),
            throwaway_num: Some(0),
            diff: Some(diff),
            chaff: None,
        }
    }

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
        let frags = fragger.make_fragments(&inst(0, 1, vec![1, 2, 3]), 1000).unwrap();
        assert_eq!(frags.len(), 1);
        assert!(frags[0].final_flag);
    }

    #[test]
    fn fragmenter_multi_fragment() {
        // 2000 bytes of variably incrementing data to force fragmentation
        let big_diff: Vec<u8> = (0..2000).map(|i| (i % 200) as u8).collect();
        let mut fragger = Fragmenter::new();
        let frags = fragger.make_fragments(&inst(0, 1, big_diff), 100).unwrap();
        assert!(frags.len() > 1, "Expected >1 fragments, got {}", frags.len());
        assert!(!frags[0].final_flag);
        assert!(frags.last().unwrap().final_flag);
    }

    /// Mosh bumps the instruction id only when the instruction changed.
    #[test]
    fn identical_instruction_reuses_its_id() {
        let mut fragger = Fragmenter::new();
        let a = fragger.make_fragments(&inst(0, 1, b"x".to_vec()), 500).unwrap();
        let b = fragger.make_fragments(&inst(0, 1, b"x".to_vec()), 500).unwrap();
        assert_eq!(a[0].id, b[0].id, "retransmit should reuse the id");

        let c = fragger.make_fragments(&inst(1, 2, b"y".to_vec()), 500).unwrap();
        assert_ne!(b[0].id, c[0].id, "a new instruction needs a new id");
    }

    #[test]
    fn changed_chaff_or_mtu_forces_a_new_id() {
        let mut fragger = Fragmenter::new();
        let mut i1 = inst(0, 1, b"x".to_vec());
        let a = fragger.make_fragments(&i1, 500).unwrap();
        i1.chaff = Some(vec![7, 7, 7]);
        let b = fragger.make_fragments(&i1, 500).unwrap();
        assert_ne!(a[0].id, b[0].id);
        let c = fragger.make_fragments(&i1, 400).unwrap();
        assert_ne!(b[0].id, c[0].id);
    }

    #[test]
    fn assembly_roundtrip() {
        let mut fragger = Fragmenter::new();
        let mut i = inst(10, 20, b"hello world".to_vec());
        i.ack_num = Some(5);
        i.throwaway_num = Some(3);
        let frags = fragger.make_fragments(&i, 200).unwrap();

        let mut assembler = FragmentAssembly::new();
        for frag in frags {
            if let Some(reassembled) = assembler.add_fragment(frag).unwrap() {
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
    fn assembly_handles_out_of_order_fragments() {
        let big: Vec<u8> = (0..3000).map(|i| (i % 251) as u8).collect();
        let mut fragger = Fragmenter::new();
        let mut frags = fragger.make_fragments(&inst(0, 1, big.clone()), 100).unwrap();
        frags.reverse();

        let mut assembler = FragmentAssembly::new();
        let mut got = None;
        for frag in frags {
            if let Some(i) = assembler.add_fragment(frag).unwrap() {
                got = Some(i);
            }
        }
        assert_eq!(got.unwrap().diff, Some(big));
    }

    #[test]
    fn assembly_ignores_duplicate_fragments() {
        let big: Vec<u8> = (0..3000).map(|i| (i % 251) as u8).collect();
        let mut fragger = Fragmenter::new();
        let frags = fragger.make_fragments(&inst(0, 1, big.clone()), 100).unwrap();

        let mut assembler = FragmentAssembly::new();
        let mut completions = 0;
        for frag in frags.iter().flat_map(|f| [f.clone(), f.clone()]) {
            if assembler.add_fragment(frag).unwrap().is_some() {
                completions += 1;
            }
        }
        assert_eq!(completions, 1, "duplicates must not re-complete the assembly");
    }

    /// A lost fragment used to leave its partial assembly in a HashMap forever.
    #[test]
    fn abandoned_assembly_does_not_accumulate() {
        let big: Vec<u8> = (0..3000).map(|i| (i % 251) as u8).collect();
        let mut fragger = Fragmenter::new();
        let mut assembler = FragmentAssembly::new();

        for n in 0..50u64 {
            let frags = fragger
                .make_fragments(&inst(n, n + 1, big.clone()), 100)
                .unwrap();
            // Deliver everything except the final fragment.
            for frag in frags.iter().take(frags.len() - 1) {
                assert!(assembler.add_fragment(frag.clone()).unwrap().is_none());
            }
        }

        // Only the most recent instruction is still held.
        assert_eq!(assembler.current_id, Some(fragger.next_instruction_id));
        assert!(assembler.fragments.len() <= 40);
    }

    #[test]
    fn chaff_is_random_and_bounded() {
        let a = make_chaff();
        let b = make_chaff();
        assert!(a.len() <= CHAFF_MAX);
        assert!(b.len() <= CHAFF_MAX);
        // Two independent draws of up to 16 bytes are essentially never equal
        // unless both are empty.
        let samples: Vec<Vec<u8>> = (0..8).map(|_| make_chaff()).collect();
        assert!(
            samples.windows(2).any(|w| w[0] != w[1]),
            "chaff looks deterministic"
        );
    }
}
