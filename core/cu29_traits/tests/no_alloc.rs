//! Verifies that the new zero-allocation `CuError` constructors and
//! builders never touch the heap.
//!
//! This runs in its own test binary so the counting global allocator
//! does not interfere with other tests, and all assertions live inside
//! a single `#[test]` function so the cargo test runner does not
//! schedule sibling tests on background threads while we are sampling
//! the process-wide allocation counter.

#![cfg(feature = "std")]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use cu29_traits::CuError;
use std::alloc::{GlobalAlloc, Layout, System};

struct CountingAllocator;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);

// SAFETY: delegates every call straight to the System allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarding caller-supplied layout to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if TRACKING.load(Ordering::Relaxed) {
            DEALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: pointer/layout pair came from `alloc` above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[track_caller]
fn assert_no_alloc<R>(label: &str, f: impl FnOnce() -> R) -> R {
    let allocs_before = ALLOCS.load(Ordering::Relaxed);
    let deallocs_before = DEALLOCS.load(Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let r = f();
    TRACKING.store(false, Ordering::Relaxed);
    let allocs = ALLOCS.load(Ordering::Relaxed) - allocs_before;
    let deallocs = DEALLOCS.load(Ordering::Relaxed) - deallocs_before;
    assert_eq!(
        allocs, 0,
        "{label}: expected zero heap allocations, observed {allocs} (deallocs={deallocs})"
    );
    r
}

#[test]
fn zero_alloc_apis_never_allocate() {
    {
        let err = assert_no_alloc("from_static", || CuError::from_static("zero alloc literal"));
        assert_eq!(err.message(), "zero alloc literal");
    }

    {
        let err = assert_no_alloc("truncated short", || CuError::truncated("short"));
        assert_eq!(err.message(), "short");
    }

    {
        let err = assert_no_alloc("truncated long", || {
            CuError::truncated(
                "this message is intentionally far longer than the inline capacity allows",
            )
        });
        assert!(err.message().len() <= 23);
    }

    {
        let err = assert_no_alloc("from_display", || {
            CuError::from_display(format_args!("bad value {}", 7))
        });
        assert!(err.message().contains("bad value 7"));
    }

    {
        let err = assert_no_alloc("new(idx)", || CuError::new(42));
        assert_eq!(err.message(), "<interned>");
    }

    {
        assert_no_alloc("with_cause_static", || {
            CuError::from_static("op failed").with_cause_static("io error")
        });
    }

    {
        assert_no_alloc("with_cause_truncated", || {
            CuError::from_static("op failed")
                .with_cause_truncated("permission denied for this very specific case")
        });
    }

    {
        assert_no_alloc("with_cause_display", || {
            CuError::from_static("op failed").with_cause_display(format_args!("io errno {}", 13))
        });
    }

    {
        let err = CuError::from_static("zero alloc literal");
        let cloned = assert_no_alloc("clone(Static)", || err.clone());
        assert_eq!(cloned.message(), err.message());
    }

    {
        let err = CuError::truncated("inline buffer copy is free");
        let cloned = assert_no_alloc("clone(Inline)", || err.clone());
        assert_eq!(cloned.message(), err.message());
    }

    {
        let err = CuError::new(7);
        let cloned = assert_no_alloc("clone(Interned)", || err.clone());
        assert_eq!(cloned.message(), err.message());
    }
}
