use morsh_terminal::MoshTerminal;

/// Minimized from a host-output workload that aborted in ref_counted_set.zig.
/// Ghostty's region-scroll path moves a tracked cursor pin between pages
/// without moving its page-local style reference. The next cursorDown then
/// releases the style from the wrong page. Native source is pinned at fdbf9ff3a.
#[test]
fn resizing_then_scrolling_preserves_the_cursor_style_reference() {
    let mut terminal = MoshTerminal::new(3000, 4, 0).unwrap();
    terminal.write(b"\x1b[;2r\x1b[2H\n\n\n\n\n\n\n\n\n\n\n");
    terminal.resize(3000, 5).unwrap();
    terminal.write(b"\x1b[1m\x1b[3r\x1b[5H\n");
    terminal.write(b"x");
    let snapshot = terminal.snapshot().unwrap();
    assert_eq!(snapshot.rows[4][0].text, "x");
    assert!(snapshot.rows[4][0].style.bold);
}
