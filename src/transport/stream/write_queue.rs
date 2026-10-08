//! Allocation-stable command queue for a transport writer worker.

use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

use super::WriterCommand;
#[cfg(any(unix, test))]
use super::buffers::OwnedWriteBuffer;

const INITIAL_WRITER_QUEUE_CAPACITY: usize = 8;

struct QueueState {
    commands: VecDeque<WriterCommand>,
    sender_alive: bool,
    receiver_alive: bool,
}

impl QueueState {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "QueueState"))]
    fn new() -> Self {
        Self::with_capacity(INITIAL_WRITER_QUEUE_CAPACITY)
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "QueueState"))]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            commands: VecDeque::with_capacity(capacity),
            sender_alive: true,
            receiver_alive: true,
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "QueueState"))]
    fn enqueue(&mut self, command: WriterCommand) -> Result<(), WriterCommand> {
        if !self.receiver_alive {
            return Err(command);
        }
        if let WriterCommand::Data(data) = &command
            && let Some(WriterCommand::Data(pending)) = self.commands.back_mut()
            && pending.try_append(data.remaining())
        {
            return Ok(());
        }
        self.commands.push_back(command);
        Ok(())
    }

    /// Drain queued commands before reporting sender disconnection. Both
    /// receive paths use this transition; locking and waiting stay outside it.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "QueueState"))]
    fn try_dequeue(&mut self) -> Result<WriterCommand, TryRecvError> {
        if let Some(command) = self.commands.pop_front() {
            Ok(command)
        } else if self.sender_alive {
            Err(TryRecvError::Empty)
        } else {
            Err(TryRecvError::Disconnected)
        }
    }
}

struct SharedQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

pub(super) struct WriterSender {
    shared: Arc<SharedQueue>,
}

pub(super) struct WriterReceiver {
    shared: Arc<SharedQueue>,
}

pub(super) enum TryRecvError {
    Empty,
    Disconnected,
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn channel() -> (WriterSender, WriterReceiver) {
    let shared = Arc::new(SharedQueue {
        state: Mutex::new(QueueState::new()),
        ready: Condvar::new(),
    });
    (
        WriterSender {
            shared: Arc::clone(&shared),
        },
        WriterReceiver { shared },
    )
}

impl WriterSender {
    /// Publish an entire large write before a worker or control command can
    /// observe it. The caller has already accounted for all of these bytes.
    #[cfg(any(unix, test))]
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterSender"))]
    pub(super) fn send_batch(
        &self,
        buffers: impl IntoIterator<Item = OwnedWriteBuffer>,
    ) -> Result<(), ()> {
        let mut state = self.shared.state.lock().expect("poisoned writer queue");
        if !state.receiver_alive {
            return Err(());
        }
        // Large batches retain each allocation instead of copying into a
        // contiguous block. Small writes continue through send's coalescing.
        state
            .commands
            .extend(buffers.into_iter().map(WriterCommand::Data));
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterSender"))]
    pub(super) fn send(&self, command: WriterCommand) -> Result<(), WriterCommand> {
        let mut state = self.shared.state.lock().expect("poisoned writer queue");
        state.enqueue(command)?;
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }
}

impl Drop for WriterSender {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<WriterSender as Drop>")
    )]
    fn drop(&mut self) {
        // Serialize disconnection with recv's predicate check and Condvar wait
        // to prevent a lost shutdown notification. No I/O or wait is performed
        // while holding this guard; deferring cleanup would require a runtime.
        self.shared
            .state
            // qualirs:ignore Q0082
            .lock()
            .expect("poisoned writer queue")
            .sender_alive = false;
        self.shared.ready.notify_all();
    }
}

impl WriterReceiver {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterReceiver"))]
    pub(super) fn recv(&self) -> Result<WriterCommand, ()> {
        let mut state = self.shared.state.lock().expect("poisoned writer queue");
        loop {
            match state.try_dequeue() {
                Ok(command) => return Ok(command),
                Err(TryRecvError::Disconnected) => return Err(()),
                Err(TryRecvError::Empty) => {}
            }
            state = self
                .shared
                .ready
                .wait(state)
                .expect("poisoned writer queue");
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WriterReceiver"))]
    pub(super) fn try_recv(&self) -> Result<WriterCommand, TryRecvError> {
        self.shared
            .state
            .lock()
            .expect("poisoned writer queue")
            .try_dequeue()
    }
}

impl Drop for WriterReceiver {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<WriterReceiver as Drop>")
    )]
    fn drop(&mut self) {
        // Serialize disconnection with enqueue so later sends are rejected.
        // This guard only updates queue metadata; it does not wait for the
        // worker to finish. Contention is possible, but cleanup must be
        // synchronous.
        self.shared
            .state
            // qualirs:ignore Q0082
            .lock()
            .expect("poisoned writer queue")
            .receiver_alive = false;
        self.shared.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_receive_paths_drain_pending_commands_after_sender_disconnects() {
        for blocking in [false, true] {
            let (sender, receiver) = channel();
            assert!(sender.send(WriterCommand::WriteEof).is_ok());
            assert!(sender.send(WriterCommand::Stop).is_ok());
            drop(sender);

            if blocking {
                assert!(matches!(receiver.recv(), Ok(WriterCommand::WriteEof)));
                assert!(matches!(receiver.recv(), Ok(WriterCommand::Stop)));
                assert!(receiver.recv().is_err());
            } else {
                assert!(matches!(receiver.try_recv(), Ok(WriterCommand::WriteEof)));
                assert!(matches!(receiver.try_recv(), Ok(WriterCommand::Stop)));
                assert!(matches!(
                    receiver.try_recv(),
                    Err(TryRecvError::Disconnected)
                ));
            }
        }
    }

    #[test]
    fn blocking_receive_wakes_on_command_and_sender_disconnect() {
        let (sender, receiver) = channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert!(matches!(receiver.recv(), Ok(WriterCommand::Stop)));
            assert!(receiver.recv().is_err());
            result_tx.send(()).unwrap();
        });
        assert!(sender.send(WriterCommand::Stop).is_ok());
        drop(sender);
        result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        worker.join().unwrap();
    }

    #[cfg_attr(feature = "profile", hotpath::measure)]
    fn warmed_batch_allocation_probe(
        sender: &WriterSender,
        receiver: &WriterReceiver,
        data: &pyo3::Bound<'_, pyo3::types::PyBytes>,
    ) {
        for _ in 0..100 {
            let buffers: [_; 16] = std::array::from_fn(|_| OwnedWriteBuffer::from_python(data));
            assert!(sender.send_batch(buffers).is_ok());
            for _ in 0..16 {
                assert!(matches!(receiver.recv(), Ok(WriterCommand::Data(_))));
            }
        }
    }

    #[cfg(unix)]
    #[cfg_attr(feature = "profile", hotpath::measure)]
    fn warmed_transport_batch_allocation_probe(
        core: &Arc<super::super::StreamTransportCore>,
        receiver: &WriterReceiver,
        segments: &[pyo3::Bound<'_, pyo3::types::PyBytes>; 16],
    ) {
        for _ in 0..100 {
            core.try_write_segments(segments, 16 * 8192).unwrap();
            for _ in 0..16 {
                assert!(matches!(receiver.recv(), Ok(WriterCommand::Data(_))));
            }
            core.record_write_buffer_drained(16 * 8192);
            core.set_write_backpressure_active(false);
            assert_eq!(core.get_write_buffer_size(), 0);
        }
    }

    #[test]
    fn warmed_python_batches_reuse_queue_storage() {
        #[cfg(feature = "hotpath-alloc-profile")]
        let _profile = std::env::var("RSLOOP_ALLOCATION_REPORT").ok().map(|path| {
            hotpath::HotpathGuardBuilder::new("write-batch-allocation-budget")
                .format(hotpath::Format::Json)
                .functions_limit(0)
                .output_path(path)
                .build()
        });
        crate::initialize_python_for_tests();
        pyo3::Python::attach(|py| {
            let (sender, receiver) = channel();
            let data = pyo3::types::PyBytes::new(py, &[7; 1024]);
            let warmup: [_; 16] = std::array::from_fn(|_| OwnedWriteBuffer::from_python(&data));
            assert!(sender.send_batch(warmup).is_ok());
            for _ in 0..16 {
                assert!(matches!(receiver.recv(), Ok(WriterCommand::Data(_))));
            }
            let capacity = sender.shared.state.lock().unwrap().commands.capacity();
            warmed_batch_allocation_probe(&sender, &receiver, &data);
            assert_eq!(
                sender.shared.state.lock().unwrap().commands.capacity(),
                capacity
            );
            #[cfg(unix)]
            {
                use super::super::test_support::{build_test_core, shutdown_test_core};
                let (core, receiver, loop_core, _protocol) = build_test_core(py);
                core.set_write_buffer_limits(Some(1024 * 1024), Some(0))
                    .unwrap();
                let data = pyo3::types::PyBytes::new(py, &[7; 8192]);
                let segments = std::array::from_fn(|_| data.clone());
                // Warm the real transport's queue before enforcing its budget.
                core.try_write_segments(&segments, 16 * 8192).unwrap();
                for _ in 0..16 {
                    assert!(matches!(receiver.recv(), Ok(WriterCommand::Data(_))));
                }
                core.record_write_buffer_drained(16 * 8192);
                core.set_write_backpressure_active(false);
                warmed_transport_batch_allocation_probe(&core, &receiver, &segments);
                shutdown_test_core(core, receiver, loop_core);
            }
        });
    }

    #[test]
    fn large_batch_retains_allocations_and_precedes_shutdown() {
        let (sender, receiver) = channel();
        let first = OwnedWriteBuffer::from_slice(b"first");
        let second = OwnedWriteBuffer::from_slice(b"second");
        let addresses = [first.remaining().as_ptr(), second.remaining().as_ptr()];
        assert!(sender.send_batch(vec![first, second]).is_ok());
        assert!(sender.send(WriterCommand::WriteEof).is_ok());
        for (index, expected) in [b"first".as_slice(), b"second".as_slice()]
            .iter()
            .enumerate()
        {
            let WriterCommand::Data(data) = receiver.recv().unwrap() else {
                panic!("control command overtook batch");
            };
            assert_eq!(data.remaining(), *expected);
            assert_eq!(data.remaining().as_ptr(), addresses[index]);
        }
        assert!(matches!(receiver.recv(), Ok(WriterCommand::WriteEof)));
        drop(receiver);
        assert!(
            sender
                .send_batch(vec![OwnedWriteBuffer::from_slice(b"rejected")])
                .is_err()
        );
        assert!(sender.shared.state.lock().unwrap().commands.is_empty());
    }

    #[test]
    fn queue_reuses_capacity_and_reports_disconnects() {
        let (sender, receiver) = channel();
        assert!(sender.send(WriterCommand::Stop).is_ok());
        assert!(matches!(receiver.recv(), Ok(WriterCommand::Stop)));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        drop(sender);
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn queue_retains_growth_for_the_next_burst() {
        let (sender, receiver) = channel();
        for _ in 0..32 {
            assert!(sender.send(WriterCommand::Stop).is_ok());
        }
        let grown_capacity = sender
            .shared
            .state
            .lock()
            .expect("writer queue")
            .commands
            .capacity();
        for _ in 0..32 {
            assert!(matches!(receiver.recv(), Ok(WriterCommand::Stop)));
        }
        for _ in 0..32 {
            assert!(sender.send(WriterCommand::Stop).is_ok());
        }

        assert_eq!(
            sender
                .shared
                .state
                .lock()
                .expect("writer queue")
                .commands
                .capacity(),
            grown_capacity
        );
    }

    #[test]
    fn queue_coalesces_adjacent_data_without_crossing_control_commands() {
        let (sender, receiver) = channel();
        assert!(
            sender
                .send(WriterCommand::Data(
                    super::super::buffers::OwnedWriteBuffer::from_slice(b"one")
                ))
                .is_ok()
        );
        assert!(
            sender
                .send(WriterCommand::Data(
                    super::super::buffers::OwnedWriteBuffer::from_slice(b"two")
                ))
                .is_ok()
        );
        assert!(sender.send(WriterCommand::WriteEof).is_ok());
        assert!(
            sender
                .send(WriterCommand::Data(
                    super::super::buffers::OwnedWriteBuffer::from_slice(b"three")
                ))
                .is_ok()
        );

        let WriterCommand::Data(data) = receiver.recv().expect("coalesced data") else {
            panic!("expected data");
        };
        assert_eq!(data.remaining(), b"onetwo");
        assert!(matches!(receiver.recv(), Ok(WriterCommand::WriteEof)));
        let WriterCommand::Data(data) = receiver.recv().expect("data after control") else {
            panic!("expected data");
        };
        assert_eq!(data.remaining(), b"three");
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn merge_writer_queue_reports_both_closed_ends() {
        let mut state = QueueState::with_capacity(0);
        state.receiver_alive = false;
        let rejected = state.enqueue(WriterCommand::Stop);
        assert!(matches!(rejected, Err(WriterCommand::Stop)));

        let mut state = QueueState::with_capacity(0);
        state.sender_alive = false;
        assert!(matches!(
            state.try_dequeue(),
            Err(TryRecvError::Disconnected)
        ));
    }

    fn control_tag(command: &WriterCommand) -> u8 {
        match command {
            WriterCommand::WriteEof => 0,
            WriterCommand::Close => 1,
            WriterCommand::Abort => 2,
            WriterCommand::Stop => 3,
            WriterCommand::Data(_) => unreachable!("control-only model produced data"),
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn merge_writer_queue_preserves_control_fifo() {
        let mut state = QueueState::with_capacity(4);
        assert!(state.enqueue(WriterCommand::WriteEof).is_ok());
        assert!(state.enqueue(WriterCommand::Close).is_ok());
        assert!(state.enqueue(WriterCommand::Abort).is_ok());
        assert!(state.enqueue(WriterCommand::Stop).is_ok());

        for expected in 0..4 {
            let Ok(command) = state.try_dequeue() else {
                unreachable!("queued control command disappeared");
            };
            assert_eq!(control_tag(&command), expected);
        }

        assert!(matches!(state.try_dequeue(), Err(TryRecvError::Empty)));
        state.sender_alive = false;
        assert!(matches!(
            state.try_dequeue(),
            Err(TryRecvError::Disconnected)
        ));
    }
}
