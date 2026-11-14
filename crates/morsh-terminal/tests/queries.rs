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
