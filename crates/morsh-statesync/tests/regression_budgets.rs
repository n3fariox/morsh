//! Structural budgets are CI gates; elapsed time belongs in the benchmark.
use morsh_crypto::{Message, Nonce, Session};
use morsh_network::{
    constants::*,
    fragment::{FragmentAssembly, Fragmenter},
    State,
};
use morsh_proto::transport::Instruction;
use morsh_statesync::{Complete, UserStream};
use morsh_terminal::{DisplayDiff, MoshTerminal};

#[test]
#[serial_test::serial]
fn sparse_screen_updates_stay_sparse_and_reconstruct_unicode() {
    let mut source = Complete::new(120, 40).unwrap();
    source
        .act("界面 café e\u{301} 🦀\r\n".repeat(1000).as_bytes())
        .unwrap();
    let mut receiver = Complete::new(120, 40).unwrap();
    receiver.apply_string(&source.diff_from(&receiver)).unwrap();
    assert!(receiver.state_equal(&source));
    let before = source.clone();
    source.act(b"\x1b[20;60HZ").unwrap();
    let diff = source.diff_from(&before);
    assert!(
        diff.len() < 128,
        "one cell became {} protobuf bytes",
        diff.len()
    );
    receiver.apply_string(&diff).unwrap();
    assert!(receiver.state_equal(&source));
    assert!(source.diff_from(&source).is_empty());
    assert!(DisplayDiff::diff(source.screen(), source.screen()).is_empty());
}

#[test]
fn input_batching_and_encrypted_packet_budgets() {
    let mut input = UserStream::new();
    for &byte in b"hello" {
        input.push_key(byte);
    }
    let typed = input.diff_from(&UserStream::new());
    assert_eq!(typed.len(), 11, "five keys should share one instruction");
    for (name, diff, min_packets, max_packets, max_bytes) in [
        ("keystroke", typed, 1, 1, 100),
        ("redraw", vec![b'x'; 4800], 1, 1, 150),
        ("incompressible", noise(65536), 54, 56, 68000),
    ] {
        let instruction = Instruction {
            protocol_version: Some(2),
            old_num: Some(0),
            new_num: Some(1),
            ack_num: Some(0),
            throwaway_num: Some(0),
            diff: Some(diff),
            // Fixed maximum-length chaff covers worst-case privacy padding.
            chaff: Some(vec![0xab; CHAFF_MAX]),
        };
        let fragments = Fragmenter::new()
            .make_fragments(&instruction, max_fragment_payload(MTU_IPV4))
            .unwrap();
        assert!(
            (min_packets..=max_packets).contains(&fragments.len()),
            "{name}: {} packets",
            fragments.len()
        );
        let mut encrypt = Session::new([0x42; 16]);
        let mut decrypt = Session::new([0x42; 16]);
        let mut assembly = FragmentAssembly::new();
        let mut bytes = 0;
        let mut rebuilt = None;
        for (seq, fragment) in fragments.iter().enumerate().rev() {
            let mut plain = vec![0xff; 4]; // no RTT sample
            plain.extend_from_slice(&fragment.to_bytes());
            let packet = encrypt.encrypt(&Message::new(Nonce::from_val(seq as u64), plain));
            bytes += packet.len();
            assert!(packet.len() <= MTU_IPV4);
            let plain = decrypt.decrypt(&packet).unwrap().text;
            if let Some(inst) = assembly
                .add_fragment(morsh_network::fragment::Fragment::from_bytes(&plain[4..]).unwrap())
                .unwrap()
            {
                rebuilt = Some(inst);
            }
        }
        assert!(bytes <= max_bytes, "{name}: {bytes} UDP payload bytes");
        assert_eq!(rebuilt.unwrap(), instruction);
        println!("{name}: packets={}, udp_bytes={bytes}", fragments.len());
    }
}

#[test]
#[serial_test::serial]
fn full_redraw_and_scrolling_preserve_screen_contents() {
    let mut source = MoshTerminal::new(120, 40, 0).unwrap();
    for data in [
        "\x1b[H".to_owned() + &"x".repeat(4800),
        "scroll\r\n".repeat(1000),
        "界🦀e\u{301}\r\n".repeat(100),
    ] {
        source.write(data.as_bytes());
        let snapshot = source.snapshot().unwrap();
        let rendered = DisplayDiff::full_redraw(&snapshot);
        assert!(rendered.len() < 20000);
        let mut destination = MoshTerminal::new(120, 40, 0).unwrap();
        destination.write(rendered.as_bytes());
        assert_eq!(destination.snapshot().unwrap(), snapshot);
    }
}

fn noise(len: usize) -> Vec<u8> {
    let mut seed = 0x1234abcd_u32;
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect()
}

#[test]
fn incomplete_large_instruction_recovers_when_a_complete_replacement_arrives() {
    let mut instruction = Instruction {
        protocol_version: Some(2),
        old_num: Some(0),
        new_num: Some(1),
        ack_num: Some(0),
        throwaway_num: Some(0),
        diff: Some(noise(32768)),
        chaff: None,
    };
    let mut fragmenter = Fragmenter::new();
    let fragments = fragmenter
        .make_fragments(&instruction, max_fragment_payload(MTU_IPV4))
        .unwrap();
    assert!(fragments.len() > 11);
    let mut assembly = FragmentAssembly::new();
    // Dropping one fragment in every eleven guarantees this attempt is incomplete.
    for (i, fragment) in fragments.into_iter().enumerate() {
        if (i + 1) % 11 != 0 {
            assert!(assembly.add_fragment(fragment).unwrap().is_none());
        }
    }
    instruction.new_num = Some(2);
    let replacement = fragmenter
        .make_fragments(&instruction, max_fragment_payload(MTU_IPV4))
        .unwrap();
    let mut result = None;
    for fragment in replacement.into_iter().rev() {
        if let Some(value) = assembly.add_fragment(fragment).unwrap() {
            result = Some(value);
        }
    }
    assert_eq!(result.unwrap(), instruction);
}
