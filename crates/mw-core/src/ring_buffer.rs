//! 固定容量のロックフリー SPSC リングバッファ。
//!
//! `Producer` と `Consumer` はそれぞれ 1 つのスレッドだけから使う。構築後に
//! バッファが再確保されることはなく、`push`/`pop`/チャンク操作はロックを取得しない。
//! 容量は次の 2 のべきへ切り上げられる。

use std::cell::Cell;
use std::mem::MaybeUninit;

#[cfg(all(loom, test))]
use loom::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(all(loom, test)))]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(all(loom, test))]
use loom::sync::Arc;
#[cfg(not(all(loom, test)))]
use std::sync::Arc;

/// 要素を追加できないときのエラー。
#[derive(Debug, PartialEq, Eq)]
pub enum PushError<T> {
    /// リングバッファが満杯。要素は返される。
    Full(T),
}

/// 要素を取り出せないときのエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopError {
    /// リングバッファが空。
    Empty,
}

/// チャンク操作で要求した要素数を確保できないときのエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkError {
    /// 現在利用できる要素数。
    TooFewSlots(usize),
}

/// 生産側と消費側が共有する記憶領域。
struct Inner<T> {
    /// 消費側が進める読み出し位置。
    head: AtomicUsize,
    /// 生産側が進める書き込み位置。
    tail: AtomicUsize,
    /// 論理的な容量。必ず 2 のべきで 1 以上。
    capacity: usize,
    /// 各要素の記憶領域。初期化済みかどうかは head/tail の所有権で管理する。
    data: Box<[std::cell::UnsafeCell<MaybeUninit<T>>]>,
}

// SAFETY: 各スロットは SPSC の所有権プロトコルにより、一度に生産側または
// 消費側のどちらか一方だけがアクセスする。要素をスレッド間で移動するため T: Send
// を要求する。
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    fn slot(&self, position: usize) -> *mut T {
        let index = position & (self.capacity - 1);
        // SAFETY: `index` は `capacity` 未満であり、`data` は同じ長さで構築される。
        // SPSC の所有権プロトコルにより、呼び出し側はこのスロットへのアクセス権を持つ。
        unsafe { (*self.data.get_unchecked(index).get()).as_mut_ptr() }
    }

    fn available_from(&self, head: usize, tail: usize) -> usize {
        tail.wrapping_sub(head)
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        let mut position = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        let pending = tail.wrapping_sub(position);

        // 最後の Producer/Consumer が破棄される時点では、他方の側も既に存在しない。
        // したがって、この時点で head..tail のスロットを順に破棄できる。
        for _ in 0..pending {
            // SAFETY: head..tail は生産側が公開済みで消費側が未回収の要素だけを表す。
            unsafe { self.slot(position).drop_in_place() };
            position = position.wrapping_add(1);
        }
    }
}

/// 固定容量 SPSC リングバッファの生成口。
pub struct RingBuffer<T>(std::marker::PhantomData<T>);

impl<T> RingBuffer<T> {
    /// バッファを構築し、生産側と消費側を返す。
    ///
    /// 要求容量は 1 以上の次の 2 のべきへ切り上げる。0 は最小容量 1 として扱う。
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new(requested_capacity: usize) -> (Producer<T>, Consumer<T>) {
        let capacity = requested_capacity
            .max(1)
            .checked_next_power_of_two()
            .expect("ring buffer capacity is too large");
        assert!(
            capacity <= usize::MAX / 2,
            "ring buffer capacity must fit in half of usize"
        );

        let mut data = Vec::with_capacity(capacity);
        data.resize_with(capacity, || {
            std::cell::UnsafeCell::new(MaybeUninit::uninit())
        });
        let inner = Arc::new(Inner {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            capacity,
            data: data.into_boxed_slice(),
        });

        let producer = Producer {
            inner: Arc::clone(&inner),
            cached_head: Cell::new(0),
            cached_tail: Cell::new(0),
        };
        let consumer = Consumer {
            inner,
            cached_head: Cell::new(0),
            cached_tail: Cell::new(0),
        };
        (producer, consumer)
    }
}

/// リングバッファへ書き込む側。
pub struct Producer<T> {
    inner: Arc<Inner<T>>,
    cached_head: Cell<usize>,
    cached_tail: Cell<usize>,
}

// SAFETY: Producer は SPSC の生産側であり、移動後は 1 スレッドからだけアクセスされる。
// Cell の内容もそのスレッドだけが変更する。
unsafe impl<T: Send> Send for Producer<T> {}

impl<T> Producer<T> {
    /// 現在書き込める要素数を返す。
    pub fn slots(&self) -> usize {
        let head = self.inner.head.load(Ordering::Acquire);
        self.cached_head.set(head);
        self.inner
            .capacity
            .saturating_sub(self.inner.available_from(head, self.cached_tail.get()))
    }

    /// バッファ容量を返す。
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// 要素を 1 つ追加する。
    pub fn push(&mut self, value: T) -> Result<(), PushError<T>> {
        let tail = self.cached_tail.get();
        let mut head = self.cached_head.get();
        if self.inner.available_from(head, tail) >= self.inner.capacity {
            head = self.inner.head.load(Ordering::Acquire);
            self.cached_head.set(head);
            if self.inner.available_from(head, tail) >= self.inner.capacity {
                return Err(PushError::Full(value));
            }
        }

        // SAFETY: head/tail の所有権計算により、tail が指すスロットは空いている。
        unsafe { self.inner.slot(tail).write(value) };
        let next_tail = tail.wrapping_add(1);
        self.inner.tail.store(next_tail, Ordering::Release);
        self.cached_tail.set(next_tail);
        Ok(())
    }

    /// `n` 個の初期化済み書き込みスロットを予約する。
    pub fn write_chunk(&mut self, n: usize) -> Result<WriteChunk<'_, T>, ChunkError>
    where
        T: Default,
    {
        let available = self.available_slots(n);
        if available < n {
            return Err(ChunkError::TooFewSlots(available));
        }

        let start = self.cached_tail.get();
        let first_index = start & (self.inner.capacity - 1);
        let first_len = n.min(self.inner.capacity - first_index);
        let second_len = n - first_len;
        let first_ptr = self.inner.slot(start);
        let second_ptr = self.inner.slot(start.wrapping_add(first_len));

        for offset in 0..first_len {
            // SAFETY: write_chunk が予約した未使用スロットである。
            unsafe { first_ptr.add(offset).write(T::default()) };
        }
        for offset in 0..second_len {
            // SAFETY: write_chunk が予約した未使用スロットである。
            unsafe { second_ptr.add(offset).write(T::default()) };
        }

        Ok(WriteChunk {
            producer: self,
            first_ptr,
            first_len,
            second_ptr,
            second_len,
            committed: false,
        })
    }

    fn available_slots(&self, requested: usize) -> usize {
        let tail = self.cached_tail.get();
        let mut head = self.cached_head.get();
        let mut available = self
            .inner
            .capacity
            .saturating_sub(self.inner.available_from(head, tail));
        if available < requested {
            head = self.inner.head.load(Ordering::Acquire);
            self.cached_head.set(head);
            available = self
                .inner
                .capacity
                .saturating_sub(self.inner.available_from(head, tail));
        }
        available
    }

    fn commit(&mut self, count: usize) {
        let next_tail = self.cached_tail.get().wrapping_add(count);
        self.inner.tail.store(next_tail, Ordering::Release);
        self.cached_tail.set(next_tail);
    }
}

/// 生産側が予約した初期化済み書き込み領域。
pub struct WriteChunk<'a, T> {
    producer: &'a mut Producer<T>,
    first_ptr: *mut T,
    first_len: usize,
    second_ptr: *mut T,
    second_len: usize,
    committed: bool,
}

// SAFETY: WriteChunk は Producer への排他的な借用を保持するため、Producer と同じ
// スレッドでのみ使われる。Producer をスレッド間で移動できる条件 T: Send を引き継ぐ。
unsafe impl<T: Send> Send for WriteChunk<'_, T> {}

impl<T> WriteChunk<'_, T> {
    /// 予約領域を 2 つの連続したスライスとして返す。
    pub fn as_mut_slices(&mut self) -> (&mut [T], &mut [T]) {
        // SAFETY: write_chunk で全要素を Default::default() で初期化済みであり、
        // 各ポインタと長さはリング内の予約領域を表す。WriteChunk が Producer を
        // 排他的に借用しているため、同じ領域への別の可変参照は存在しない。
        unsafe {
            (
                std::slice::from_raw_parts_mut(self.first_ptr, self.first_len),
                std::slice::from_raw_parts_mut(self.second_ptr, self.second_len),
            )
        }
    }

    /// 先頭 `n` 個だけを公開し、残りを破棄する。
    pub fn commit(mut self, n: usize) {
        assert!(n <= self.len(), "cannot commit more than chunk size");
        self.drop_suffix(n);
        self.producer.commit(n);
        self.committed = true;
    }

    /// 予約した全要素を公開する。
    pub fn commit_all(mut self) {
        let len = self.len();
        self.producer.commit(len);
        self.committed = true;
    }

    /// 予約した要素数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.first_len + self.second_len
    }

    /// 予約領域が空か返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn drop_suffix(&mut self, committed: usize) {
        for offset in committed..self.first_len {
            // SAFETY: write_chunk が全領域を初期化済みで、未公開領域はまだ consumer
            // から読まれない。
            unsafe { self.first_ptr.add(offset).drop_in_place() };
        }
        let second_start = committed.saturating_sub(self.first_len);
        for offset in second_start..self.second_len {
            // SAFETY: write_chunk が全領域を初期化済みで、未公開領域はまだ consumer
            // から読まれない。
            unsafe { self.second_ptr.add(offset).drop_in_place() };
        }
    }
}

impl<T> Drop for WriteChunk<'_, T> {
    fn drop(&mut self) {
        if !self.committed {
            self.drop_suffix(0);
        }
    }
}

/// リングバッファから読み出す側。
pub struct Consumer<T> {
    inner: Arc<Inner<T>>,
    cached_head: Cell<usize>,
    cached_tail: Cell<usize>,
}

// SAFETY: Consumer は SPSC の消費側であり、移動後は 1 スレッドからだけアクセスされる。
// Cell の内容もそのスレッドだけが変更する。
unsafe impl<T: Send> Send for Consumer<T> {}

impl<T> Consumer<T> {
    /// 現在読み出せる要素数を返す。
    pub fn slots(&self) -> usize {
        let tail = self.inner.tail.load(Ordering::Acquire);
        self.cached_tail.set(tail);
        self.inner.available_from(self.cached_head.get(), tail)
    }

    /// バッファ容量を返す。
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// 要素を 1 つ取り出す。
    pub fn pop(&mut self) -> Result<T, PopError> {
        let head = self.cached_head.get();
        let mut tail = self.cached_tail.get();
        if head == tail {
            tail = self.inner.tail.load(Ordering::Acquire);
            self.cached_tail.set(tail);
            if head == tail {
                return Err(PopError::Empty);
            }
        }

        // SAFETY: tail の Release 公開により、この head のスロットは初期化済みで、
        // consumer だけが所有している。
        let value = unsafe { self.inner.slot(head).read() };
        let next_head = head.wrapping_add(1);
        self.inner.head.store(next_head, Ordering::Release);
        self.cached_head.set(next_head);
        Ok(value)
    }

    /// `n` 個の読み出し領域を予約する。
    pub fn read_chunk(&mut self, n: usize) -> Result<ReadChunk<'_, T>, ChunkError> {
        let available = self.available_slots(n);
        if available < n {
            return Err(ChunkError::TooFewSlots(available));
        }

        let start = self.cached_head.get();
        let first_index = start & (self.inner.capacity - 1);
        let first_len = n.min(self.inner.capacity - first_index);
        let second_len = n - first_len;
        let first_ptr = self.inner.slot(start);
        let second_ptr = self.inner.slot(start.wrapping_add(first_len));

        Ok(ReadChunk {
            consumer: self,
            first_ptr,
            first_len,
            second_ptr,
            second_len,
        })
    }

    fn available_slots(&self, requested: usize) -> usize {
        let head = self.cached_head.get();
        let mut tail = self.cached_tail.get();
        let mut available = self.inner.available_from(head, tail);
        if available < requested {
            tail = self.inner.tail.load(Ordering::Acquire);
            self.cached_tail.set(tail);
            available = self.inner.available_from(head, tail);
        }
        available
    }

    fn commit(&mut self, count: usize) {
        let next_head = self.cached_head.get().wrapping_add(count);
        self.inner.head.store(next_head, Ordering::Release);
        self.cached_head.set(next_head);
    }
}

/// 消費側が予約した読み出し領域。
pub struct ReadChunk<'a, T> {
    consumer: &'a mut Consumer<T>,
    first_ptr: *mut T,
    first_len: usize,
    second_ptr: *mut T,
    second_len: usize,
}

// SAFETY: ReadChunk は Consumer への排他的な借用を保持するため、Consumer と同じ
// スレッドでのみ使われる。Consumer をスレッド間で移動できる条件 T: Send を引き継ぐ。
unsafe impl<T: Send> Send for ReadChunk<'_, T> {}

impl<T> ReadChunk<'_, T> {
    /// 予約領域を 2 つの連続した読み出しスライスとして返す。
    pub fn as_slices(&self) -> (&[T], &[T]) {
        // SAFETY: read_chunk は tail が公開済みの初期化済み領域だけを予約し、
        // ReadChunk が Consumer を排他的に借用しているため、領域は読み出し中に
        // producer から上書きされない。
        unsafe {
            (
                std::slice::from_raw_parts(self.first_ptr, self.first_len),
                std::slice::from_raw_parts(self.second_ptr, self.second_len),
            )
        }
    }

    /// 先頭 `n` 個を破棄して再利用可能にする。
    pub fn commit(self, n: usize) {
        assert!(n <= self.len(), "cannot commit more than chunk size");
        let mut chunk = self;
        chunk.drop_prefix(n);
        chunk.consumer.commit(n);
    }

    /// 予約した全要素を破棄して再利用可能にする。
    pub fn commit_all(self) {
        let len = self.len();
        let mut chunk = self;
        chunk.drop_prefix(len);
        chunk.consumer.commit(len);
    }

    /// 予約した要素数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.first_len + self.second_len
    }

    /// 予約領域が空か返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn drop_prefix(&mut self, count: usize) {
        let first_count = count.min(self.first_len);
        for offset in 0..first_count {
            // SAFETY: tail が公開済みの初期化済み要素で、未だ consumer の所有下にある。
            unsafe { self.first_ptr.add(offset).drop_in_place() };
        }
        let second_count = count.saturating_sub(self.first_len).min(self.second_len);
        for offset in 0..second_count {
            // SAFETY: tail が公開済みの初期化済み要素で、未だ consumer の所有下にある。
            unsafe { self.second_ptr.add(offset).drop_in_place() };
        }
    }
}

impl<T: Copy> Consumer<T> {
    /// 可能な範囲で要素を出力スライスへコピーし、コピー済みと残りを返す。
    pub fn pop_partial_slice<'a>(&mut self, slice: &'a mut [T]) -> (&'a mut [T], &'a mut [T]) {
        let count = self.slots().min(slice.len());
        let chunk = match self.read_chunk(count) {
            Ok(chunk) => chunk,
            Err(_) => return slice.split_at_mut(0),
        };
        let (first, second) = chunk.as_slices();
        let first_len = first.len();
        slice[..first_len].copy_from_slice(first);
        slice[first_len..first_len + second.len()].copy_from_slice(second);
        chunk.commit_all();
        slice.split_at_mut(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

    #[test]
    fn requested_capacity_is_rounded_to_a_power_of_two() {
        let (producer, consumer) = RingBuffer::<u8>::new(3);
        assert_eq!(producer.capacity(), 4);
        assert_eq!(consumer.capacity(), 4);
        let (producer, consumer) = RingBuffer::<u8>::new(0);
        assert_eq!(producer.capacity(), 1);
        assert_eq!(consumer.capacity(), 1);
    }

    #[test]
    fn capacity_boundary_rejects_the_first_overflow_and_accepts_after_pop() {
        let (mut producer, mut consumer) = RingBuffer::new(4);
        for value in 0..4 {
            assert_eq!(producer.push(value), Ok(()));
        }
        assert_eq!(producer.slots(), 0);
        assert_eq!(producer.push(4), Err(PushError::Full(4)));
        assert_eq!(consumer.pop(), Ok(0));
        assert_eq!(producer.push(4), Ok(()));
        assert_eq!(consumer.slots(), 4);
    }

    #[test]
    fn push_and_pop_wrap_without_reordering() {
        let (mut producer, mut consumer) = RingBuffer::new(4);
        for value in 0..3 {
            producer.push(value).unwrap();
        }
        assert_eq!(consumer.pop(), Ok(0));
        assert_eq!(consumer.pop(), Ok(1));
        for value in 3..5 {
            producer.push(value).unwrap();
        }
        for value in 2..5 {
            assert_eq!(consumer.pop(), Ok(value));
        }
        assert_eq!(consumer.pop(), Err(PopError::Empty));
    }

    #[test]
    fn write_chunk_commits_partial_data_and_drops_the_suffix() {
        let (mut producer, mut consumer) = RingBuffer::new(4);
        let dropped = std::sync::Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            producer
                .push(DropMarker(std::sync::Arc::clone(&dropped)))
                .unwrap();
        }
        drop(consumer.pop().unwrap());
        drop(consumer.pop().unwrap());
        {
            let marker = DropMarker(std::sync::Arc::clone(&dropped));
            let mut chunk = producer.write_chunk(3).unwrap();
            let (first, second) = chunk.as_mut_slices();
            first[0] = marker;
            second[0] = DropMarker(std::sync::Arc::clone(&dropped));
            // The third initialized default value is intentionally not committed.
            chunk.commit(2);
        }
        assert_eq!(consumer.slots(), 2);
        assert_eq!(dropped.load(StdOrdering::SeqCst), 3);
        drop(consumer.pop().unwrap());
        drop(consumer.pop().unwrap());
        assert_eq!(dropped.load(StdOrdering::SeqCst), 4);
    }

    #[test]
    fn pop_partial_slice_copies_across_the_wrap_boundary() {
        let (mut producer, mut consumer) = RingBuffer::new(4);
        for value in 0..4 {
            producer.push(value).unwrap();
        }
        assert_eq!(consumer.pop(), Ok(0));
        assert_eq!(consumer.pop(), Ok(1));
        producer.push(4).unwrap();
        producer.push(5).unwrap();

        let mut output = [99; 4];
        let (filled, remainder) = consumer.pop_partial_slice(&mut output);
        assert_eq!(filled, &[2, 3, 4, 5]);
        assert!(remainder.is_empty());
        assert_eq!(consumer.pop(), Err(PopError::Empty));
    }

    #[test]
    fn dropping_the_handles_drops_all_queued_elements_exactly_once() {
        let dropped = std::sync::Arc::new(AtomicUsize::new(0));
        let (mut producer, consumer) = RingBuffer::new(4);
        for _ in 0..3 {
            producer
                .push(DropMarker(std::sync::Arc::clone(&dropped)))
                .unwrap();
        }
        drop(consumer);
        assert_eq!(dropped.load(StdOrdering::SeqCst), 0);
        drop(producer);
        assert_eq!(dropped.load(StdOrdering::SeqCst), 3);
    }

    #[test]
    fn dropping_an_uncommitted_read_chunk_keeps_elements_available() {
        let (mut producer, mut consumer) = RingBuffer::new(2);
        producer.push(10).unwrap();
        {
            let chunk = consumer.read_chunk(1).unwrap();
            assert_eq!(chunk.as_slices().0, &[10]);
        }
        assert_eq!(consumer.pop(), Ok(10));
    }

    #[derive(Debug, Default)]
    struct DropMarker(std::sync::Arc<AtomicUsize>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.fetch_add(1, StdOrdering::SeqCst);
        }
    }

    #[cfg(loom)]
    #[test]
    fn loom_spsc_model_never_loses_or_duplicates_values() {
        loom::model(|| {
            let (producer, consumer) = RingBuffer::new(2);
            let producer_thread = loom::thread::spawn(move || {
                let mut producer = producer;
                for value in 0..3 {
                    loop {
                        if producer.push(value).is_ok() {
                            break;
                        }
                        loom::thread::yield_now();
                    }
                }
            });
            let consumer_thread = loom::thread::spawn(move || {
                let mut consumer = consumer;
                let mut values = Vec::new();
                while values.len() < 3 {
                    match consumer.pop() {
                        Ok(value) => values.push(value),
                        Err(PopError::Empty) => loom::thread::yield_now(),
                    }
                }
                values
            });

            producer_thread.join().unwrap();
            assert_eq!(consumer_thread.join().unwrap(), vec![0, 1, 2]);
        });
    }
}
