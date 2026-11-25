use morsh_network::connection::timestamp_diff;

#[test]
fn signed_timestamp_delta_is_invariant_across_every_rollover_position() {
    // Includes the signed half-range boundary, plus mosh's no-sample sentinel.
    // This deliberately uses no clocks or sleeps.
    for base in 0..=u16::MAX {
        for delta in [i16::MIN, -5000, -1, 0, 1, 5000, i16::MAX] {
            let next = base.wrapping_add(delta as u16);
            assert_eq!(timestamp_diff(next, base), i32::from(delta));
        }
    }
}
