//! CI gates for low-scale ultrawide monitors with side-by-side tmux panes.
//! Sizes are terminal columns/rows, not physical monitor pixels.
#[path = "../test_support/split_terminal.rs"]
mod split_terminal;

use morsh_network::{constants::*, fragment::Fragmenter, State};
use morsh_proto::transport::Instruction;
use morsh_statesync::Complete;
use morsh_terminal::DisplayDiff;
use split_terminal::{initial_screen, pane_bounds, scroll_pane, update_pane, Side};

const GEOMETRIES: &[(u16, u16)] = &[(320, 80), (512, 100), (513, 100), (1024, 128)];

fn wire_size(diff: Vec<u8>) -> (usize, usize) {
    let instruction = Instruction {
        protocol_version: Some(2),
        old_num: Some(0),
        new_num: Some(1),
        ack_num: Some(0),
        throwaway_num: Some(0),
        diff: Some(diff),
        chaff: Some(vec![0xab; CHAFF_MAX]),
    };
    let fragments = Fragmenter::new()
        .make_fragments(&instruction, max_fragment_payload(MTU_IPV4))
        .unwrap();
    // Exact UDP payload length: serialized fragment + timestamps + nonce + OCB tag.
    let bytes = fragments
        .iter()
        .map(|f| f.to_bytes().len() + ADDED_BYTES + SESSION_ADDED_BYTES)
        .sum();
    (fragments.len(), bytes)
}

#[test]
#[serial_test::serial]
fn pane_redraws_and_local_scrolling_preserve_the_other_half() {
    for &(cols, rows) in GEOMETRIES {
        let mut source = Complete::new(cols, rows).unwrap();
        source.act(&initial_screen(cols, rows)).unwrap();
        let mut receiver = Complete::new(cols, rows).unwrap();
        receiver.apply_string(&source.diff_from(&receiver)).unwrap();
        assert!(receiver.state_equal(&source), "initial {cols}x{rows}");
        for side in [Side::Left, Side::Right] {
            for frame in 1..=3 {
                let before = source.clone();
                let update = update_pane(cols, rows, side, frame);
                source.act(&update).unwrap();
                let old = before.screen();
                let new = source.screen();
                let (start, width) = pane_bounds(cols, side);
                let end = (start + width) as usize;
                for y in 0..rows as usize - 1 {
                    assert_eq!(
                        &new.rows[y][..start as usize],
                        &old.rows[y][..start as usize]
                    );
                    assert_eq!(&new.rows[y][end..], &old.rows[y][end..]);
                    assert_ne!(
                        &new.rows[y][start as usize..end],
                        &old.rows[y][start as usize..end]
                    );
                    if frame % 2 == 1 {
                        assert!(new.rows[y][end - 2].wide, "wide glyph at pane edge");
                        assert!(new.rows[y][end - 1].spacer);
                    }
                }
                assert_eq!(
                    new.rows[rows as usize - 1],
                    old.rows[rows as usize - 1],
                    "status row changed"
                );
                let rendered = DisplayDiff::diff(old, new);
                // Permit cursor/style sequences per row, but reject a full-screen repaint.
                assert!(
                    rendered.len() <= update.len() + rows as usize * 96 + 512,
                    "{cols}x{rows} {side:?}: {} bytes from {} input bytes",
                    rendered.len(),
                    update.len()
                );
                let diff = source.diff_from(&before);
                let state_bytes = diff.len();
                let (packets, udp_bytes) = wire_size(diff.clone());
                receiver.apply_string(&diff).unwrap();
                assert!(
                    receiver.state_equal(&source),
                    "{cols}x{rows} {side:?} frame {frame}"
                );
                println!("{cols}x{rows},{side:?},frame={frame},vt_bytes={},state_bytes={state_bytes},packets={packets},udp_bytes={udp_bytes}", rendered.len());
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn sparse_input_past_column_255_stays_small_and_replays_at_the_right_position() {
    for &(cols, rows) in GEOMETRIES {
        let mut source = Complete::new(cols, rows).unwrap();
        source.act(&initial_screen(cols, rows)).unwrap();
        let mut receiver = Complete::new(cols, rows).unwrap();
        receiver.apply_string(&source.diff_from(&receiver)).unwrap();
        for x in std::collections::BTreeSet::from([255, 256, 511, 512, cols - 1]) {
            if x >= cols || x == cols / 2 - 1 {
                continue;
            } // retain the pane divider
            let before = source.clone();
            source
                .act(format!("\x1b[20;{}HX", x + 1).as_bytes())
                .unwrap();
            assert_eq!(source.screen().cell(x, 19).unwrap().text, "X");
            let diff = source.diff_from(&before);
            assert!(
                diff.len() < 512,
                "{cols}x{rows} x={x}: {} protobuf bytes",
                diff.len()
            );
            let (packets, bytes) = wire_size(diff.clone());
            assert_eq!(packets, 1);
            assert!(bytes < 256, "{cols}x{rows} x={x}: {bytes} UDP bytes");
            receiver.apply_string(&diff).unwrap();
            assert!(receiver.state_equal(&source), "{cols}x{rows} x={x}");
            assert!(source.diff_from(&source).is_empty());
            assert!(DisplayDiff::diff(source.screen(), source.screen()).is_empty());
        }
    }
}

#[test]
#[serial_test::serial]
fn split_panes_resize_between_normal_and_ultrawide_without_stale_columns() {
    let mut source = Complete::new(120, 40).unwrap();
    let mut receiver = Complete::new(120, 40).unwrap();
    for (cols, rows) in [(1024, 128), (320, 80), (513, 100), (120, 40), (1024, 128)] {
        let before = source.clone();
        source.resize(cols, rows).unwrap();
        source.act(&initial_screen(cols, rows)).unwrap();
        receiver.apply_string(&source.diff_from(&before)).unwrap();
        assert_eq!(receiver.dimensions(), (cols, rows));
        assert!(receiver.state_equal(&source), "resize to {cols}x{rows}");
        assert_eq!(source.screen().cell(cols / 2 - 1, 0).unwrap().text, "│");
    }
}

#[test]
#[serial_test::serial]
fn pane_scroll_moves_rows_without_scrolling_the_neighbor_or_status() {
    for &(cols, rows) in GEOMETRIES {
        for side in [Side::Left, Side::Right] {
            let mut source = Complete::new(cols, rows).unwrap();
            source.act(&initial_screen(cols, rows)).unwrap();
            let before = source.clone();
            source.act(&scroll_pane(cols, rows, side)).unwrap();
            let (start, width) = pane_bounds(cols, side);
            let active = start as usize..(start + width) as usize;
            for row in 0..rows as usize - 2 {
                assert_eq!(
                    &source.screen().rows[row][active.clone()],
                    &before.screen().rows[row + 1][active.clone()]
                );
            }
            for row in 0..rows as usize - 1 {
                assert_eq!(
                    &source.screen().rows[row][..active.start],
                    &before.screen().rows[row][..active.start]
                );
                assert_eq!(
                    &source.screen().rows[row][active.end..],
                    &before.screen().rows[row][active.end..]
                );
            }
            assert_eq!(
                source.screen().rows[rows as usize - 1],
                before.screen().rows[rows as usize - 1]
            );
            let mut receiver = before.clone();
            receiver.apply_string(&source.diff_from(&before)).unwrap();
            assert!(
                receiver.state_equal(&source),
                "pane scroll {cols}x{rows} {side:?}"
            );
        }
    }
}
