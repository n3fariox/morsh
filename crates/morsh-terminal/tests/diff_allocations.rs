//! Frame diffing must not allocate a String for each old cell.
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;
thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn record_allocation() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn idle_and_sparse_diffs_have_constant_allocation_counts() {
    for (cols, rows) in [(120, 40), (1024, 128)] {
        let mut old = ScreenSnapshot::new(cols, rows);
        for row in &mut old.rows {
            for cell in row {
                cell.text = "x".into();
            }
        }
        let mut new = old.clone();
        for changed in [false, true] {
            if changed {
                new.rows[rows as usize / 2][cols as usize / 2].text = "y".into();
            }
            ALLOCATIONS.with(|count| count.set(Some(0)));
            let diff = DisplayDiff::diff(&old, &new);
            let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
            assert!(
                allocations <= 2,
                "{cols}x{rows}, changed={changed}: {allocations} allocations"
            );
            assert_eq!(diff.is_empty(), !changed);
        }
    }
}
