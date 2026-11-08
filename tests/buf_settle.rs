//! Under a memory budget that is used up, large vectors go to the file, but
//! those grown in `on_heap` stay on the heap until they are settled. Its own
//! test program, since the budget is the process's.

use manby::buf::{Buf, on_heap, set_budget};

#[test]
fn grown_on_heap_and_settled_to_file() {
    // past 128 KB of large heap vectors, new ones go to the file
    set_budget(1 << 20, &std::env::temp_dir());
    let fill = |b: &mut Buf<u64>| {
        for i in 0..(1u64 << 16) {
            b.push(i);
        }
    };

    let mut direct: Buf<u64> = Buf::new();
    fill(&mut direct);
    assert!(direct.in_file());

    let mut built: Buf<u64> = Buf::new();
    on_heap(|| fill(&mut built));
    assert!(!built.in_file());
    built.settle();
    assert!(built.in_file());
    assert_eq!(built.len(), 1 << 16);
    assert!(built.iter().enumerate().all(|(i, &v)| v == i as u64));

    // small vectors and a budget with room stay where they are
    let mut small: Buf<u64> = Buf::new();
    small.push(1);
    small.settle();
    assert!(!small.in_file());
}
