//! Fixed-work benchmarks of the client's per-keystroke and per-frame work,
//! without a benchmark framework. Times are informational, never gates.
//!
//! Output: name,samples,iterations,units,median_ns,p90_ns,units_per_sec

use morsh_prediction::{DisplayPreference, NotificationEngine, PredictionEngine};
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::{hint::black_box, time::Instant};

const COLS: u16 = 200;
const ROWS: u16 = 60;
const TYPED: &str = "the quick brown fox jumps over the lazy dog";

fn measure(name: &str, units: usize, iterations: usize, mut work: impl FnMut()) {
    if std::env::var("MORSH_BENCH_FILTER").is_ok_and(|filter| !name.contains(&filter)) {
        return;
    }
    for _ in 0..5 {
        work();
    }
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..iterations {
            work();
        }
        samples.push(start.elapsed().as_nanos() as f64 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name},21,{iterations},{units},{:.0},{:.0},{:.2}",
        samples[10],
        samples[19],
        units as f64 * 1e9 / samples[10]
    );
}

/// A full screen of text with a shell prompt on the cursor row.
fn busy_screen() -> ScreenSnapshot {
    let mut screen = ScreenSnapshot::new(COLS, ROWS);
    for (y, row) in screen.rows.iter_mut().enumerate() {
        let line = format!("{y:>3} lorem ipsum dolor sit amet, consectetur adipiscing elit ");
        for (cell, ch) in row.iter_mut().zip(line.chars().cycle()) {
            cell.text = ch.to_string();
        }
    }
    let prompt_row = ROWS as usize - 1;
    for (x, cell) in screen.rows[prompt_row].iter_mut().enumerate() {
        cell.text = if x < 2 { "$".into() } else { String::new() };
    }
    screen.cursor_y = prompt_row as u16;
    screen.cursor_x = 2;
    screen
}

/// An engine that has predicted `TYPED` on the prompt row and is waiting
/// for the server, with underlining on.
fn typing_engine(screen: &ScreenSnapshot) -> PredictionEngine {
    let mut engine = PredictionEngine::new();
    engine.set_display_preference(DisplayPreference::Always);
    engine.set_send_interval(100);
    engine.set_local_frame_sent(1);
    for ch in TYPED.chars() {
        engine.new_user_byte(ch, screen);
    }
    engine
}

fn main() {
    let screen = busy_screen();

    measure("keystroke_predict", TYPED.len(), 50, || {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);
        for ch in TYPED.chars() {
            engine.new_user_byte(ch, &screen);
        }
        black_box(&engine);
    });

    let engine = typing_engine(&screen);
    measure("frame_apply_pending", 1, 200, || {
        let mut frame = screen.clone();
        engine.apply(&mut frame);
        black_box(&frame);
    });

    // What the client does on a wakeup where nothing has changed: rebuild the
    // target frame and diff it against what the user already sees.
    let idle = PredictionEngine::new();
    let mut notifications = NotificationEngine::new();
    let mut shown = screen.clone();
    idle.apply(&mut shown);
    notifications.apply(&mut shown);
    measure("frame_idle", 1, 200, || {
        let mut target = screen.clone();
        idle.apply(&mut target);
        notifications.adjust_message();
        notifications.apply(&mut target);
        black_box(DisplayDiff::diff(&shown, &target));
    });

    let typing = typing_engine(&screen);
    let mut shown = screen.clone();
    typing.apply(&mut shown);
    measure("frame_idle_with_predictions", 1, 200, || {
        let mut target = screen.clone();
        typing.apply(&mut target);
        notifications.apply(&mut target);
        black_box(DisplayDiff::diff(&shown, &target));
    });
}
