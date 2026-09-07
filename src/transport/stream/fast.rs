//! Optimized asyncio stream reader, protocol, and writer bindings.

use pyo3::exceptions::{PyStopIteration, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyByteArray, PyBytes, PyDict, PyTuple};
use pyo3_async_runtimes::TaskLocals;

use super::buffers::{OwnedReadBuffer, ReadBufferPool};
use super::{PyStreamTransport, task_locals_for_loop};
use crate::bindings::PyLoop;
use crate::python_names;

const DEFAULT_STREAM_LIMIT: usize = 65_536;

/// Single-use awaitable for reads that are already satisfied from the native
/// buffer. Returning a completed asyncio Future here allocates Future state and
/// calls `set_result` for every WebSocket header and payload slice even though no
/// scheduling is required.
#[pyclass(module = "rsloop._loop")]
struct PyImmediateRead {
    value: Option<Py<PyAny>>,
}

#[pymethods]
impl PyImmediateRead {
    fn __await__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let value = self.value.take().unwrap_or_else(|| py.None());
        Err(PyStopIteration::new_err(value))
    }
}

fn asyncio_iscoroutine(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static ISCOROUTINE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(ISCOROUTINE
        .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
            Ok(py.import("asyncio")?.getattr("iscoroutine")?.unbind())
        })?
        .bind(py))
}

/// Create a future on `loop_obj`, skipping the Python-level method dispatch
/// when the loop is a native rsloop instance running on this thread.
fn loop_create_future(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    if let Some(future) = crate::bindings::try_fast_create_future(py, loop_obj)? {
        return Ok(future);
    }
    python_names::call_method0(py, loop_obj.bind(py), python_names::create_future(py))
}

/// Sliding buffer that delays compaction and releases unusually large bursts.
struct ReadBuffer {
    bytes: OwnedReadBuffer,
    start: usize,
    baseline_capacity: usize,
    max_retained_capacity: usize,
}

impl ReadBuffer {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: OwnedReadBuffer::with_capacity(capacity),
            start: 0,
            baseline_capacity: capacity,
            max_retained_capacity: super::tuning::MAX_STREAM_READ_BUFFER_SIZE,
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.bytes.len().saturating_sub(self.start)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn unread(&self) -> &[u8] {
        &self.bytes[self.start..]
    }

    fn extend(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.compact_if_needed();
        self.bytes.reserve(data.len());
        self.bytes.extend_from_slice(data);
    }

    fn extend_owned(&mut self, data: OwnedReadBuffer) -> Option<OwnedReadBuffer> {
        if data.is_empty() {
            return Some(data);
        }
        if self.is_empty() {
            let previous = std::mem::replace(&mut self.bytes, data);
            self.start = 0;
            Some(previous)
        } else {
            self.extend(&data);
            Some(data)
        }
    }

    fn consume(&mut self, n: usize) {
        self.start = self.start.saturating_add(n).min(self.bytes.len());
        self.compact_if_needed();
    }

    #[inline]
    fn consume_all(&mut self) {
        self.start = self.bytes.len();
        self.compact_if_needed();
    }

    fn replace(&mut self, data: &[u8]) {
        self.bytes.clear();
        self.bytes.extend_from_slice(data);
        self.start = 0;
    }

    fn compact_if_needed(&mut self) {
        if self.start == 0 {
            return;
        }
        if self.start == self.bytes.len() {
            if self.bytes.capacity() > self.max_retained_capacity {
                self.bytes = OwnedReadBuffer::with_capacity(self.baseline_capacity);
            } else {
                self.bytes.clear();
            }
            self.start = 0;
            return;
        }
        if self.start >= 4096 && self.start * 2 >= self.bytes.len() {
            let remaining = self.bytes.len() - self.start;
            self.bytes.copy_within(self.start.., 0);
            self.bytes.truncate(remaining);
            self.start = 0;
        }
    }
}

/// Where proptest keeps the seeds of past failures.
///
/// The default puts them in a `proptest-regressions/` directory beside `src/`;
/// these live under `tests/` with the rest of the test material instead. The
/// path is resolved from the source file, not the working directory, and keeps
/// proptest's own per-source-file naming: this file maps to
/// `tests/proptest-regressions/transport/stream/fast.txt`.
#[cfg(test)]
fn regression_file() -> Option<Box<dyn proptest::test_runner::FailurePersistence>> {
    Some(Box::new(
        proptest::test_runner::FileFailurePersistence::SourceParallel("tests/proptest-regressions"),
    ))
}

#[cfg(test)]
mod read_buffer_tests {
    use proptest::prelude::*;

    use super::{OwnedReadBuffer, ReadBuffer, regression_file};

    #[derive(Clone, Debug)]
    enum ReadOperation {
        Extend(Vec<u8>),
        ExtendOwned(Vec<u8>),
        Consume(usize),
        ConsumeAll,
        Replace(Vec<u8>),
    }

    fn read_operation() -> impl Strategy<Value = ReadOperation> {
        let bytes = || prop::collection::vec(any::<u8>(), 0..8192);
        prop_oneof![
            4 => bytes().prop_map(ReadOperation::Extend),
            4 => bytes().prop_map(ReadOperation::ExtendOwned),
            5 => (0_usize..16_384).prop_map(ReadOperation::Consume),
            1 => Just(ReadOperation::ConsumeAll),
            2 => bytes().prop_map(ReadOperation::Replace),
        ]
    }

    #[test]
    fn releases_oversized_allocation_after_consuming_all_data() {
        let mut buffer = ReadBuffer::with_capacity(4096);
        buffer.extend(&vec![0_u8; 2 * 1024 * 1024]);
        assert!(buffer.bytes.capacity() >= 2 * 1024 * 1024);

        buffer.consume_all();

        assert_eq!(buffer.len(), 0);
        assert!(buffer.bytes.capacity() <= 4096);
    }

    #[test]
    fn adopts_owned_data_without_copying_when_empty() {
        let mut buffer = ReadBuffer::with_capacity(4096);
        let data = vec![7_u8; 1024];
        let data_ptr = data.as_ptr();

        let recycled = buffer.extend_owned(OwnedReadBuffer::from_vec(data));

        assert_eq!(buffer.unread().as_ptr(), data_ptr);
        assert_eq!(buffer.unread(), &[7_u8; 1024]);
        assert!(recycled.is_some());
    }

    #[test]
    fn compacts_consumed_prefix_before_appending_more_data() {
        let mut buffer = ReadBuffer::with_capacity(4096);
        let initial = (0_u8..=250).cycle().take(8192).collect::<Vec<_>>();
        buffer.extend(&initial);
        buffer.consume(4096);

        assert_eq!(buffer.start, 0);
        assert_eq!(buffer.unread(), &initial[4096..]);

        buffer.extend(b"tail");
        assert_eq!(&buffer.unread()[..4096], &initial[4096..]);
        assert_eq!(&buffer.unread()[4096..], b"tail");

        buffer.replace(b"replacement");
        assert_eq!(buffer.unread(), b"replacement");
    }

    #[test]
    fn consuming_an_unbounded_count_saturates_at_the_end() {
        let mut buffer = ReadBuffer::with_capacity(0);
        buffer.extend(b"abc");
        buffer.consume(1);
        buffer.consume(usize::MAX);

        assert!(buffer.is_empty());
        assert_eq!(buffer.start, 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            max_shrink_iters: 10_000,
            failure_persistence: regression_file(),
            ..ProptestConfig::default()
        })]

        #[test]
        fn mixed_operations_match_a_vec_model(
            initial_capacity in 0_usize..=4096,
            operations in prop::collection::vec(read_operation(), 1..80),
        ) {
            let mut buffer = ReadBuffer::with_capacity(initial_capacity);
            let mut model = Vec::new();

            for operation in &operations {
                match operation {
                    ReadOperation::Extend(data) => {
                        buffer.extend(data);
                        model.extend_from_slice(data);
                    }
                    ReadOperation::ExtendOwned(data) => {
                        let recycled = buffer.extend_owned(OwnedReadBuffer::from_vec(data.clone()));
                        prop_assert!(recycled.is_some());
                        model.extend_from_slice(data);
                    }
                    ReadOperation::Consume(count) => {
                        buffer.consume(*count);
                        let consumed = (*count).min(model.len());
                        model.drain(..consumed);
                    }
                    ReadOperation::ConsumeAll => {
                        buffer.consume_all();
                        model.clear();
                    }
                    ReadOperation::Replace(data) => {
                        buffer.replace(data);
                        model.clone_from(data);
                    }
                }

                prop_assert_eq!(buffer.unread(), model.as_slice());
                prop_assert_eq!(buffer.len(), model.len());
                prop_assert_eq!(buffer.is_empty(), model.is_empty());
                prop_assert!(buffer.start <= buffer.bytes.len());
                prop_assert_eq!(buffer.bytes.len() - buffer.start, model.len());
                prop_assert!(buffer.bytes.capacity() >= buffer.bytes.len());
                if model.is_empty() {
                    prop_assert_eq!(buffer.start, 0);
                }
            }
        }
    }
}

/// `bytes.find(needle, from)` over a slice.
#[inline]
fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > haystack.len() {
        return None;
    }
    let hay = &haystack[from..];
    let found = match needle {
        [] => Some(0),
        [byte] => memchr::memchr(*byte, hay),
        _ => memchr::memmem::find(hay, needle),
    };
    found.map(|index| index + from)
}

/// Result of one pass of the `readuntil()` scan over the buffered bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UntilScan {
    Found {
        match_start: usize,
        match_end: usize,
    },
    /// No separator yet. Whether that is a wait or an `IncompleteReadError`
    /// depends on EOF, which is why `is_pending` takes it as an argument.
    NeedMore,
    /// The unsearchable prefix passed the stream limit with no separator.
    OverLimit { consumed: usize },
}

impl UntilScan {
    #[inline]
    fn is_pending(&self, eof: bool) -> bool {
        matches!(self, Self::NeedMore) && !eof
    }
}

/// Longest separator kept out of the allocator. Covers `b"\n"`, `b"\r\n"`,
/// `b"\r\n\r\n"` and the usual one-byte delimiters.
const INLINE_SEPARATOR_LEN: usize = 8;

/// The separator list for one `readuntil()`.
///
/// A single short separator covers `readline()` and nearly every
/// `readuntil()`, and this is rebuilt on every call, so that case is stored
/// inline: a heap allocation here would be a noticeable slice of a `readline()`
/// that otherwise costs about 0.2 us.
enum Separators {
    Inline {
        bytes: [u8; INLINE_SEPARATOR_LEN],
        len: usize,
    },
    Heap(Vec<Vec<u8>>),
}

impl Separators {
    fn single(bytes: &[u8]) -> Self {
        if bytes.len() > INLINE_SEPARATOR_LEN {
            return Self::Heap(vec![bytes.to_vec()]);
        }
        let mut inline = [0_u8; INLINE_SEPARATOR_LEN];
        inline[..bytes.len()].copy_from_slice(bytes);
        Self::Inline {
            bytes: inline,
            len: bytes.len(),
        }
    }

    fn from_list(list: Vec<Vec<u8>>) -> Self {
        if let [only] = list.as_slice() {
            return Self::single(only);
        }
        Self::Heap(list)
    }

    /// Order shortest-first, matching `sorted(separator, key=len)`. The sort has
    /// to stay stable so equal-length separators keep their given order.
    fn sort_by_length(&mut self) {
        if let Self::Heap(list) = self {
            list.sort_by_key(Vec::len);
        }
    }

    fn shortest(&self) -> Option<&[u8]> {
        match self {
            Self::Inline { bytes, len } => Some(&bytes[..*len]),
            Self::Heap(list) => list.first().map(Vec::as_slice),
        }
    }

    fn longest_len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => *len,
            Self::Heap(list) => list.last().map_or(0, Vec::len),
        }
    }
}

fn separator_match_replaces(best: Option<(usize, usize)>, candidate_end: usize) -> bool {
    best.is_none_or(|(_, best_end)| candidate_end < best_end)
}

/// Scan state for one `readuntil()` / `readline()` call.
///
/// `offset` is asyncio's: the number of leading buffer bytes already known not
/// to start a separator. Carrying it across feeds is what keeps assembling a
/// long line linear instead of rescanning the whole buffer per chunk.
struct UntilReadState {
    separators: Separators,
    min_seplen: usize,
    max_seplen: usize,
    offset: usize,
    /// `readline()` translates the two error outcomes rather than raising them.
    line_mode: bool,
}

impl UntilReadState {
    fn new(mut separators: Separators, line_mode: bool) -> PyResult<Self> {
        // asyncio sorts shortest-first and keeps a strictly closer match end, so
        // the shortest separator wins when two of them end at the same byte.
        separators.sort_by_length();
        let Some(shortest) = separators.shortest() else {
            return Err(PyValueError::new_err(
                "Separator should contain at least one element",
            ));
        };
        let min_seplen = shortest.len();
        if min_seplen == 0 {
            return Err(PyValueError::new_err(
                "Separator should be at least one-byte string",
            ));
        }
        let max_seplen = separators.longest_len();
        Ok(Self {
            separators,
            min_seplen,
            max_seplen,
            offset: 0,
            line_mode,
        })
    }

    /// One iteration of asyncio's `readuntil` scan loop.
    fn scan(&mut self, buffer: &[u8], limit: usize) -> UntilScan {
        let buflen = buffer.len();
        if buflen.saturating_sub(self.offset) < self.min_seplen {
            return UntilScan::NeedMore;
        }

        let offset = self.offset;
        let mut best: Option<(usize, usize)> = None;
        let mut consider = |separator: &[u8]| {
            let Some(match_start) = find_from(buffer, separator, offset) else {
                return;
            };
            let match_end = match_start + separator.len();
            if separator_match_replaces(best, match_end) {
                best = Some((match_start, match_end));
            }
        };
        match &self.separators {
            Separators::Inline { bytes, len } => consider(&bytes[..*len]),
            Separators::Heap(list) => {
                for separator in list {
                    consider(separator);
                }
            }
        }
        if let Some((match_start, match_end)) = best {
            return UntilScan::Found {
                match_start,
                match_end,
            };
        }

        // Everything but the trailing `max_seplen - 1` bytes is now known not to
        // begin a separator, so the next pass can start there.
        self.offset = (buflen + 1).saturating_sub(self.max_seplen);
        if self.offset > limit {
            return UntilScan::OverLimit {
                consumed: self.offset,
            };
        }
        UntilScan::NeedMore
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        ReadBuffer, Separators, UntilReadState, UntilScan, exact_fill_amount, find_from,
        separator_match_replaces,
    };
    use crate::verification::MAX_BYTES;

    #[kani::proof]
    #[kani::unwind(6)]
    fn merge_read_buffer_consume_saturates_without_overflow() {
        let count: usize = kani::any();
        let mut buffer = ReadBuffer::with_capacity(0);
        buffer.extend(b"abc");
        buffer.consume(1);
        buffer.consume(count);

        match count {
            0 => assert_eq!(buffer.unread(), b"bc"),
            1 => assert_eq!(buffer.unread(), b"c"),
            _ => assert!(buffer.is_empty()),
        }
        assert!(buffer.start <= buffer.bytes.len());
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn merge_find_from_handles_bounded_offsets() {
        let from = usize::from(kani::any::<u8>() % 7);
        let expected = match from {
            0 | 1 => Some(1),
            2 | 3 => Some(3),
            _ => None,
        };
        assert_eq!(find_from(b"ababa", b"ba", from), expected);
    }

    #[kani::proof]
    fn merge_find_from_handles_empty_needle() {
        let from: usize = kani::any();
        assert_eq!(find_from(b"abaca", b"", from), (from <= 5).then_some(from));
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn merge_separator_selection_keeps_the_earliest_end_and_first_tie() {
        const CANDIDATES: usize = 4;
        let active: [bool; CANDIDATES] = kani::any();
        let ends: [usize; CANDIDATES] = kani::any();
        let mut best: Option<(usize, usize)> = None;

        for index in 0..CANDIDATES {
            if active[index] && separator_match_replaces(best, ends[index]) {
                best = Some((index, ends[index]));
            }

            if let Some((best_index, best_end)) = best {
                assert!(best_index <= index);
                assert!(active[best_index]);
                assert_eq!(best_end, ends[best_index]);
                for earlier in 0..=index {
                    if active[earlier] {
                        assert!(best_end <= ends[earlier]);
                        if earlier < best_index {
                            assert!(ends[earlier] > best_end);
                        }
                    }
                }
            } else {
                for earlier in 0..=index {
                    assert!(!active[earlier]);
                }
            }
        }
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn merge_readuntil_fruitless_scan_has_exact_offset() {
        let len: usize = kani::any();
        kani::assume(len <= MAX_BYTES);
        let limit: usize = kani::any();
        let mut state =
            UntilReadState::new(Separators::single(b"zz"), false).expect("non-empty separator");

        let scan = state.scan(&[b'a'; MAX_BYTES][..len], limit);
        let expected_offset = len.saturating_add(1).saturating_sub(2);
        assert_eq!(state.offset, expected_offset);
        match scan {
            UntilScan::OverLimit { consumed } => {
                assert_eq!(consumed, expected_offset);
                assert!(expected_offset > limit);
            }
            UntilScan::NeedMore => assert!(expected_offset <= limit),
            UntilScan::Found { .. } => panic!("separator is absent from the input alphabet"),
        }
    }

    struct FixedReadBuffer {
        bytes: [u8; MAX_BYTES],
        len: usize,
    }

    impl FixedReadBuffer {
        fn new() -> Self {
            Self {
                bytes: [0; MAX_BYTES],
                len: 0,
            }
        }

        fn len(&self) -> usize {
            self.len
        }

        fn unread(&self) -> &[u8] {
            &self.bytes[..self.len]
        }

        fn push(&mut self, byte: u8) {
            assert!(self.len < MAX_BYTES);
            self.bytes[self.len] = byte;
            self.len += 1;
        }

        fn consume(&mut self, amount: usize) {
            let amount = amount.min(self.len);
            for index in amount..self.len {
                self.bytes[index - amount] = self.bytes[index];
            }
            self.len -= amount;
        }
    }

    fn consume_into(
        buffer: &mut FixedReadBuffer,
        observed: &mut [u8; MAX_BYTES],
        observed_len: &mut usize,
        amount: usize,
    ) {
        let amount = amount.min(buffer.len());
        for byte in &buffer.unread()[..amount] {
            observed[*observed_len] = *byte;
            *observed_len += 1;
        }
        buffer.consume(amount);
    }

    fn assert_conservation(
        accepted: &[u8; MAX_BYTES],
        accepted_len: usize,
        observed: &[u8; MAX_BYTES],
        observed_len: usize,
        buffer: &FixedReadBuffer,
    ) {
        assert_eq!(accepted_len, observed_len + buffer.len());
        for index in 0..accepted_len {
            let actual = if index < observed_len {
                observed[index]
            } else {
                buffer.unread()[index - observed_len]
            };
            assert_eq!(actual, accepted[index]);
        }
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn extended_incremental_and_bulk_separator_scans_agree() {
        const SEQUENCE_BYTES: usize = 4;
        let data: [u8; SEQUENCE_BYTES] = kani::any();
        let data_len = usize::from(kani::any::<u8>() % (SEQUENCE_BYTES as u8 + 1));
        let data = &data[..data_len];
        let mut bulk_state =
            UntilReadState::new(Separators::single(b"aba"), false).expect("valid separator");
        let bulk = bulk_state.scan(&data, usize::MAX);

        let mut incremental_state =
            UntilReadState::new(Separators::single(b"aba"), false).expect("valid separator");
        let mut incremental = UntilScan::NeedMore;
        for end in 1..=data.len() {
            incremental = incremental_state.scan(&data[..end], usize::MAX);
            if matches!(incremental, UntilScan::Found { .. }) {
                break;
            }
        }
        assert_eq!(incremental, bulk);
    }

    #[kani::proof]
    #[kani::unwind(10)]
    fn merge_overlapping_separator_chooses_the_earliest_match() {
        let mut state =
            UntilReadState::new(Separators::single(b"aba"), false).expect("valid separator");
        assert_eq!(
            state.scan(b"ababa", usize::MAX),
            UntilScan::Found {
                match_start: 0,
                match_end: 3,
            }
        );
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn extended_fast_reader_operations_conserve_unread_bytes() {
        const OPERATIONS: usize = 4;
        let operations: [u8; OPERATIONS] = kani::any();
        let values: [u8; OPERATIONS] = kani::any();
        let mut accepted = [0_u8; MAX_BYTES];
        let mut accepted_len = 0;
        let mut observed = [0_u8; MAX_BYTES];
        let mut observed_len = 0;
        let mut buffer = FixedReadBuffer::new();
        let mut eof = false;
        let mut connection_lost = false;

        for index in 0..OPERATIONS {
            match operations[index] % 7 {
                0 => {
                    if !eof && !connection_lost && accepted_len < MAX_BYTES {
                        let byte = values[index];
                        buffer.push(byte);
                        accepted[accepted_len] = byte;
                        accepted_len += 1;
                    }
                }
                1 => {
                    if !connection_lost {
                        consume_into(
                            &mut buffer,
                            &mut observed,
                            &mut observed_len,
                            usize::from(values[index] % 4),
                        );
                    }
                }
                2 => {
                    if !connection_lost {
                        let exact = usize::from(values[index] % 3) + 1;
                        if buffer.len() >= exact {
                            consume_into(&mut buffer, &mut observed, &mut observed_len, exact);
                        } else if eof {
                            let remaining = buffer.len();
                            consume_into(&mut buffer, &mut observed, &mut observed_len, remaining);
                        }
                    }
                }
                3 => {
                    if !connection_lost {
                        let mut state = UntilReadState::new(Separators::single(b"aba"), false)
                            .expect("valid separator");
                        match state.scan(buffer.unread(), usize::MAX) {
                            UntilScan::Found { match_end, .. } => consume_into(
                                &mut buffer,
                                &mut observed,
                                &mut observed_len,
                                match_end,
                            ),
                            UntilScan::NeedMore if eof => {
                                let remaining = buffer.len();
                                consume_into(
                                    &mut buffer,
                                    &mut observed,
                                    &mut observed_len,
                                    remaining,
                                );
                            }
                            UntilScan::NeedMore | UntilScan::OverLimit { .. } => {}
                        }
                    }
                }
                4 => eof = true,
                5 => {
                    // Cancelling a pending read must not mutate buffered data.
                    let before_len = buffer.len();
                    assert_eq!(buffer.len(), before_len);
                }
                _ => {
                    // Connection loss resolves the waiter with an exception but
                    // does not duplicate or consume buffered bytes.
                    connection_lost = true;
                }
            }
            assert_conservation(&accepted, accepted_len, &observed, observed_len, &buffer);
        }
    }

    #[kani::proof]
    fn merge_exact_read_fill_range_is_initialized_and_in_bounds() {
        let buffer_len: usize = kani::any();
        let filled: usize = kani::any();
        let expected: usize = kani::any();
        kani::assume(filled <= expected);

        let amount = exact_fill_amount(buffer_len, filled, expected);
        assert!(amount <= buffer_len);
        assert!(amount <= expected - filled);
        assert!(filled.checked_add(amount).is_some());
        assert!(filled + amount <= expected);
    }
}

#[cfg(test)]
mod until_scan_tests {
    use proptest::prelude::*;

    use super::{Separators, UntilReadState, UntilScan, find_from, regression_file};

    fn separators(list: &[&[u8]]) -> Separators {
        Separators::from_list(list.iter().map(|sep| sep.to_vec()).collect())
    }

    fn until_state(list: &[&[u8]]) -> UntilReadState {
        UntilReadState::new(separators(list), false).expect("valid separators")
    }

    #[test]
    fn find_from_matches_bytes_find_semantics() {
        assert_eq!(find_from(b"hello world", b"o", 0), Some(4));
        assert_eq!(find_from(b"hello world", b"o", 5), Some(7));
        assert_eq!(find_from(b"hello world", b"o", 8), None);
        assert_eq!(find_from(b"hello world", b"lo w", 0), Some(3));
        assert_eq!(find_from(b"hello", b"hello world", 0), None);
        assert_eq!(find_from(b"hello", b"l", 99), None);
    }

    #[test]
    fn scan_finds_a_buffered_separator() {
        let mut state = until_state(&[b"\n"]);
        let scan = state.scan(b"line one\nline two", 64);
        assert!(matches!(
            scan,
            UntilScan::Found {
                match_start: 8,
                match_end: 9
            }
        ));
    }

    #[test]
    fn scan_keeps_a_partial_separator_in_range_across_feeds() {
        // "xxA" ends mid-separator, so the next pass must resume at the "A"
        // rather than skipping the whole buffer.
        let mut state = until_state(&[b"ABC"]);
        assert!(matches!(state.scan(b"xxA", 64), UntilScan::NeedMore));
        assert_eq!(state.offset, 1);
        let scan = state.scan(b"xxABCyy", 64);
        assert!(matches!(
            scan,
            UntilScan::Found {
                match_start: 2,
                match_end: 5
            }
        ));
    }

    #[test]
    fn scan_advances_the_offset_so_rescans_stay_linear() {
        let mut state = until_state(&[b"\n"]);
        assert!(matches!(state.scan(b"aaaa", 64), UntilScan::NeedMore));
        assert_eq!(state.offset, 4);
        assert!(matches!(state.scan(b"aaaaaaaa", 64), UntilScan::NeedMore));
        assert_eq!(state.offset, 8);
    }

    #[test]
    fn scan_reports_overrun_only_past_the_limit() {
        let mut state = until_state(&[b"\n"]);
        assert!(matches!(state.scan(b"12345678", 8), UntilScan::NeedMore));

        let mut state = until_state(&[b"\n"]);
        let scan = state.scan(b"123456789", 8);
        assert!(matches!(scan, UntilScan::OverLimit { consumed: 9 }));
    }

    #[test]
    fn scan_breaks_an_end_tie_towards_the_shortest_separator() {
        // asyncio sorts shortest-first and keeps a strictly closer end, so the
        // one-byte separator's later start is what the limit is measured on.
        let mut state = until_state(&[b"ab", b"b"]);
        let scan = state.scan(b"xxab yy", 64);
        assert!(matches!(
            scan,
            UntilScan::Found {
                match_start: 3,
                match_end: 4
            }
        ));
    }

    #[test]
    fn scan_prefers_the_earliest_end_across_separators() {
        let mut state = until_state(&[b"\r\n", b"!"]);
        let scan = state.scan(b"hi!there\r\n", 64);
        assert!(matches!(
            scan,
            UntilScan::Found {
                match_start: 2,
                match_end: 3
            }
        ));
    }

    #[test]
    fn new_rejects_empty_separator_lists_and_empty_separators() {
        assert!(UntilReadState::new(separators(&[]), false).is_err());
        assert!(UntilReadState::new(separators(&[b""]), false).is_err());
        // The shortest is checked, so an empty entry anywhere is rejected.
        assert!(UntilReadState::new(separators(&[b"\n", b""]), false).is_err());
    }

    /// Independent oracle: literal window search, no memchr, no offset skipping.
    fn literal_find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        if needle.is_empty() || needle.len() > haystack.len() {
            return None;
        }
        // `start > end` makes an inclusive range empty, which covers `from`
        // running past the last position a match could start at.
        (from..=haystack.len() - needle.len())
            .find(|&at| &haystack[at..at + needle.len()] == needle)
    }

    fn naive_first_match(data: &[u8], separators: &[Vec<u8>]) -> Option<(usize, usize)> {
        let mut sorted = separators.to_vec();
        sorted.sort_by_key(Vec::len);
        let mut best: Option<(usize, usize)> = None;
        for separator in &sorted {
            if let Some(match_start) = literal_find(data, separator, 0) {
                let match_end = match_start + separator.len();
                if best.is_none_or(|(_, best_end)| match_end < best_end) {
                    best = Some((match_start, match_end));
                }
            }
        }
        best
    }

    fn found_pair(scan: &UntilScan) -> Option<(usize, usize)> {
        match scan {
            UntilScan::Found {
                match_start,
                match_end,
            } => Some((*match_start, *match_end)),
            _ => None,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            max_shrink_iters: 10_000,
            failure_persistence: regression_file(),
            ..ProptestConfig::default()
        })]

        /// Feeding the buffer in pieces has to reach the same match as one pass
        /// over the whole thing. This is what the `offset` skip could break: it
        /// advances past bytes proven not to start a separator, so an off-by-one
        /// there loses a separator that straddles a chunk boundary.
        #[test]
        fn incremental_scanning_agrees_with_a_single_pass(
            data in prop::collection::vec(prop::sample::select(&b"ab\r\n\x00"[..]), 0..400),
            separator_list in prop::collection::vec(
                prop::collection::vec(prop::sample::select(&b"ab\r\n"[..]), 1..4),
                1..4,
            ),
            chunk in 1_usize..13,
        ) {
            let expected = naive_first_match(&data, &separator_list);

            let mut bulk = UntilReadState::new(
                Separators::from_list(separator_list.clone()),
                false,
            ).expect("non-empty separators");
            prop_assert_eq!(found_pair(&bulk.scan(&data, usize::MAX)), expected);

            let mut incremental = UntilReadState::new(
                Separators::from_list(separator_list),
                false,
            ).expect("non-empty separators");
            let mut reached = None;
            let mut filled = 0;
            loop {
                filled = (filled + chunk).min(data.len());
                let scan = incremental.scan(&data[..filled], usize::MAX);
                if let Some(pair) = found_pair(&scan) {
                    reached = Some(pair);
                    break;
                }
                if filled == data.len() {
                    break;
                }
            }
            prop_assert_eq!(reached, expected);
        }

        /// The offset left by a fruitless pass has to be exactly asyncio's
        /// `max(0, buflen + 1 - max_seplen)`. A smaller offset is still safe --
        /// it only rescans -- so `incremental_scanning_agrees_with_a_single_pass`
        /// cannot see the difference. It is observable anyway, because this
        /// offset is what `LimitOverrunError.consumed` reports and what the
        /// limit is compared against.
        #[test]
        fn a_fruitless_scan_leaves_the_exact_asyncio_offset(
            len in 0_usize..200,
            seplen in 1_usize..6,
        ) {
            // A separator drawn from an alphabet the data never uses, so every
            // pass is guaranteed to come up empty.
            let data = vec![b'a'; len];
            let separator = vec![b'z'; seplen];
            let mut state = UntilReadState::new(Separators::single(&separator), false)
                .expect("non-empty separator");

            let scan = state.scan(&data, usize::MAX);
            prop_assert!(matches!(scan, UntilScan::NeedMore));
            let expected = if len < seplen { 0 } else { len + 1 - seplen };
            prop_assert_eq!(state.offset, expected);
        }

        /// `find_from` has to behave like `bytes.find(needle, from)` for every
        /// start offset, including ones past the end of the haystack.
        #[test]
        fn find_from_agrees_with_a_literal_search(
            haystack in prop::collection::vec(prop::sample::select(&b"abc"[..]), 0..80),
            needle in prop::collection::vec(prop::sample::select(&b"abc"[..]), 1..4),
            from in 0_usize..90,
        ) {
            prop_assert_eq!(
                find_from(&haystack, &needle, from),
                literal_find(&haystack, &needle, from)
            );
        }
    }

    #[test]
    fn single_short_separators_stay_out_of_the_allocator() {
        assert!(matches!(
            Separators::single(b"\r\n"),
            Separators::Inline { len: 2, .. }
        ));
        // A one-element tuple collapses to the inline form too.
        assert!(matches!(
            separators(&[b"\n"]),
            Separators::Inline { len: 1, .. }
        ));
        // Anything longer than the inline budget spills, and still scans.
        let long = b"--boundary-marker";
        assert!(matches!(Separators::single(long), Separators::Heap(_)));
        let mut state = until_state(&[long]);
        assert!(matches!(
            state.scan(b"body--boundary-marker!", 64),
            UntilScan::Found {
                match_start: 4,
                match_end: 21
            }
        ));
    }
}

/// How a finished `readuntil()` / `readline()` resolves its awaitable.
enum UntilOutcome {
    Value(Py<PyAny>),
    Exception(Py<PyAny>),
}

/// Collect the separator argument, accepting the tuple form Python 3.13+ takes.
fn extract_separators(separator: &Bound<'_, PyAny>) -> PyResult<Separators> {
    if let Ok(tuple) = separator.cast::<PyTuple>() {
        let mut list = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            list.push(extract_separator_bytes(&item)?);
        }
        return Ok(Separators::from_list(list));
    }
    // The single-separator path is the hot one, and a `bytes` object lends its
    // buffer directly, so it reaches the inline form without allocating.
    if let Ok(bytes) = separator.cast::<PyBytes>() {
        return Ok(Separators::single(bytes.as_bytes()));
    }
    Ok(Separators::single(&separator.extract::<Vec<u8>>()?))
}

fn extract_separator_bytes(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Ok(bytes.as_bytes().to_vec());
    }
    value.extract::<Vec<u8>>()
}

#[derive(Clone)]
enum ReadWaitKind {
    Any(usize),
    Exact(usize),
    All,
    /// The separator search state lives in `ReadWaiter::until`, so cloning this
    /// kind on every feed stays free.
    Until,
}

struct ReadWaiter {
    // A reader has at most one outstanding Python read operation at a time.
    future: Py<PyAny>,
    kind: ReadWaitKind,
    exact: Option<ExactReadAccumulator>,
    until: Option<UntilReadState>,
}

struct ExactReadAccumulator {
    value: Py<PyBytes>,
    filled: usize,
    expected: usize,
}

fn exact_fill_amount(buffer_len: usize, filled: usize, expected: usize) -> usize {
    debug_assert!(filled <= expected);
    buffer_len.min(expected - filled)
}

impl ExactReadAccumulator {
    fn new(py: Python<'_>, expected: usize) -> PyResult<Self> {
        let expected_size = ffi::Py_ssize_t::try_from(expected)
            .map_err(|_| pyo3::exceptions::PyOverflowError::new_err("read size is too large"))?;
        // SAFETY: The object is retained only inside this waiter and is not
        // exposed to Python until all `expected` bytes have been initialized.
        let ptr = unsafe { ffi::PyBytes_FromStringAndSize(core::ptr::null(), expected_size) };
        if ptr.is_null() {
            return Err(PyErr::fetch(py));
        }
        // SAFETY: `ptr` is a newly owned bytes object of the expected concrete type.
        let value = unsafe {
            Bound::<PyAny>::from_owned_ptr(py, ptr)
                .cast_into_unchecked::<PyBytes>()
                .unbind()
        };
        Ok(Self {
            value,
            filled: 0,
            expected,
        })
    }

    fn fill_from(&mut self, buffer: &mut ReadBuffer) {
        let amount = exact_fill_amount(buffer.len(), self.filled, self.expected);
        if amount == 0 {
            return;
        }
        // SAFETY: `value` has exactly `expected` bytes, remains private to
        // this accumulator, and the destination range is disjoint/in-bounds.
        unsafe {
            let destination = ffi::PyBytes_AsString(self.value.as_ptr())
                .cast::<u8>()
                .add(self.filled);
            core::ptr::copy_nonoverlapping(buffer.unread().as_ptr(), destination, amount);
        }
        self.filled += amount;
        buffer.consume(amount);
    }

    fn partial(&self) -> &[u8] {
        // SAFETY: The first `filled` bytes were initialized by `fill_from` and
        // the returned slice is consumed immediately while `self` is alive.
        unsafe {
            core::slice::from_raw_parts(
                ffi::PyBytes_AsString(self.value.as_ptr()).cast::<u8>(),
                self.filled,
            )
        }
    }
}

/// Native buffered reader used by rsloop's optimized streams API.
///
/// The Python surface mirrors the commonly used `asyncio.StreamReader`
/// operations while retaining buffers in Rust. Only one read coroutine may wait
/// at a time, matching asyncio's stream-reader contract.
#[pyclass(module = "rsloop._loop")]
pub struct PyFastStreamReader {
    loop_obj: Py<PyAny>,
    limit: usize,
    buffer: ReadBuffer,
    waiter: Option<ReadWaiter>,
    transport: Py<PyAny>,
    paused: bool,
    eof: bool,
    exception: Option<Py<PyAny>>,
}

impl PyFastStreamReader {
    fn set_future_result_or_ignore_cancelled(
        py: Python<'_>,
        future: &Py<PyAny>,
        value: Py<PyAny>,
    ) -> PyResult<()> {
        let future = future.bind(py);
        match python_names::call_method1(py, future, python_names::set_result(py), value.bind(py)) {
            Ok(_) => Ok(()),
            Err(err) => {
                if python_names::call_method0(py, future, python_names::cancelled(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    fn set_future_exception_or_ignore_cancelled(
        py: Python<'_>,
        future: &Py<PyAny>,
        exc: Py<PyAny>,
    ) -> PyResult<()> {
        let future = future.bind(py);
        match python_names::call_method1(py, future, python_names::set_exception(py), exc.bind(py))
        {
            Ok(_) => Ok(()),
            Err(err) => {
                if python_names::call_method0(py, future, python_names::cancelled(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    fn new_with_loop(py: Python<'_>, loop_obj: Py<PyAny>, limit: usize) -> PyResult<Self> {
        if limit == 0 {
            return Err(PyValueError::new_err("Limit cannot be <= 0"));
        }

        Ok(Self {
            loop_obj,
            limit,
            // The flow-control limit is not an allocation target. Idle
            // streams should retain only a small baseline and grow on data.
            buffer: ReadBuffer::with_capacity(limit.min(4096)),
            waiter: None,
            transport: py.None(),
            paused: false,
            eof: false,
            exception: None,
        })
    }

    #[inline]
    fn create_future(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        loop_create_future(py, &self.loop_obj)
    }

    fn ready_result_awaitable(&self, py: Python<'_>, value: Py<PyAny>) -> PyResult<Py<PyAny>> {
        Ok(Py::new(py, PyImmediateRead { value: Some(value) })?.into_any())
    }

    fn ready_exception_future(&self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let future = self.create_future(py)?;
        python_names::call_method1(
            py,
            future.bind(py),
            python_names::set_exception(py),
            exc.bind(py),
        )?;
        Ok(future)
    }

    #[inline]
    fn bytes_object(py: Python<'_>, data: &[u8]) -> Py<PyAny> {
        PyBytes::new(py, data).unbind().into_any()
    }

    fn unread_bytes_object(&mut self, py: Python<'_>, n: usize) -> Py<PyAny> {
        let len = self.buffer.len().min(n);
        let value = Self::bytes_object(py, &self.buffer.unread()[..len]);
        self.buffer.consume(len);
        value
    }

    fn unread_all_bytes_object(&mut self, py: Python<'_>) -> Py<PyAny> {
        let value = Self::bytes_object(py, self.buffer.unread());
        self.buffer.consume_all();
        value
    }

    fn incomplete_read_error(
        py: Python<'_>,
        partial: &[u8],
        expected: usize,
    ) -> PyResult<Py<PyAny>> {
        let asyncio = py.import("asyncio")?;
        Ok(asyncio
            .getattr("IncompleteReadError")?
            .call1((PyBytes::new(py, partial), expected))?
            .unbind())
    }

    fn maybe_resume_transport(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.paused && self.buffer.len() <= self.limit && !self.transport.bind(py).is_none() {
            self.paused = false;
            python_names::call_method0(
                py,
                self.transport.bind(py),
                python_names::resume_reading(py),
            )?;
        }
        Ok(())
    }

    fn maybe_pause_transport(&mut self, py: Python<'_>) -> PyResult<()> {
        // Never pause while a coroutine is waiting for data: a pending
        // readexactly()/read() waiter may need more than 2 * limit bytes to
        // complete, and pausing would starve it forever. This mirrors
        // asyncio.StreamReader._wait_for_data, which resumes the transport
        // while a reader is waiting.
        if self.waiter.is_none()
            && !self.transport.bind(py).is_none()
            && !self.paused
            && self.buffer.len() > 2 * self.limit
        {
            match python_names::call_method0(
                py,
                self.transport.bind(py),
                python_names::pause_reading(py),
            ) {
                Ok(_) => {
                    self.paused = true;
                }
                Err(err) if err.is_instance_of::<pyo3::exceptions::PyNotImplementedError>(py) => {
                    self.transport = py.None();
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn maybe_complete_waiter(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some((future, kind)) = self
            .waiter
            .as_ref()
            .map(|waiter| (waiter.future.clone_ref(py), waiter.kind.clone()))
        else {
            return Ok(());
        };

        if let Some(exc) = self.exception.as_ref() {
            self.waiter = None;
            Self::set_future_exception_or_ignore_cancelled(py, &future, exc.clone_ref(py))?;
            return Ok(());
        }

        if let Some(waiter) = &mut self.waiter
            && let Some(exact) = &mut waiter.exact
        {
            exact.fill_from(&mut self.buffer);
        }

        match kind {
            ReadWaitKind::Any(n) => {
                if self.buffer.is_empty() && !self.eof {
                    return Ok(());
                }
                self.waiter = None;
                let data = self.unread_bytes_object(py, n);
                self.maybe_resume_transport(py)?;
                Self::set_future_result_or_ignore_cancelled(py, &future, data)?;
            }
            ReadWaitKind::Exact(n) => {
                let exact = self
                    .waiter
                    .as_ref()
                    .and_then(|waiter| waiter.exact.as_ref())
                    .expect("exact read waiter missing accumulator");
                if exact.filled >= n {
                    let data = exact.value.clone_ref(py).into_any();
                    self.waiter = None;
                    self.maybe_resume_transport(py)?;
                    Self::set_future_result_or_ignore_cancelled(py, &future, data)?;
                    return Ok(());
                }
                if !self.eof {
                    return Ok(());
                }
                let err = Self::incomplete_read_error(py, exact.partial(), n)?;
                self.waiter = None;
                Self::set_future_exception_or_ignore_cancelled(py, &future, err)?;
            }
            ReadWaitKind::All => {
                if !self.eof {
                    return Ok(());
                }
                self.waiter = None;
                let data = self.unread_all_bytes_object(py);
                Self::set_future_result_or_ignore_cancelled(py, &future, data)?;
            }
            ReadWaitKind::Until => {
                let limit = self.limit;
                let (scan, line_mode) = {
                    let Some(state) = self
                        .waiter
                        .as_mut()
                        .and_then(|waiter| waiter.until.as_mut())
                    else {
                        return Ok(());
                    };
                    (state.scan(self.buffer.unread(), limit), state.line_mode)
                };
                if scan.is_pending(self.eof) {
                    return Ok(());
                }
                self.waiter = None;
                match self.resolve_until_scan(py, scan, line_mode)? {
                    UntilOutcome::Value(value) => {
                        Self::set_future_result_or_ignore_cancelled(py, &future, value)?;
                    }
                    UntilOutcome::Exception(exc) => {
                        Self::set_future_exception_or_ignore_cancelled(py, &future, exc)?;
                    }
                }
            }
        }

        Ok(())
    }

    fn start_waiter(
        &mut self,
        py: Python<'_>,
        func_name: &str,
        kind: ReadWaitKind,
        until: Option<UntilReadState>,
    ) -> PyResult<Py<PyAny>> {
        if self.waiter.is_some() {
            return Err(PyValueError::new_err(format!(
                "{func_name}() called while another coroutine is already waiting for incoming data"
            )));
        }
        if self.paused && !self.transport.bind(py).is_none() {
            self.paused = false;
            python_names::call_method0(
                py,
                self.transport.bind(py),
                python_names::resume_reading(py),
            )?;
        }
        let future = self.create_future(py)?;
        let exact = match &kind {
            ReadWaitKind::Exact(expected) => Some(ExactReadAccumulator::new(py, *expected)?),
            _ => None,
        };
        self.waiter = Some(ReadWaiter {
            future: future.clone_ref(py),
            kind,
            exact,
            until,
        });
        if let Some(waiter) = &mut self.waiter
            && let Some(exact) = &mut waiter.exact
        {
            exact.fill_from(&mut self.buffer);
        }
        Ok(future)
    }

    pub(crate) fn set_transport_obj(
        &mut self,
        py: Python<'_>,
        transport: Py<PyAny>,
    ) -> PyResult<()> {
        self.transport = transport;
        self.maybe_resume_transport(py)
    }

    pub(crate) fn feed_data_internal(&mut self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        if self.eof {
            return Err(PyValueError::new_err("feed_data after feed_eof"));
        }
        self.buffer.extend(data);
        self.maybe_complete_waiter(py)?;
        self.maybe_pause_transport(py)
    }

    pub(super) fn feed_owned_data_internal(
        &mut self,
        py: Python<'_>,
        data: Vec<u8>,
        pool: &std::sync::Arc<ReadBufferPool>,
    ) -> PyResult<()> {
        let data = OwnedReadBuffer::from_pooled(data, pool);
        if self.eof {
            return Err(PyValueError::new_err("feed_data after feed_eof"));
        }
        let _recycled = self.buffer.extend_owned(data);
        self.maybe_complete_waiter(py)?;
        self.maybe_pause_transport(py)
    }

    #[inline]
    pub(crate) fn feed_eof_internal(&mut self, py: Python<'_>) -> PyResult<()> {
        self.eof = true;
        self.maybe_complete_waiter(py)
    }

    pub(crate) fn set_exception_internal(
        &mut self,
        py: Python<'_>,
        exc: Py<PyAny>,
    ) -> PyResult<()> {
        self.exception = Some(exc);
        self.maybe_complete_waiter(py)
    }

    fn build_read_future(&mut self, py: Python<'_>, n: isize) -> PyResult<Py<PyAny>> {
        if let Some(exc) = self.exception.as_ref() {
            return self.ready_exception_future(py, exc.clone_ref(py));
        }
        if n == 0 {
            return self.ready_result_awaitable(py, Self::bytes_object(py, &[]));
        }
        if n < 0 {
            if self.eof {
                let data = self.unread_all_bytes_object(py);
                return self.ready_result_awaitable(py, data);
            }
            return self.start_waiter(py, "read", ReadWaitKind::All, None);
        }
        let n = usize::try_from(n).expect("nonnegative read size fits usize");
        if !self.buffer.is_empty() || self.eof {
            let data = self.unread_bytes_object(py, n);
            self.maybe_resume_transport(py)?;
            return self.ready_result_awaitable(py, data);
        }
        self.start_waiter(py, "read", ReadWaitKind::Any(n), None)
    }

    fn build_readexactly_future(&mut self, py: Python<'_>, n: usize) -> PyResult<Py<PyAny>> {
        if let Some(exc) = self.exception.as_ref() {
            return self.ready_exception_future(py, exc.clone_ref(py));
        }
        if n == 0 {
            return self.ready_result_awaitable(py, Self::bytes_object(py, &[]));
        }
        if self.buffer.len() >= n {
            let data = self.unread_bytes_object(py, n);
            self.maybe_resume_transport(py)?;
            return self.ready_result_awaitable(py, data);
        }
        if self.eof {
            let err = Self::incomplete_read_error(py, self.buffer.unread(), n)?;
            self.buffer.consume_all();
            return self.ready_exception_future(py, err);
        }
        self.start_waiter(py, "readexactly", ReadWaitKind::Exact(n), None)
    }

    fn limit_overrun_error(py: Python<'_>, message: &str, consumed: usize) -> PyResult<Py<PyAny>> {
        let asyncio = py.import("asyncio")?;
        Ok(asyncio
            .getattr("LimitOverrunError")?
            .call1((message, consumed))?
            .unbind())
    }

    /// `IncompleteReadError` with an undefined expected size, which is what
    /// `readuntil()` raises: the caller never said how many bytes it wanted.
    fn incomplete_until_error(py: Python<'_>, partial: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let asyncio = py.import("asyncio")?;
        Ok(asyncio
            .getattr("IncompleteReadError")?
            .call1((partial, py.None()))?
            .unbind())
    }

    fn until_limit_overrun(
        &mut self,
        py: Python<'_>,
        message: &str,
        consumed: usize,
        line_mode: bool,
    ) -> PyResult<UntilOutcome> {
        if !line_mode {
            // readuntil() leaves the buffer intact so the data survives for a
            // retry with a larger limit.
            return Ok(UntilOutcome::Exception(Self::limit_overrun_error(
                py, message, consumed,
            )?));
        }
        // readline() drops the oversized line instead — including its newline
        // when one was found — and reports a plain ValueError.
        let unread = self.buffer.unread();
        if unread.len() > consumed && unread[consumed] == b'\n' {
            self.buffer.consume(consumed + 1);
        } else {
            self.buffer.consume_all();
        }
        self.maybe_resume_transport(py)?;
        Ok(UntilOutcome::Exception(
            PyValueError::new_err(message.to_owned())
                .into_value(py)
                .into_any(),
        ))
    }

    /// Turn a finished scan into the value or exception the awaitable carries.
    /// Only called once `UntilScan::is_pending` has ruled out waiting, so
    /// `NeedMore` here always means EOF arrived first.
    fn resolve_until_scan(
        &mut self,
        py: Python<'_>,
        scan: UntilScan,
        line_mode: bool,
    ) -> PyResult<UntilOutcome> {
        match scan {
            UntilScan::Found {
                match_start,
                match_end,
            } => {
                if match_start > self.limit {
                    return self.until_limit_overrun(
                        py,
                        "Separator is found, but chunk is longer than limit",
                        match_start,
                        line_mode,
                    );
                }
                let value = Self::bytes_object(py, &self.buffer.unread()[..match_end]);
                self.buffer.consume(match_end);
                self.maybe_resume_transport(py)?;
                Ok(UntilOutcome::Value(value))
            }
            UntilScan::OverLimit { consumed } => self.until_limit_overrun(
                py,
                "Separator is not found, and chunk exceed the limit",
                consumed,
                line_mode,
            ),
            UntilScan::NeedMore => {
                let partial = self.unread_all_bytes_object(py);
                if line_mode {
                    // readline() reports the unterminated tail as the line, and
                    // an empty bytes object once the buffer is drained.
                    return Ok(UntilOutcome::Value(partial));
                }
                Ok(UntilOutcome::Exception(Self::incomplete_until_error(
                    py, partial,
                )?))
            }
        }
    }

    fn build_until_future(
        &mut self,
        py: Python<'_>,
        func_name: &str,
        mut state: UntilReadState,
    ) -> PyResult<Py<PyAny>> {
        if let Some(exc) = self.exception.as_ref() {
            return self.ready_exception_future(py, exc.clone_ref(py));
        }
        let line_mode = state.line_mode;
        // A chunk can complete the separator and set EOF at once, so the buffer
        // is always inspected before EOF is allowed to end the read.
        let scan = state.scan(self.buffer.unread(), self.limit);
        if scan.is_pending(self.eof) {
            return self.start_waiter(py, func_name, ReadWaitKind::Until, Some(state));
        }
        match self.resolve_until_scan(py, scan, line_mode)? {
            UntilOutcome::Value(value) => self.ready_result_awaitable(py, value),
            UntilOutcome::Exception(exc) => self.ready_exception_future(py, exc),
        }
    }
}

#[pymethods]
impl PyFastStreamReader {
    #[new]
    #[pyo3(signature = (limit=DEFAULT_STREAM_LIMIT, loop_obj=None))]
    fn py_new(py: Python<'_>, limit: usize, loop_obj: Option<Py<PyAny>>) -> PyResult<Self> {
        let loop_obj = match loop_obj {
            Some(loop_obj) => loop_obj,
            None => py
                .import("asyncio.events")?
                .call_method0("get_event_loop")?
                .unbind(),
        };
        Self::new_with_loop(py, loop_obj, limit)
    }

    #[getter(_rsloop_fast_reader)]
    fn get_rsloop_fast_reader(&self, py: Python<'_>) -> Py<PyAny> {
        py.None()
    }

    #[getter(_loop)]
    fn get_loop_obj(&self, py: Python<'_>) -> Py<PyAny> {
        self.loop_obj.clone_ref(py)
    }

    #[getter(_limit)]
    fn get_limit(&self) -> usize {
        self.limit
    }

    #[getter(_buffer)]
    fn get_buffer(&self, py: Python<'_>) -> Py<PyAny> {
        PyByteArray::new(py, self.buffer.unread())
            .unbind()
            .into_any()
    }

    #[setter(_buffer)]
    fn set_buffer(&mut self, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let data: Vec<u8> = value.extract()?;
        self.buffer.replace(&data);
        Ok(())
    }

    #[getter(_waiter)]
    fn get_waiter(&self, py: Python<'_>) -> Py<PyAny> {
        self.waiter
            .as_ref()
            .map(|waiter| waiter.future.clone_ref(py))
            .unwrap_or_else(|| py.None())
    }

    #[getter(_transport)]
    fn get_transport(&self, py: Python<'_>) -> Py<PyAny> {
        self.transport.clone_ref(py)
    }

    #[getter(_paused)]
    fn get_paused(&self) -> bool {
        self.paused
    }

    #[getter(_eof)]
    fn get_eof(&self) -> bool {
        self.eof
    }

    #[getter(_exception)]
    fn get_exception_obj(&self, py: Python<'_>) -> Py<PyAny> {
        self.exception
            .as_ref()
            .map(|exc| exc.clone_ref(py))
            .unwrap_or_else(|| py.None())
    }

    fn exception(&self, py: Python<'_>) -> Py<PyAny> {
        self.get_exception_obj(py)
    }

    fn set_exception(&mut self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<()> {
        self.set_exception_internal(py, exc)
    }

    fn set_transport_public(&mut self, py: Python<'_>, transport: Py<PyAny>) -> PyResult<()> {
        self.set_transport_obj(py, transport)
    }

    fn feed_data(&mut self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        self.feed_data_internal(py, data)
    }

    fn feed_eof(&mut self, py: Python<'_>) -> PyResult<()> {
        self.feed_eof_internal(py)
    }

    fn at_eof(&self) -> bool {
        self.eof && self.buffer.is_empty()
    }

    #[pyo3(signature = (n=-1))]
    fn read(&mut self, py: Python<'_>, n: isize) -> PyResult<Py<PyAny>> {
        self.build_read_future(py, n)
    }

    fn readexactly(&mut self, py: Python<'_>, n: usize) -> PyResult<Py<PyAny>> {
        self.build_readexactly_future(py, n)
    }

    #[pyo3(signature = (separator=None))]
    fn readuntil(
        &mut self,
        py: Python<'_>,
        separator: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let separators = match separator {
            Some(separator) => extract_separators(separator),
            None => Ok(Separators::single(b"\n")),
        };
        let state = match separators.and_then(|separators| UntilReadState::new(separators, false)) {
            Ok(state) => state,
            // asyncio validates the separator inside the coroutine, so these
            // surface when the awaitable is awaited rather than when it is made.
            Err(err) => return self.ready_exception_future(py, err.into_value(py).into_any()),
        };
        self.build_until_future(py, "readuntil", state)
    }

    fn readline(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let state = UntilReadState::new(Separators::single(b"\n"), true)?;
        self.build_until_future(py, "readline", state)
    }
}

#[pyclass(module = "rsloop._loop")]
pub struct PyFastStreamProtocol {
    loop_obj: Py<PyAny>,
    reader: Py<PyFastStreamReader>,
    client_connected_cb: Py<PyAny>,
    transport: Py<PyAny>,
    task: Py<PyAny>,
    closed: Py<PyAny>,
    ready_none: Py<PyAny>,
    paused: bool,
    drain_waiters: Vec<Py<PyAny>>,
    connection_lost: bool,
}

impl PyFastStreamProtocol {
    fn new_with_loop(
        py: Python<'_>,
        loop_obj: Py<PyAny>,
        reader: Py<PyFastStreamReader>,
        client_connected_cb: Py<PyAny>,
    ) -> PyResult<Self> {
        let closed = loop_create_future(py, &loop_obj)?;
        let ready_none = loop_create_future(py, &loop_obj)?;
        python_names::call_method1(
            py,
            ready_none.bind(py),
            python_names::set_result(py),
            py.None().bind(py),
        )?;
        Ok(Self {
            closed,
            ready_none,
            loop_obj,
            reader,
            client_connected_cb,
            transport: py.None(),
            task: py.None(),
            paused: false,
            drain_waiters: Vec::new(),
            connection_lost: false,
        })
    }

    fn has_client_connected_cb(&self, py: Python<'_>) -> bool {
        !self.client_connected_cb.bind(py).is_none()
    }

    pub(crate) fn reader_ref(&self, py: Python<'_>) -> Py<PyFastStreamReader> {
        self.reader.clone_ref(py)
    }

    fn ready_none_future(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.ready_none.clone_ref(py))
    }

    fn ready_exception_future(&self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let future = loop_create_future(py, &self.loop_obj)?;
        python_names::call_method1(
            py,
            future.bind(py),
            python_names::set_exception(py),
            exc.bind(py),
        )?;
        Ok(future)
    }

    fn push_drain_waiter(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let future = loop_create_future(py, &self.loop_obj)?;
        self.drain_waiters.push(future.clone_ref(py));
        Ok(future)
    }

    fn resolve_drain_waiters(&mut self, py: Python<'_>, exc: Option<Py<PyAny>>) -> PyResult<()> {
        for future in self.drain_waiters.drain(..) {
            let future = future.bind(py);
            if python_names::call_method0(py, future, python_names::done(py))?
                .bind(py)
                .extract::<bool>()?
            {
                continue;
            }
            match exc.as_ref() {
                Some(exc) => {
                    python_names::call_method1(
                        py,
                        future,
                        python_names::set_exception(py),
                        exc.bind(py),
                    )?;
                }
                None => {
                    python_names::call_method1(
                        py,
                        future,
                        python_names::set_result(py),
                        py.None().bind(py),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn build_drain_future(
        &mut self,
        py: Python<'_>,
        reader_exception: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        if let Some(exc) = reader_exception {
            return self.ready_exception_future(py, exc);
        }
        if self.connection_lost {
            let builtins = py.import("builtins")?;
            let exc = builtins
                .getattr("ConnectionResetError")?
                .call1(("Connection lost",))?
                .unbind();
            return self.ready_exception_future(py, exc);
        }
        if !self.paused {
            return self.ready_none_future(py);
        }
        self.push_drain_waiter(py)
    }

    pub(crate) fn handle_connection_made(
        slf: Py<Self>,
        py: Python<'_>,
        transport: Py<PyAny>,
    ) -> PyResult<()> {
        {
            let mut protocol = slf.borrow_mut(py);
            protocol.transport = transport.clone_ref(py);
            protocol
                .reader
                .borrow_mut(py)
                .set_transport_obj(py, transport.clone_ref(py))?;
            if !protocol.has_client_connected_cb(py) {
                return Ok(());
            }
        }

        let (loop_obj, callback, reader) = {
            let protocol = slf.borrow(py);
            (
                protocol.loop_obj.clone_ref(py),
                protocol.client_connected_cb.clone_ref(py),
                protocol.reader.clone_ref(py),
            )
        };
        let writer = Py::new(
            py,
            PyFastStreamWriter {
                transport: transport.clone_ref(py),
                protocol: slf.clone_ref(py),
                reader: reader.clone_ref(py),
            },
        )?;
        let result = callback.call1(py, (reader.clone_ref(py), writer))?;
        if !asyncio_iscoroutine(py)?
            .call1((result.clone_ref(py),))?
            .extract::<bool>()?
        {
            return Ok(());
        }

        let task = match crate::bindings::try_fast_create_task(py, &loop_obj, result.clone_ref(py))?
        {
            Some(task) => task,
            None => loop_obj.call_method1(py, "create_task", (result,))?,
        };
        slf.borrow_mut(py).task = task.clone_ref(py);
        let done_cb = Py::new(
            py,
            PyFastClientDoneCallback {
                loop_obj,
                transport,
            },
        )?;
        task.call_method1(py, "add_done_callback", (done_cb,))?;
        Ok(())
    }

    pub(crate) fn handle_connection_lost(
        &mut self,
        py: Python<'_>,
        exc: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        self.connection_lost = true;
        match exc {
            Some(exc) => {
                self.reader
                    .borrow_mut(py)
                    .set_exception_internal(py, exc.clone_ref(py))?;
                if !python_names::call_method0(py, self.closed.bind(py), python_names::done(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    python_names::call_method1(
                        py,
                        self.closed.bind(py),
                        python_names::set_exception(py),
                        exc.bind(py),
                    )?;
                }
                self.resolve_drain_waiters(py, Some(exc))?;
            }
            None => {
                self.reader.borrow_mut(py).feed_eof_internal(py)?;
                if !python_names::call_method0(py, self.closed.bind(py), python_names::done(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    python_names::call_method1(
                        py,
                        self.closed.bind(py),
                        python_names::set_result(py),
                        py.None().bind(py),
                    )?;
                }
                self.resolve_drain_waiters(py, None)?;
            }
        }
        {
            let mut reader = self.reader.borrow_mut(py);
            reader.transport = py.None();
            reader.paused = false;
        }
        self.transport = py.None();
        self.task = py.None();
        Ok(())
    }
}

#[pymethods]
impl PyFastStreamProtocol {
    #[new]
    #[pyo3(signature = (reader, client_connected_cb=None, loop_obj=None))]
    fn py_new(
        py: Python<'_>,
        reader: Py<PyFastStreamReader>,
        client_connected_cb: Option<Py<PyAny>>,
        loop_obj: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let loop_obj = match loop_obj {
            Some(loop_obj) => loop_obj,
            None => py
                .import("asyncio.events")?
                .call_method0("get_event_loop")?
                .unbind(),
        };
        Self::new_with_loop(
            py,
            loop_obj,
            reader,
            client_connected_cb.unwrap_or_else(|| py.None()),
        )
    }

    #[getter(_rsloop_fast_reader)]
    fn get_rsloop_fast_reader(&self, py: Python<'_>) -> Py<PyAny> {
        self.reader.clone_ref(py).into_any()
    }

    fn connection_made(slf: Py<Self>, py: Python<'_>, transport: Py<PyAny>) -> PyResult<()> {
        Self::handle_connection_made(slf, py, transport)
    }

    fn pause_writing(&mut self) {
        self.paused = true;
    }

    fn resume_writing(&mut self, py: Python<'_>) -> PyResult<()> {
        self.paused = false;
        self.resolve_drain_waiters(py, None)
    }

    fn _drain_helper(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.build_drain_future(py, None)
    }

    fn data_received(&mut self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        self.reader.borrow_mut(py).feed_data_internal(py, data)
    }

    fn eof_received(&mut self, py: Python<'_>) -> PyResult<bool> {
        self.reader.borrow_mut(py).feed_eof_internal(py)?;
        Ok(true)
    }

    fn connection_lost(&mut self, py: Python<'_>, exc: Option<Py<PyAny>>) -> PyResult<()> {
        self.handle_connection_lost(py, exc)
    }
}

/// Native stream writer paired with [`PyFastStreamReader`].
///
/// Writes use a direct Rust path for rsloop transports and delegate to Python
/// for foreign asyncio transports.
#[pyclass(module = "rsloop._loop")]
pub struct PyFastStreamWriter {
    transport: Py<PyAny>,
    protocol: Py<PyFastStreamProtocol>,
    reader: Py<PyFastStreamReader>,
}

#[pymethods]
impl PyFastStreamWriter {
    #[getter]
    fn transport(&self, py: Python<'_>) -> Py<PyAny> {
        self.transport.clone_ref(py)
    }

    fn write(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        // Native transports take the direct Rust path; only foreign
        // transports go through Python method dispatch.
        if let Ok(transport) = self.transport.bind(py).cast_exact::<PyStreamTransport>() {
            return transport.borrow().write_data(py, data);
        }
        self.transport.call_method1(py, "write", (data,))?;
        Ok(())
    }

    fn writelines(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        if let Ok(transport) = self.transport.bind(py).cast_exact::<PyStreamTransport>() {
            return transport.borrow().writelines(py, data);
        }
        self.transport.call_method1(py, "writelines", (data,))?;
        Ok(())
    }

    fn write_eof(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.transport.call_method0(py, "write_eof")
    }

    fn can_write_eof(&self, py: Python<'_>) -> PyResult<bool> {
        self.transport
            .call_method0(py, "can_write_eof")?
            .extract(py)
    }

    fn close(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.transport.call_method0(py, "close")
    }

    fn is_closing(&self, py: Python<'_>) -> PyResult<bool> {
        self.transport.call_method0(py, "is_closing")?.extract(py)
    }

    #[pyo3(signature = (name, default=None))]
    fn get_extra_info(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.transport.call_method1(
            py,
            "get_extra_info",
            (name, default.unwrap_or_else(|| py.None())),
        )
    }

    fn wait_closed(&self, py: Python<'_>) -> Py<PyAny> {
        self.protocol.borrow(py).closed.clone_ref(py)
    }

    fn drain(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let reader_exception = self
            .reader
            .borrow(py)
            .exception
            .as_ref()
            .map(|exc| exc.clone_ref(py));
        self.protocol
            .borrow_mut(py)
            .build_drain_future(py, reader_exception)
    }
}

#[pyclass(module = "rsloop._loop")]
struct PyFastClientDoneCallback {
    loop_obj: Py<PyAny>,
    transport: Py<PyAny>,
}

#[pymethods]
impl PyFastClientDoneCallback {
    fn __call__(&self, py: Python<'_>, task: Py<PyAny>) -> PyResult<()> {
        if task.call_method0(py, "cancelled")?.extract::<bool>(py)? {
            self.transport.call_method0(py, "close")?;
            return Ok(());
        }

        let exc = task.call_method0(py, "exception")?;
        if exc.bind(py).is_none() {
            return Ok(());
        }

        let context = PyDict::new(py);
        context.set_item("message", "Unhandled exception in client_connected_cb")?;
        context.set_item("exception", exc.clone_ref(py))?;
        context.set_item("transport", self.transport.clone_ref(py))?;
        self.loop_obj
            .call_method1(py, "call_exception_handler", (context,))?;
        self.transport.call_method0(py, "close")?;
        Ok(())
    }
}

#[pyclass(module = "rsloop._loop")]
struct PyFastProtocolFactory {
    loop_obj: Py<PyAny>,
    limit: usize,
    client_connected_cb: Py<PyAny>,
}

#[pymethods]
impl PyFastProtocolFactory {
    fn __call__(&self, py: Python<'_>) -> PyResult<Py<PyFastStreamProtocol>> {
        let reader = Py::new(
            py,
            PyFastStreamReader::new_with_loop(py, self.loop_obj.clone_ref(py), self.limit)?,
        )?;
        Py::new(
            py,
            PyFastStreamProtocol::new_with_loop(
                py,
                self.loop_obj.clone_ref(py),
                reader,
                self.client_connected_cb.clone_ref(py),
            )?,
        )
    }
}

fn running_loop(py: Python<'_>) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("asyncio.events")?
        .call_method0("get_running_loop")?
        .unbind())
}

fn call_asyncio_streams_function(
    py: Python<'_>,
    name: &str,
    args: &Bound<'_, PyTuple>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let module = py.import("asyncio.streams")?;
    Ok(module.getattr(name)?.call(args, kwargs)?.unbind())
}

fn kwargs_with_limit<'py>(
    py: Python<'py>,
    kwargs: Option<&Bound<'py, PyDict>>,
    limit: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    if let Some(kwargs) = kwargs {
        for (key, value) in kwargs.iter() {
            dict.set_item(key, value)?;
        }
    }
    dict.set_item("limit", limit)?;
    Ok(dict)
}

fn copy_kwargs<'py>(
    py: Python<'py>,
    kwargs: Option<&Bound<'py, PyDict>>,
) -> PyResult<Option<Bound<'py, PyDict>>> {
    let Some(kwargs) = kwargs else {
        return Ok(None);
    };

    let copied = PyDict::new(py);
    for (key, value) in kwargs.iter() {
        copied.set_item(key, value)?;
    }
    Ok(Some(copied))
}

fn native_stream_loop(
    py: Python<'_>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Option<Py<PyAny>>> {
    let loop_obj = running_loop(py)?;
    if !loop_obj.bind(py).is_instance_of::<PyLoop>() {
        return Ok(None);
    }
    if let Some(kwargs) = kwargs
        && let Some(ssl) = kwargs.get_item("ssl")?
        && !ssl.is_none()
    {
        return Ok(None);
    }
    Ok(Some(loop_obj))
}

fn host_port_objects(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
) -> (Py<PyAny>, Py<PyAny>) {
    let host_obj = host
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let port_obj = port
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    (host_obj, port_obj)
}

fn fast_open_connection_awaitable(
    py: Python<'_>,
    loop_obj: &Py<PyAny>,
    host_obj: Py<PyAny>,
    port_obj: Py<PyAny>,
    limit: usize,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<(TaskLocals, Py<PyAny>)> {
    let locals = task_locals_for_loop(py, loop_obj)?;
    let factory = Py::new(
        py,
        PyFastProtocolFactory {
            loop_obj: loop_obj.clone_ref(py),
            limit,
            client_connected_cb: py.None(),
        },
    )?;
    let kwargs = copy_kwargs(py, kwargs)?;
    let create_args = PyTuple::new(py, [factory.into_any(), host_obj, port_obj])?;
    let awaitable = loop_obj.call_method(py, "create_connection", &create_args, kwargs.as_ref())?;
    Ok((locals, awaitable))
}

fn fast_open_connection_result(py: Python<'_>, created: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let result = created.bind(py).cast::<PyTuple>()?;
    let transport = result.get_item(0)?.unbind();
    let protocol: Py<PyFastStreamProtocol> = result.get_item(1)?.extract()?;
    let reader = protocol.borrow(py).reader.clone_ref(py);
    let writer = Py::new(
        py,
        PyFastStreamWriter {
            transport,
            protocol,
            reader: reader.clone_ref(py),
        },
    )?;
    let output = PyTuple::new(py, [reader.into_any(), writer.into_any()])?;
    Ok(output.unbind().into_any())
}

/// Returns an awaitable that opens a stream connection.
///
/// With a running [`PyLoop`](crate::PyLoop) and no TLS argument, the awaitable
/// resolves to a native [`PyFastStreamReader`] and [`PyFastStreamWriter`].
/// Unsupported loop or TLS configurations delegate to
/// `asyncio.open_connection`.
///
/// Extra keyword arguments are forwarded to the loop connection factory.
#[pyfunction(signature = (host=None, port=None, *, limit=DEFAULT_STREAM_LIMIT, **kwargs))]
pub fn open_connection(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    limit: usize,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let (host_obj, port_obj) = host_port_objects(py, host, port);
    let args = PyTuple::new(py, [host_obj.clone_ref(py), port_obj.clone_ref(py)])?;

    let Some(loop_obj) = native_stream_loop(py, kwargs)? else {
        let kwargs = kwargs_with_limit(py, kwargs, limit)?;
        return call_asyncio_streams_function(py, "open_connection", &args, Some(&kwargs));
    };

    let (locals, awaitable) =
        fast_open_connection_awaitable(py, &loop_obj, host_obj, port_obj, limit, kwargs)?;

    Ok(pyo3_async_runtimes::async_std::future_into_py_with_locals(
        py,
        locals.clone(),
        async move {
            let created = Python::attach(|py| {
                pyo3_async_runtimes::into_future_with_locals(&locals, awaitable.bind(py).clone())
            })?
            .await?;

            Python::attach(|py| fast_open_connection_result(py, created))
        },
    )?
    .unbind())
}

/// Returns an awaitable that starts a stream server.
///
/// With a running [`PyLoop`](crate::PyLoop) and no TLS argument, accepted
/// connections use native fast readers and writers before invoking
/// `client_connected_cb`. Unsupported configurations delegate to
/// `asyncio.start_server`.
///
/// `limit` controls each reader's buffer limit; extra keyword arguments are
/// forwarded to the loop server factory.
#[pyfunction(signature = (client_connected_cb, host=None, port=None, *, limit=DEFAULT_STREAM_LIMIT, **kwargs))]
pub fn start_server(
    py: Python<'_>,
    client_connected_cb: Py<PyAny>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    limit: usize,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let host_obj = host
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let port_obj = port
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let args = PyTuple::new(
        py,
        [
            client_connected_cb.clone_ref(py),
            host_obj.clone_ref(py),
            port_obj.clone_ref(py),
        ],
    )?;

    let Some(loop_obj) = native_stream_loop(py, kwargs)? else {
        let kwargs = kwargs_with_limit(py, kwargs, limit)?;
        return call_asyncio_streams_function(py, "start_server", &args, Some(&kwargs));
    };

    let locals = task_locals_for_loop(py, &loop_obj)?;
    let factory = Py::new(
        py,
        PyFastProtocolFactory {
            loop_obj: loop_obj.clone_ref(py),
            limit,
            client_connected_cb,
        },
    )?;
    let kwargs = copy_kwargs(py, kwargs)?;
    let create_args = PyTuple::new(py, [factory.into_any(), host_obj, port_obj])?;
    let awaitable = loop_obj.call_method(py, "create_server", &create_args, kwargs.as_ref())?;

    Ok(pyo3_async_runtimes::async_std::future_into_py_with_locals(
        py,
        locals.clone(),
        async move {
            Python::attach(|py| {
                pyo3_async_runtimes::into_future_with_locals(&locals, awaitable.bind(py).clone())
            })?
            .await
        },
    )?
    .unbind())
}
