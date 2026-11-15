//! Expected replies captured from the reference mosh 1.4.0 terminal sources.
use morsh_terminal::MoshTerminal;

#[test]
#[serial_test::serial]
fn replies_match_stock_mosh_for_complete_and_fragmented_queries() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"\x1b[c", b"\x1b[?62c"),
        (b"\x1b[0c", b"\x1b[?62c"),
        (b"\x1b[123;456c", b"\x1b[?62c"),
        (b"\x1b[>c", b"\x1b[>1;10;0c"),
        (b"\x1b[>0c", b"\x1b[>1;10;0c"),
        (b"\x1b[>123;456c", b"\x1b[>1;10;0c"),
        (b"\x1b[5n", b"\x1b[0n"),
        (b"\x1b[05n", b"\x1b[0n"),
        (b"\x1b[5;999n", b"\x1b[0n"),
        (b"\x1b[6n", b"\x1b[1;1R"),
        (b"\x1b[6;999n", b"\x1b[1;1R"),
        (
            b"\x1b[999;999H\x1b[6n\x1b[H\x1b[6n",
            b"\x1b[40;120R\x1b[1;1R",
        ),
        (b"\x1b[3;8r\x1b[?6h\x1b[2;4H\x1b[6n", b"\x1b[4;4R"),
        (b"\x1b[H\xe7\x95\x8ce\xcc\x81\x1b[6n", b"\x1b[1;4R"),
        (b"\x1b[6\x0an", b"\x1b[2;1R"),
        (b"\x1b[6\x7fn", b"\x1b[1;1R"),
        (b"\x1b[?6n", b""),
        (b"\x1b[=c", b""),
        (b"\x1bZ", b""),
        (b"\x05", b""),
        (b"\x1b[?7$p", b""),
        (b"\x1b[18t", b""),
        (b"\x1b]10;?\x07", b""),
        (b"\x1b[?c", b""),
        (b"\x1b[5 n", b""),
        (b"\x1b[;5n", b""),
        (b"\x1b[65536n", b""),
        (b"\x1b[6:1n", b""),
        (b"\x1b[6\x18n", b""),
        (b"\x1b[6\x1an", b""),
        (b"\x1b[6\x1bn", b""),
        (b"\x1b[>6n", b""),
        (b"\x1b[>>c", b""),
        (b"\x1b]title;[6n\x07", b""),
        (b"\x1bPq[6n\x1b\\", b""),
        (b"\x1b_[6n\x1b\\", b""),
        (
            b"\xc2\x9bc\xc2\x9b>c\xc2\x9b5n\xc2\x9b6n",
            b"\x1b[?62c\x1b[>1;10;0c\x1b[0n\x1b[1;1R",
        ),
        (b"\x9bc", b""),
        (b"\x1b[\xc2\x1b[5n", b"\x1b[0n"),
        (b"\xc2\xa9\x1b[6n", b"\x1b[1;2R"),
        (b"\xc2\x84\xc2\x9b6n", b"\x1b[2;1R"),
        (b"\xc2\x85\xc2\x9b6n", b"\x1b[2;1R"),
        (b"\x1b[5\xc2\xa0n", b""),
        (b"\x1b([6n", b""),
        (b"\x1b[6\xc2\x9cn", b""),
    ];
    for &(input, expected) in cases {
        for chunk_size in [1, 2, 3, 5, input.len()] {
            let mut terminal = MoshTerminal::new(120, 40, 0).unwrap();
            let mut replies = Vec::new();
            for chunk in input.chunks(chunk_size) {
                replies.extend(terminal.write_with_replies(chunk));
            }
            assert_eq!(replies, expected, "{input:?}, chunk size {chunk_size}");
        }
    }
}

#[test]
#[serial_test::serial]
fn cancellation_controls_never_become_printable_cells() {
    let mut controls = vec![vec![0x18], vec![0x1a]];
    for c1 in [
        0x80, 0x81, 0x82, 0x83, 0x86, 0x87, 0x89, 0x8a, 0x8b, 0x8c, 0x8e, 0x8f, 0x91, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x99, 0x9a,
    ] {
        controls.push(vec![0xc2, c1]);
    }
    for control in controls {
        let mut input = vec![b'x'];
        input.extend(&control);
        input.extend_from_slice(b"y\x1b[6n");
        for size in [1, 2, input.len()] {
            let mut terminal = MoshTerminal::new(20, 3, 0).unwrap();
            let mut replies = Vec::new();
            for chunk in input.chunks(size) {
                replies.extend(terminal.write_with_replies(chunk));
            }
            assert_eq!(replies, b"\x1b[1;3R", "{control:?}, chunk size {size}");
            let screen = terminal.snapshot().unwrap();
            assert_eq!(screen.rows[0][0].text, "x");
            assert_eq!(screen.rows[0][1].text, "y");
            let display = morsh_terminal::DisplayDiff::full_redraw(&screen);
            assert!(!display.contains('\x18') && !display.contains('\x1a'));
        }
    }
}
