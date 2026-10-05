//! Allocation-shape regression, not a throughput benchmark. Input is allocated before measurement.

use alloy_primitives::B256;
use reth_filter_maps::{
    BlockInput, LogInput, LogValueStream, LogValueStreamEvent, LogValueStreamItem,
    LogValueStreamTermination, ValueSpaceAnchor, DEFAULT_PARAMS,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    static ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn record(bytes: usize) {
    let _ = ALLOCATED.try_with(|count| {
        if let Some(previous) = count.get() {
            count.set(Some(previous + bytes));
        }
    });
}

// SAFETY: Every allocation and deallocation is delegated unchanged to System. The thread-local
// counter uses a const-initialized Cell, performs no allocation and does not access allocated
// memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: The caller supplies the valid layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The pointer and layout originate from this allocator's System allocation.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: The caller upholds GlobalAlloc's pointer, layout and new-size requirements.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[test]
fn a_large_block_does_not_allocate_expanded_output() {
    let input = BlockInput::new(
        0,
        B256::ZERO,
        (0..30_000).map(|_| LogInput::new(Default::default(), [B256::ZERO; 4])),
    );
    ALLOCATED.with(|count| count.set(Some(0)));
    let mut stream = LogValueStream::new(
        DEFAULT_PARAMS,
        ValueSpaceAnchor::new(0, B256::ZERO, 0),
        [input],
        LogValueStreamTermination::ReachedHead,
    );
    let first = stream.next().unwrap().unwrap();
    let after_first = ALLOCATED.with(|count| count.get().unwrap());
    let mut slots = 0;
    for item in stream.by_ref() {
        if matches!(item.unwrap(), LogValueStreamItem::Event(LogValueStreamEvent::Slot(_))) {
            slots += 1;
        }
    }
    let total = ALLOCATED.with(|count| count.replace(None).unwrap());
    assert!(matches!(first, LogValueStreamItem::Event(LogValueStreamEvent::BlockPointer(_))));
    assert!(slots >= 150_000);
    assert!(after_first < 4096, "first event allocated {after_first} bytes of auxiliary storage");
    assert!(total < 4096, "stream allocated {total} bytes of auxiliary storage");
    eprintln!("30,000 four-topic logs: auxiliary allocation bytes, first event={after_first}, entire stream={total}");
}
