//! Fixed-work benchmarks without a benchmark framework. Times are informational, never gates.
use morsh_crypto::{Message, Nonce, Session};
use morsh_network::{constants::*, fragment::Fragmenter, State};
use morsh_proto::transport::Instruction;
use morsh_statesync::{Complete, UserEvent, UserStream};
use morsh_terminal::{DisplayDiff, KeyMap, MoshTerminal};
use std::{hint::black_box, time::Instant};

fn measure(name: &str, units: usize, mut work: impl FnMut()) {
    // Warm caches/native allocator before recording independent fixed-work batches.
    for _ in 0..5 {
        work();
    }
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..20 {
            work();
        }
        samples.push(start.elapsed().as_nanos() as f64 / 20.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name},21,20,{units},{:.0},{:.0},{:.2}",
        samples[10],
        samples[19],
        units as f64 * 1e9 / samples[10]
    );
}

fn main() {
    println!("workload,samples,iterations,units_per_iteration,median_ns,p95_ns,units_per_second");
    let mut terminal = MoshTerminal::new(120, 40, 0).unwrap();
    let flood = "0123456789 abcdefghijklmnopqrstuvwxyz\r\n".repeat(8192);
    measure("emulate_ascii_flood_bytes", flood.len(), || {
        terminal.write(black_box(flood.as_bytes()))
    });
    let scroll = "scroll line 0123456789\r\n".repeat(100);
    measure("emulate_scroll_bytes", scroll.len(), || {
        terminal.write(black_box(scroll.as_bytes()))
    });
    let unicode = "界面 日本語 café e\u{301} 🦀\r\n".repeat(100);
    measure("emulate_unicode_bytes", unicode.len(), || {
        terminal.write(black_box(unicode.as_bytes()))
    });
    let redraw = format!("\x1b[H{}", "x".repeat(120 * 40));
    measure("emulate_full_screen_bytes", redraw.len(), || {
        terminal.write(black_box(redraw.as_bytes()))
    });
    let old = terminal.snapshot().unwrap();
    terminal.write(b"\x1b[20;60HZ");
    let new = terminal.snapshot().unwrap();
    measure("snapshot_cells", 4800, || {
        black_box(terminal.snapshot().unwrap());
    });
    measure("diff_unchanged_cells", 4800, || {
        black_box(DisplayDiff::diff(black_box(&new), black_box(&new)));
    });
    measure("diff_one_cell", 1, || {
        black_box(DisplayDiff::diff(black_box(&old), black_box(&new)));
    });
    measure("render_full_screen_cells", 4800, || {
        black_box(DisplayDiff::full_redraw(black_box(&new)));
    });
    let mut dense = new.clone();
    for row in &mut dense.rows {
        for cell in row {
            cell.text = "y".into();
        }
    }
    measure("diff_full_screen_cells", 4800, || {
        black_box(DisplayDiff::diff(black_box(&new), black_box(&dense)));
    });
    let mut scrolling = MoshTerminal::new(120, 40, 0).unwrap();
    for line in 0..39 {
        scrolling.write(format!("row {line:02}\r\n").as_bytes());
    }
    let before_scroll = scrolling.snapshot().unwrap();
    scrolling.write(b"\r\nnew row");
    let after_scroll = scrolling.snapshot().unwrap();
    measure("diff_scroll_cells", 4800, || {
        black_box(DisplayDiff::diff(
            black_box(&before_scroll),
            black_box(&after_scroll),
        ));
    });
    let mut input = UserStream::new();
    for &b in unicode.as_bytes() {
        input.push_key(b);
    }
    measure("input_diff_bytes", unicode.len(), || {
        black_box(input.diff_from(black_box(&UserStream::new())));
    });
    let mut keymap = KeyMap::new().unwrap();
    let mut state = Complete::new(120, 40).unwrap();
    measure("local_key_to_render", 1, || {
        let encoded = keymap.encode_char(black_box('x'));
        let mut input = UserStream::new();
        for byte in encoded {
            input.push_key(byte);
        }
        let diff = input.diff_from(&UserStream::new());
        let mut peer = UserStream::new();
        peer.apply_string(&diff).unwrap();
        let before = state.clone();
        // Model a PTY echo; excludes OS, network, compositor, and prediction.
        let echo: Vec<u8> = peer
            .events()
            .iter()
            .filter_map(|event| match event {
                UserEvent::UserByte(byte) => Some(*byte),
                _ => None,
            })
            .collect();
        state.act(&echo).unwrap();
        black_box(state.diff_from(&before));
    });
    let instruction = Instruction {
        protocol_version: Some(2),
        old_num: Some(0),
        new_num: Some(1),
        ack_num: Some(0),
        throwaway_num: Some(0),
        diff: Some(input.diff_from(&UserStream::new())),
        chaff: None,
    };
    measure(
        "wire_compress_fragment_bytes",
        instruction.diff.as_ref().unwrap().len(),
        || {
            black_box(
                Fragmenter::new()
                    .make_fragments(black_box(&instruction), max_fragment_payload(MTU_IPV4))
                    .unwrap(),
            );
        },
    );
    let fragments = Fragmenter::new()
        .make_fragments(&instruction, max_fragment_payload(MTU_IPV4))
        .unwrap();
    let mut session = Session::new([0x42; 16]);
    let mut sequence = 0;
    measure(
        "wire_encrypt_plaintext_bytes",
        fragments
            .iter()
            .map(|fragment| 4 + fragment.to_bytes().len())
            .sum(),
        || {
            for fragment in &fragments {
                let mut plain = vec![0xff; 4];
                plain.extend_from_slice(&fragment.to_bytes());
                black_box(session.encrypt(&Message::new(Nonce::from_val(sequence), plain)));
                sequence += 1;
            }
        },
    );
    let mut random_instruction = instruction.clone();
    let mut seed = 0x1234abcd_u32;
    random_instruction.diff = Some(
        (0..65536)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect(),
    );
    measure("wire_incompressible_bytes", 65536, || {
        black_box(
            Fragmenter::new()
                .make_fragments(
                    black_box(&random_instruction),
                    max_fragment_payload(MTU_IPV4),
                )
                .unwrap(),
        );
    });
}
