//! Vectors whose storage may be a temporary file.
//!
//! A run has a memory budget. Large vectors take heap memory until together
//! they hold an eighth of it; past that, a new large vector's storage is a
//! region of an unlinked temporary file, mapped into memory, whose pages
//! the operating system may write out and drop. Whenever the process's
//! resident memory nears the budget, a pager thread maps file regions
//! afresh: that takes their pages out of the process's memory, while the
//! page cache keeps them for the next use as long as the machine has room.

use std::alloc::Layout;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::File;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit, needs_drop, size_of};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, Once};

/// Vectors smaller than this always take heap memory.
const FILE_MIN: usize = 64 << 10;
/// File regions are multiples of this.
const GRANULE: usize = 1 << 16;
const PAGER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

struct Pager {
    budget: AtomicU64,
    /// Whether any file storage is mapped, for a check without the lock.
    mapping: AtomicBool,
    /// Heap bytes held by vectors of at least `FILE_MIN` bytes.
    heap: AtomicU64,
    trims: AtomicU64,
    state: Mutex<PagerState>,
    started: Once,
}

struct PagerState {
    dir: Option<PathBuf>,
    file: Option<File>,
    failed: bool,
    /// The file's end: regions are never reused, and freed ones become holes.
    next: u64,
    /// Mapped regions by address: their offset in the file and length.
    regions: BTreeMap<usize, (u64, usize)>,
    /// Where the next trim starts.
    cursor: usize,
    mapped: u64,
}

static PAGER: Pager = Pager {
    budget: AtomicU64::new(u64::MAX),
    mapping: AtomicBool::new(false),
    heap: AtomicU64::new(0),
    trims: AtomicU64::new(0),
    state: Mutex::new(PagerState {
        dir: None,
        file: None,
        failed: false,
        next: 0,
        regions: BTreeMap::new(),
        cursor: 0,
        mapped: 0,
    }),
    started: Once::new(),
};

/// Sets the memory budget in bytes, and the directory for the file that
/// holds what does not fit. Without a budget nothing goes to a file.
pub fn set_budget(budget: u64, dir: &Path) {
    PAGER.state.lock().unwrap().dir = Some(dir.to_path_buf());
    PAGER.budget.store(budget, Relaxed);
}

/// What the file holds and how often the pager took its pages out of
/// memory: mapped bytes, and trims.
pub fn file_use() -> (u64, u64) {
    (
        PAGER.state.lock().unwrap().mapped,
        PAGER.trims.load(Relaxed),
    )
}

/// Whether anything has gone to the file.
pub fn spilling() -> bool {
    PAGER.mapping.load(Relaxed)
}

thread_local! {
    /// Whether this thread's vectors stay on the heap for now (`on_heap`).
    static ON_HEAP: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with the vectors this thread grows kept on the heap, whatever
/// the budget, for work that builds vectors a little at a time and then
/// settles them (`Buf::settle`): growing a vector in the file maps, copies
/// and unmaps a region at every doubling, under the pager's lock, which is
/// many times slower when threads do it at once.
pub fn on_heap<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ON_HEAP.set(self.0);
        }
    }
    let _restore = Restore(ON_HEAP.replace(true));
    f()
}

fn place_in_file(bytes: usize) -> bool {
    let budget = PAGER.budget.load(Relaxed);
    bytes >= FILE_MIN
        && budget != u64::MAX
        && PAGER.heap.load(Relaxed) + bytes as u64 > budget / 8
        && !ON_HEAP.get()
}

fn file_alloc(bytes: usize) -> Option<NonNull<u8>> {
    let len = bytes.next_multiple_of(GRANULE);
    let mut s = PAGER.state.lock().unwrap();
    if s.failed {
        return None;
    }
    if s.file.is_none() {
        let dir = s.dir.clone().unwrap_or_else(|| PathBuf::from("/tmp"));
        match crate::mem::create_unlinked(&dir, "memory") {
            Ok(f) => s.file = Some(f),
            Err(e) => {
                eprintln!("Warning: keeping everything in memory: {}", e);
                s.failed = true;
                return None;
            }
        }
        PAGER.started.call_once(|| {
            std::thread::spawn(pager);
        });
    }
    let off = s.next;
    let file = s.file.as_ref().unwrap();
    if let Err(e) = file.set_len(off + len as u64) {
        eprintln!("Warning: keeping everything in memory: {}", e);
        s.failed = true;
        return None;
    }
    // SAFETY: a new shared mapping of a range of the file, which now exists.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            off as libc::off_t,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    s.next += len as u64;
    s.mapped += len as u64;
    s.regions.insert(p as usize, (off, len));
    PAGER.mapping.store(true, Relaxed);
    NonNull::new(p as *mut u8)
}

fn file_free(p: *mut u8) {
    let mut s = PAGER.state.lock().unwrap();
    let Some((off, len)) = s.regions.remove(&(p as usize)) else {
        return;
    };
    s.mapped -= len as u64;
    // SAFETY: the region was mapped by file_alloc and nothing uses it now.
    unsafe { libc::munmap(p as *mut libc::c_void, len) };
    if let Some(f) = &s.file {
        punch_hole(f, off, len);
    }
}

/// Gives the disk space of a range of the file back.
fn punch_hole(f: &File, off: u64, len: usize) {
    #[cfg(target_os = "macos")]
    {
        let arg = libc::fpunchhole_t {
            fp_flags: 0,
            reserved: 0,
            fp_offset: off as libc::off_t,
            fp_length: len as libc::off_t,
        };
        // SAFETY: arg is the fpunchhole_t that F_PUNCHHOLE takes.
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PUNCHHOLE, &arg) };
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: fallocate on an open file descriptor.
        unsafe {
            libc::fallocate(
                f.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                off as libc::off_t,
                len as libc::off_t,
            )
        };
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = (f, off, len);
}

fn pager() {
    loop {
        std::thread::sleep(PAGER_INTERVAL);
        relieve();
    }
}

/// Takes pages of the file storage out of memory if resident memory is
/// near the budget, until it is down to half. The pager does this every
/// few milliseconds; code that writes much at once calls it as it goes, so
/// as not to outrun the pager, and waits for it when over the budget.
pub fn relieve() {
    if !PAGER.mapping.load(Relaxed) {
        return;
    }
    let budget = PAGER.budget.load(Relaxed);
    let Some(r) = crate::mem::resident() else {
        return;
    };
    if r <= budget / 8 * 7 {
        return;
    }
    let state = if r > budget {
        Some(PAGER.state.lock().unwrap())
    } else {
        PAGER.state.try_lock().ok()
    };
    if let Some(mut s) = state
        && crate::mem::resident().is_some_and(|r| r > budget / 8 * 7)
    {
        trim_locked(&mut s, budget / 2);
    }
}

/// Takes the pages of all the file storage out of the process's memory.
pub fn trim() {
    trim_locked(&mut PAGER.state.lock().unwrap(), 0);
}

/// Maps file regions afresh, going round them from where the last trim
/// stopped, until resident memory is down to `target`.
fn trim_locked(s: &mut PagerState, target: u64) {
    let Some(f) = &s.file else { return };
    let from = s.cursor;
    let mut last = None;
    for (k, (&addr, &(off, len))) in s
        .regions
        .range(from..)
        .chain(s.regions.range(..from))
        .enumerate()
    {
        // SAFETY: the range is a live mapping of the same range of the file,
        // which this replaces at once while the state is locked. The pages
        // are the file's, so the contents stay as they are.
        unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                f.as_raw_fd(),
                off as libc::off_t,
            )
        };
        last = Some(addr);
        if target > 0 && k % 4 == 3 && crate::mem::resident().is_some_and(|r| r <= target) {
            break;
        }
    }
    if let Some(a) = last {
        s.cursor = a + 1;
    }
    PAGER.trims.fetch_add(1, Relaxed);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Heap,
    /// Heap memory counted against the budget.
    BigHeap,
    File,
}

/// A vector like `Vec`, whose storage may be a temporary file when it is
/// large and the budget is used up. Only types without drop glue go to a
/// file.
pub struct Buf<T> {
    ptr: NonNull<T>,
    len: usize,
    cap: usize,
    kind: Kind,
    _owns: PhantomData<T>,
}

// SAFETY: a Buf owns its elements as a Vec does.
unsafe impl<T: Send> Send for Buf<T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for Buf<T> {}

impl<T> Buf<T> {
    pub const fn new() -> Buf<T> {
        Buf {
            ptr: NonNull::dangling(),
            len: 0,
            cap: 0,
            kind: Kind::Heap,
            _owns: PhantomData,
        }
    }

    pub fn with_capacity(cap: usize) -> Buf<T> {
        let mut b = Buf::new();
        b.set_capacity(cap);
        b
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Whether the storage is a temporary file.
    pub fn in_file(&self) -> bool {
        self.kind == Kind::File
    }

    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first len elements are initialized.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as above, and self is borrowed mutably.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn reserve(&mut self, additional: usize) {
        let need = self.len.checked_add(additional).expect("capacity overflow");
        if need > self.cap {
            let min = if size_of::<T>() == 1 { 8 } else { 4 };
            self.set_capacity(need.max(self.cap * 2).max(min));
        }
    }

    pub fn reserve_exact(&mut self, additional: usize) {
        let need = self.len.checked_add(additional).expect("capacity overflow");
        if need > self.cap {
            self.set_capacity(need);
        }
    }

    /// Gives back heap memory beyond the length. Storage in the file stays:
    /// its unused end takes no memory, and moving it would copy it all.
    pub fn shrink_to_fit(&mut self) {
        if self.cap > self.len && self.kind != Kind::File {
            self.set_capacity(self.len);
        }
    }

    /// For a vector that is done growing: gives back heap memory beyond the
    /// length, and moves heap storage to the file if the budget says so.
    pub fn settle(&mut self) {
        if self.kind == Kind::File {
            return;
        }
        if self.cap > self.len || (!needs_drop::<T>() && place_in_file(self.len * size_of::<T>())) {
            self.set_capacity(self.len);
        }
    }

    #[inline]
    pub fn push(&mut self, v: T) {
        if self.len == self.cap {
            self.reserve(1);
        }
        // SAFETY: len < cap.
        unsafe { self.ptr.as_ptr().add(self.len).write(v) };
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: element len was initialized and is no longer counted.
        Some(unsafe { self.ptr.as_ptr().add(self.len).read() })
    }

    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let tail = std::ptr::slice_from_raw_parts_mut(
            // SAFETY: len < self.len.
            unsafe { self.ptr.as_ptr().add(len) },
            self.len - len,
        );
        self.len = len;
        // SAFETY: the tail was initialized and is no longer counted.
        unsafe { std::ptr::drop_in_place(tail) };
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<T>] {
        // SAFETY: the storage holds cap elements, of which len are in use.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr.as_ptr().add(self.len) as *mut MaybeUninit<T>,
                self.cap - self.len,
            )
        }
    }

    /// # Safety
    ///
    /// As `Vec::set_len`: `len <= capacity()`, and the elements up to `len`
    /// are initialized.
    pub unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.cap);
        self.len = len;
    }

    /// The vector as a `Vec`, without a copy if its storage is the heap.
    pub fn into_vec(self) -> Vec<T> {
        let mut b = ManuallyDrop::new(self);
        match b.kind {
            Kind::File => {
                let mut v = Vec::with_capacity(b.len);
                // SAFETY: the elements move into v; b's storage is freed
                // without dropping them.
                unsafe {
                    std::ptr::copy_nonoverlapping(b.ptr.as_ptr(), v.as_mut_ptr(), b.len);
                    v.set_len(b.len);
                }
                release::<T>(b.ptr, b.cap, b.kind);
                b.cap = 0;
                v
            }
            kind => {
                if kind == Kind::BigHeap {
                    PAGER.heap.fetch_sub(b.bytes(b.cap) as u64, Relaxed);
                }
                // SAFETY: heap storage comes from the global allocator with
                // Layout::array::<T>(cap), as a Vec's does.
                unsafe { Vec::from_raw_parts(b.ptr.as_ptr(), b.len, b.cap) }
            }
        }
    }

    fn bytes(&self, cap: usize) -> usize {
        cap * size_of::<T>()
    }

    fn set_capacity(&mut self, cap: usize) {
        debug_assert!(cap >= self.len);
        let size = size_of::<T>();
        if size == 0 {
            self.cap = usize::MAX;
            return;
        }
        let bytes = cap.checked_mul(size).expect("capacity overflow");
        let to_file = !needs_drop::<T>() && place_in_file(bytes);
        if !to_file && self.cap > 0 && self.kind != Kind::File && cap > 0 {
            let old = Layout::array::<T>(self.cap).unwrap();
            // SAFETY: the storage was allocated with layout old, and the new
            // size is not zero.
            let p = unsafe { std::alloc::realloc(self.ptr.as_ptr() as *mut u8, old, bytes) };
            let Some(p) = NonNull::new(p as *mut T) else {
                std::alloc::handle_alloc_error(Layout::array::<T>(cap).unwrap());
            };
            if self.kind == Kind::BigHeap {
                PAGER.heap.fetch_sub(self.bytes(self.cap) as u64, Relaxed);
            }
            self.kind = count_heap(bytes);
            self.ptr = p;
            self.cap = cap;
            return;
        }
        let (p, kind) = allocate::<T>(cap, to_file);
        // SAFETY: both hold at least len elements and do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), p.as_ptr(), self.len) };
        release::<T>(self.ptr, self.cap, self.kind);
        self.ptr = p;
        self.cap = cap;
        self.kind = kind;
    }
}

fn count_heap(bytes: usize) -> Kind {
    if bytes >= FILE_MIN {
        PAGER.heap.fetch_add(bytes as u64, Relaxed);
        Kind::BigHeap
    } else {
        Kind::Heap
    }
}

fn allocate<T>(cap: usize, to_file: bool) -> (NonNull<T>, Kind) {
    if cap == 0 {
        return (NonNull::dangling(), Kind::Heap);
    }
    let bytes = cap * size_of::<T>();
    if to_file && let Some(p) = file_alloc(bytes) {
        return (p.cast(), Kind::File);
    }
    let layout = Layout::array::<T>(cap).unwrap();
    // SAFETY: the layout's size is not zero.
    let p = unsafe { std::alloc::alloc(layout) };
    let Some(p) = NonNull::new(p as *mut T) else {
        std::alloc::handle_alloc_error(layout);
    };
    (p, count_heap(bytes))
}

fn release<T>(ptr: NonNull<T>, cap: usize, kind: Kind) {
    if cap == 0 || size_of::<T>() == 0 {
        return;
    }
    match kind {
        Kind::File => file_free(ptr.as_ptr() as *mut u8),
        Kind::Heap | Kind::BigHeap => {
            let layout = Layout::array::<T>(cap).unwrap();
            if kind == Kind::BigHeap {
                PAGER.heap.fetch_sub(layout.size() as u64, Relaxed);
            }
            // SAFETY: allocated with this layout.
            unsafe { std::alloc::dealloc(ptr.as_ptr() as *mut u8, layout) };
        }
    }
}

impl<T: Copy> Buf<T> {
    pub fn extend_from_slice(&mut self, s: &[T]) {
        self.reserve(s.len());
        // SAFETY: there is room for s after len, and s is not in it.
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), self.ptr.as_ptr().add(self.len), s.len())
        };
        self.len += s.len();
    }
}

impl<T: Clone> Buf<T> {
    pub fn resize(&mut self, len: usize, v: T) {
        if len <= self.len {
            self.truncate(len);
            return;
        }
        self.reserve_exact(len - self.len);
        while self.len < len {
            self.push(v.clone());
        }
    }
}

impl<T> Drop for Buf<T> {
    fn drop(&mut self) {
        self.clear();
        release(self.ptr, self.cap, self.kind);
    }
}

impl<T> Default for Buf<T> {
    fn default() -> Buf<T> {
        Buf::new()
    }
}

impl<T: Clone> Clone for Buf<T> {
    fn clone(&self) -> Buf<T> {
        let mut b = Buf::with_capacity(self.len);
        for v in self.iter() {
            b.push(v.clone());
        }
        b
    }
}

impl<T> std::ops::Deref for Buf<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> std::ops::DerefMut for Buf<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Buf<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl<T: PartialEq> PartialEq for Buf<T> {
    fn eq(&self, other: &Buf<T>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for Buf<T> {}

impl<T> From<Vec<T>> for Buf<T> {
    /// Takes the vector's storage, or copies it to a file if the budget
    /// says so.
    fn from(v: Vec<T>) -> Buf<T> {
        let mut v = ManuallyDrop::new(v);
        let (ptr, len, cap) = (v.as_mut_ptr(), v.len(), v.capacity());
        if cap == 0 || size_of::<T>() == 0 {
            ManuallyDrop::into_inner(v);
            let mut b = Buf::new();
            b.len = len;
            if size_of::<T>() == 0 {
                b.cap = usize::MAX;
            }
            return b;
        }
        let mut b = Buf {
            // SAFETY: a Vec's pointer is not null.
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            len,
            cap,
            kind: count_heap(cap * size_of::<T>()),
            _owns: PhantomData,
        };
        if !needs_drop::<T>() && place_in_file(len * size_of::<T>()) {
            b.set_capacity(len);
        }
        b
    }
}

impl<T> FromIterator<T> for Buf<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Buf<T> {
        let mut b = Buf::new();
        b.extend(iter);
        b
    }
}

impl<T> Extend<T> for Buf<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for v in iter {
            self.push(v);
        }
    }
}

impl<'a, T> IntoIterator for &'a Buf<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Buf<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_moves_between_heap_and_file() {
        let mut b: Buf<u64> = Buf::new();
        for i in 0..1_000_000u64 {
            b.push(i * 3);
        }
        assert_eq!(b.len(), 1_000_000);
        assert!(b.iter().enumerate().all(|(i, &v)| v == i as u64 * 3));
        let v = b.clone().into_vec();
        assert_eq!(&v[..], &b[..]);
        let c: Buf<u64> = Buf::from(v);
        assert_eq!(c, b);
        b.truncate(10);
        b.shrink_to_fit();
        assert_eq!(&b[..], &[0, 3, 6, 9, 12, 15, 18, 21, 24, 27]);
    }

    #[test]
    fn file_storage_keeps_its_contents_through_trims() {
        let dir = std::env::temp_dir();
        PAGER.state.lock().unwrap().dir = Some(dir.clone());
        let mut b: Buf<u32> = Buf::new();
        let (p, kind) = allocate::<u32>(4 << 20, true);
        b.ptr = p;
        b.cap = 4 << 20;
        b.kind = kind;
        assert!(b.in_file());
        for i in 0..(3 << 19) {
            b.push(i);
        }
        trim();
        assert!(b.iter().enumerate().all(|(i, &v)| v == i as u32));
        b.extend_from_slice(&[7; 1 << 20]);
        assert!(b.in_file());
        assert_eq!(b.len(), (3 << 19) + (1 << 20));
        trim();
        assert_eq!(b[3 << 19], 7);
        assert_eq!(b[(3 << 19) - 1], (3 << 19) - 1);
    }
}
