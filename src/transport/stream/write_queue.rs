//! Allocation-stable command queue for a transport writer worker.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use super::WriterCommand;

const INITIAL_WRITER_QUEUE_CAPACITY: usize = 8;

struct QueueState {
    commands: VecDeque<WriterCommand>,
    sender_alive: bool,
    receiver_alive: bool,
}

impl QueueState {
    fn new() -> Self {
        Self::with_capacity(INITIAL_WRITER_QUEUE_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            commands: VecDeque::with_capacity(capacity),
            sender_alive: true,
            receiver_alive: true,
        }
    }

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
    pub(super) fn send(&self, command: WriterCommand) -> Result<(), WriterCommand> {
        let mut state = self.shared.state.lock().expect("poisoned writer queue");
        state.enqueue(command)?;
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }
}

impl Drop for WriterSender {
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
    pub(super) fn recv(&self) -> Result<WriterCommand, ()> {
        let mut state = self.shared.state.lock().expect("poisoned writer queue");
        loop {
            if let Some(command) = state.commands.pop_front() {
                return Ok(command);
            }
            if !state.sender_alive {
                return Err(());
            }
            state = self
                .shared
                .ready
                .wait(state)
                .expect("poisoned writer queue");
        }
    }

    pub(super) fn try_recv(&self) -> Result<WriterCommand, TryRecvError> {
        self.shared
            .state
            .lock()
            .expect("poisoned writer queue")
            .try_dequeue()
    }
}

impl Drop for WriterReceiver {
    fn drop(&mut self) {
        // Serialize disconnection with enqueue so later sends are rejected.
        // This guard only updates queue metadata; it does not wait for the
        // worker to finish. Contention is possible, but cleanup must be synchronous.
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
