use crate::constants::{CHAFF_MAX, FRAG_HEADER_LEN};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use morsh_crypto::Prng;
use morsh_proto::transport::Instruction as TransportInstruction;
use prost::Message;

/// Largest instruction we will inflate. Mosh's `Compressor` uses a fixed
/// 2048*2048-byte buffer ("effective limit on terminal size") and rejects
/// anything that does not fit; without a cap, a few kilobytes of zlib from
/// the peer could inflate to gigabytes.
pub const MAX_INSTRUCTION_LEN: usize = 2048 * 2048;

/// Fragment numbers are 15 bits on the wire.
const MAX_FRAGMENTS: usize = 0x8000;

/// A single fragment of a larger instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub id: u64,
    pub final_flag: bool,
    pub fragment_num: u16,
    pub payload: Vec<u8>,
}

/// The 10-byte fragment header: id, then the fragment number with the
/// final flag in bit 15 (stock mosh: 1 = final/only fragment).
fn encode_header(id: u64, fragment_num: u16, final_flag: bool) -> [u8; FRAG_HEADER_LEN] {
    let mut header = [0u8; FRAG_HEADER_LEN];
    header[..8].copy_from_slice(&id.to_be_bytes());
    let combined = (final_flag as u16) << 15 | (fragment_num & 0x7FFF);
    header[8..].copy_from_slice(&combined.to_be_bytes());
    header
}

/// Split a serialized fragment into `(id, fragment_num, final_flag, payload)`.
fn decode_header(data: &[u8]) -> Result<(u64, u16, bool, &[u8]), String> {
    let Some((header, payload)) = data.split_first_chunk::<FRAG_HEADER_LEN>() else {
        return Err("Fragment too short".into());
    };
    let id = u64::from_be_bytes(header[..8].try_into().unwrap());
    let combined = u16::from_be_bytes([header[8], header[9]]);
    Ok((id, combined & 0x7FFF, combined & 0x8000 != 0, payload))
}

impl Fragment {
    /// Serialize this fragment to bytes for transmission.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FRAG_HEADER_LEN + self.payload.len());
        buf.extend_from_slice(&encode_header(self.id, self.fragment_num, self.final_flag));
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Parse a fragment from raw bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let (id, fragment_num, final_flag, payload) = decode_header(data)?;
        Ok(Self {
            id,
            final_flag,
            fragment_num,
            payload: payload.to_vec(),
        })
    }
}

/// The fields that identify an instruction for id assignment, plus the
/// payload size it was split for. Mosh compares everything except `diff`,
/// because two instructions with the same numbering carry the same diff;
/// keeping only these avoids cloning a whole screen diff on every send.
#[derive(Default, PartialEq)]
struct InstructionKey {
    protocol_version: Option<u32>,
    old_num: Option<u64>,
    new_num: Option<u64>,
    ack_num: Option<u64>,
    throwaway_num: Option<u64>,
    chaff: Option<Vec<u8>>,
    max_payload: usize,
}

impl InstructionKey {
    fn matches(&self, inst: &TransportInstruction, max_payload: usize) -> bool {
        self.max_payload == max_payload
            && self.old_num == inst.old_num
            && self.new_num == inst.new_num
            && self.ack_num == inst.ack_num
            && self.throwaway_num == inst.throwaway_num
            && self.chaff == inst.chaff
            && self.protocol_version == inst.protocol_version
    }

    fn assign(&mut self, inst: &TransportInstruction, max_payload: usize) {
        self.protocol_version = inst.protocol_version;
        self.old_num = inst.old_num;
        self.new_num = inst.new_num;
        self.ack_num = inst.ack_num;
        self.throwaway_num = inst.throwaway_num;
        self.chaff.clone_from(&inst.chaff); // reuses the allocation
        self.max_payload = max_payload;
    }
}

/// An instruction compressed and ready to be sent; see
/// [`Fragmenter::prepare`].
#[derive(Debug, Clone, Copy)]
pub struct Prepared {
    pub id: u64,
    pub count: usize,
    max_payload: usize,
}

/// Compresses an Instruction protobuf and splits it into MTU-sized fragments.
///
/// Mirrors mosh's `Network::Fragmenter` (`src/network/transportfragment.cc`):
/// the instruction id advances only when the instruction actually changed, so
/// a byte-identical retransmission reuses its id and the peer's assembler
/// recognises it as the packet it already holds instead of starting over.
///
/// The protobuf and zlib buffers and the deflate state are reused from one
/// instruction to the next, so preparing an instruction does not allocate
/// once the buffers have grown to the working size.
pub struct Fragmenter {
    next_instruction_id: u64,
    last: Option<InstructionKey>,
    encoded: Vec<u8>,
    compressed: Vec<u8>,
    deflate: Compress,
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
            last: None,
            encoded: Vec::new(),
            compressed: Vec::new(),
            deflate: Compress::new(Compression::fast(), true),
        }
    }

    /// The ack number carried by the last instruction we fragmented.
    ///
    /// Mosh's `last_ack_sent()`; a value of `u64::MAX` means we have already
    /// acknowledged the counterparty's shutdown.
    pub fn last_ack_sent(&self) -> u64 {
        self.last.as_ref().and_then(|k| k.ack_num).unwrap_or(0)
    }

    /// Serialize and compress `inst` for fragments of at most `max_payload`
    /// bytes (after the 10-byte fragment header). Fetch them with
    /// [`Fragmenter::fragment`].
    pub fn prepare(
        &mut self,
        inst: &TransportInstruction,
        max_payload: usize,
    ) -> Result<Prepared, String> {
        if max_payload == 0 {
            return Err("max_payload must be positive".into());
        }
        let last = self.last.get_or_insert_with(InstructionKey::default);
        // A fresh key never matches: `max_payload` is never 0.
        if !last.matches(inst, max_payload) {
            self.next_instruction_id += 1;
            last.assign(inst, max_payload);
        }

        self.encoded.clear();
        inst.encode(&mut self.encoded)
            .map_err(|e| format!("Protobuf encode error: {e}"))?;
        compress_into(&mut self.deflate, &self.encoded, &mut self.compressed)?;

        let count = self.compressed.len().div_ceil(max_payload).max(1);
        if count > MAX_FRAGMENTS {
            return Err(format!("Instruction needs {count} fragments"));
        }
        Ok(Prepared {
            id: self.next_instruction_id,
            count,
            max_payload,
        })
    }

    /// Fragment `index` of the instruction last [`prepare`]d: its header and
    /// payload.
    ///
    /// [`prepare`]: Fragmenter::prepare
    pub fn fragment(&self, prepared: &Prepared, index: usize) -> ([u8; FRAG_HEADER_LEN], &[u8]) {
        let start = (index * prepared.max_payload).min(self.compressed.len());
        let end = (start + prepared.max_payload).min(self.compressed.len());
        let header = encode_header(prepared.id, index as u16, index + 1 == prepared.count);
        (header, &self.compressed[start..end])
    }

    /// Compress and fragment an Instruction, returning owned fragments.
    pub fn make_fragments(
        &mut self,
        inst: &TransportInstruction,
        max_payload: usize,
    ) -> Result<Vec<Fragment>, String> {
        let prepared = self.prepare(inst, max_payload)?;
        Ok((0..prepared.count)
            .map(|i| {
                let (_, payload) = self.fragment(&prepared, i);
                Fragment {
                    id: prepared.id,
                    final_flag: i + 1 == prepared.count,
                    fragment_num: i as u16,
                    payload: payload.to_vec(),
                }
            })
            .collect())
    }
}

/// Reassembles fragments into a complete Instruction.
///
/// Mosh's model: exactly one instruction is under assembly at a time. A
/// fragment carrying a different id abandons whatever was in progress. That
/// makes the assembler O(1) in memory — the previous `HashMap` keyed by id
/// grew without bound whenever a fragmented packet was lost.
///
/// An instruction that fits in one fragment (nearly all of them) is inflated
/// straight from the datagram without being copied; the inflate state and
/// buffers are reused.
pub struct FragmentAssembly {
    fragments: Vec<Option<Vec<u8>>>,
    current_id: Option<u64>,
    fragments_arrived: usize,
    fragments_total: Option<usize>,
    joined: Vec<u8>,
    inflated: Vec<u8>,
    inflate: Decompress,
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
            joined: Vec::new(),
            inflated: Vec::new(),
            inflate: Decompress::new(true),
        }
    }

    /// Add a fragment. Returns a complete Instruction if all fragments have arrived.
    pub fn add_fragment(&mut self, frag: Fragment) -> Result<Option<TransportInstruction>, String> {
        self.add(
            frag.id,
            frag.fragment_num,
            frag.final_flag,
            Payload::Owned(frag.payload),
        )
    }

    /// Add a fragment in its wire form (header + payload).
    pub fn add_fragment_bytes(
        &mut self,
        data: &[u8],
    ) -> Result<Option<TransportInstruction>, String> {
        let (id, fragment_num, final_flag, payload) = decode_header(data)?;
        self.add(id, fragment_num, final_flag, Payload::Borrowed(payload))
    }

    fn add(
        &mut self,
        id: u64,
        fragment_num: u16,
        final_flag: bool,
        payload: Payload<'_>,
    ) -> Result<Option<TransportInstruction>, String> {
        let index = fragment_num as usize;

        // A complete single-fragment instruction with nothing in progress
        // for it: decode in place. This leaves exactly the state the general
        // path below would after assembling it.
        if index == 0 && final_flag && (self.current_id != Some(id) || self.fragments_arrived == 0)
        {
            self.fragments.clear();
            self.fragments_arrived = 0;
            self.fragments_total = None;
            self.current_id = Some(id);
            return self.decode(payload.as_slice()).map(Some);
        }

        if self.current_id != Some(id) {
            // A totally new packet: whatever was in progress is abandoned.
            self.fragments.clear();
            self.fragments.resize(index + 1, None);
            self.fragments_arrived = 1;
            self.fragments_total = None;
            self.current_id = Some(id);
            self.fragments[index] = Some(payload.into_vec());
        } else if self.fragments.get(index).is_some_and(Option::is_some) {
            // A duplicate. Mosh asserts the two copies agree; we simply
            // ignore the second one rather than aborting on a packet an
            // attacker controls.
        } else {
            if self.fragments.len() < index + 1 {
                self.fragments.resize(index + 1, None);
            }
            self.fragments[index] = Some(payload.into_vec());
            self.fragments_arrived += 1;
        }
        if final_flag {
            self.fragments_total = Some(index + 1);
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
        let mut joined = std::mem::take(&mut self.joined);
        joined.clear();
        for slot in self.fragments.iter().take(total) {
            joined.extend_from_slice(slot.as_deref().ok_or("Missing fragment")?);
        }
        self.fragments.clear();
        self.fragments_arrived = 0;
        self.fragments_total = None;

        let result = self.decode(&joined);
        self.joined = joined;
        result
    }

    fn decode(&mut self, compressed: &[u8]) -> Result<TransportInstruction, String> {
        decompress_into(&mut self.inflate, compressed, &mut self.inflated)?;
        TransportInstruction::decode(self.inflated.as_slice())
            .map_err(|e| format!("Protobuf decode error: {e}"))
    }
}

/// A fragment payload, still in the datagram or already owned.
enum Payload<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

impl Payload<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Payload::Borrowed(s) => s,
            Payload::Owned(v) => v,
        }
    }

    fn into_vec(self) -> Vec<u8> {
        match self {
            Payload::Borrowed(s) => s.to_vec(),
            Payload::Owned(v) => v,
        }
    }
}

/// Stock mosh payload format: a zlib stream holding the serialized
/// TransportInstruction, with no extra header.
fn compress_into(deflate: &mut Compress, input: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    deflate.reset();
    out.clear();
    // zlib's compressBound, roughly; the loop grows the buffer if needed.
    out.reserve(input.len() + input.len() / 1000 + 64);
    loop {
        let consumed = deflate.total_in() as usize;
        let status = deflate
            .compress_vec(&input[consumed..], out, FlushCompress::Finish)
            .map_err(|e| format!("Compression error: {e}"))?;
        match status {
            Status::StreamEnd => return Ok(()),
            Status::Ok | Status::BufError => out.reserve(out.capacity().max(64)),
        }
    }
}

/// Inflate a zlib stream into `out`, refusing to grow it past
/// [`MAX_INSTRUCTION_LEN`].
fn decompress_into(
    inflate: &mut Decompress,
    input: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let corrupt = |what: &dyn std::fmt::Display| {
        format!(
            "Zlib decompression error: {what} (data_len={})",
            input.len()
        )
    };
    inflate.reset(true);
    out.clear();
    out.reserve(
        input
            .len()
            .saturating_mul(4)
            .clamp(256, MAX_INSTRUCTION_LEN),
    );
    loop {
        let consumed = inflate.total_in() as usize;
        let produced = inflate.total_out();
        // `Finish` makes miniz_oxide fail outright when the output buffer is
        // too small on the first call; `None` resumes cleanly.
        let status = inflate
            .decompress_vec(&input[consumed..], out, FlushDecompress::None)
            .map_err(|e| corrupt(&e))?;
        if status == Status::StreamEnd {
            return Ok(());
        }
        if out.len() == out.capacity() {
            if out.len() >= MAX_INSTRUCTION_LEN {
                return Err(format!(
                    "Instruction inflates beyond {MAX_INSTRUCTION_LEN} bytes"
                ));
            }
            let grow = out.len().max(256).min(MAX_INSTRUCTION_LEN - out.len());
            out.reserve_exact(grow);
        } else if inflate.total_in() as usize == consumed && inflate.total_out() == produced {
            // Room to write, yet no progress: the input ended mid-stream.
            return Err(corrupt(&"truncated stream"));
        }
    }
}

#[cfg(test)]
fn compress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    compress_into(
        &mut Compress::new(Compression::fast(), true),
        data,
        &mut out,
    )?;
    Ok(out)
}

#[cfg(test)]
fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    decompress_into(&mut Decompress::new(true), data, &mut out)?;
    Ok(out)
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

    /// A zlib bomb: a few kilobytes that inflate past mosh's 4 MiB limit.
    #[test]
    fn oversized_inflation_is_rejected() {
        let bomb = compress(&vec![0u8; MAX_INSTRUCTION_LEN + 1]).unwrap();
        assert!(bomb.len() < 64 * 1024, "{} bytes", bomb.len());
        let mut assembler = FragmentAssembly::new();
        let mut result = Ok(None);
        for (i, chunk) in bomb.chunks(1000).enumerate() {
            result = assembler.add_fragment(Fragment {
                id: 1,
                final_flag: (i + 1) * 1000 >= bomb.len(),
                fragment_num: i as u16,
                payload: chunk.to_vec(),
            });
        }
        let err = result.unwrap_err();
        assert!(err.contains("inflates beyond"), "{err}");
    }

    #[test]
    fn instruction_at_the_limit_is_accepted() {
        let data = vec![0u8; MAX_INSTRUCTION_LEN];
        assert_eq!(
            decompress(&compress(&data).unwrap()).unwrap().len(),
            data.len()
        );
    }

    #[test]
    fn truncated_or_corrupt_streams_are_errors() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7919 % 251) as u8).collect();
        let z = compress(&data).unwrap();
        for cut in [0, 1, 2, z.len() / 2, z.len() - 1] {
            assert!(decompress(&z[..cut]).is_err(), "cut at {cut}");
        }
        let mut bad = z.clone();
        bad[z.len() / 2] ^= 0xFF;
        let _ = decompress(&bad); // must return, not hang or panic
        assert_eq!(decompress(&z).unwrap(), data);
    }

    /// Reused compressor and inflater state must not leak between
    /// instructions.
    #[test]
    fn reused_state_roundtrips_many_instructions() {
        let mut fragger = Fragmenter::new();
        let mut assembler = FragmentAssembly::new();
        for n in 1..50u64 {
            let diff: Vec<u8> = (0..(n * 97) as u32).map(|i| (i * 31 % 253) as u8).collect();
            let mut got = None;
            for frag in fragger
                .make_fragments(&inst(n - 1, n, diff.clone()), 300)
                .unwrap()
            {
                got = assembler.add_fragment_bytes(&frag.to_bytes()).unwrap();
            }
            assert_eq!(got.expect("complete").diff, Some(diff), "instruction {n}");
        }
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
