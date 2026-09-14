# Verified Surface Area

The current codebase implements these user-facing areas.

## Loop lifecycle and scheduling

- `run_forever`, `run_until_complete`, `stop`, `close`
- `time`, `is_running`, `is_closed`
- `get_debug`, `set_debug`
- `call_soon`, `call_soon_threadsafe`, `call_later`, `call_at`
- returned `Handle` and `TimerHandle` objects with `cancel()` / `cancelled()`

## Tasks, futures, and execution helpers

- `create_future`, `create_task`
- `set_task_factory`, `get_task_factory`
- `set_exception_handler`, `get_exception_handler`,
  `call_exception_handler`, `default_exception_handler`
- `set_default_executor`, `run_in_executor`
- `shutdown_asyncgens`, `shutdown_default_executor`
- callback execution under captured `contextvars.Context`
- `asyncio.get_running_loop()` support while running on `rsloop`
- `rsloop.run(...)` helper, with `asyncio.run(..., loop_factory=...)`
  integration on Python 3.12+

## I/O and networking

- `add_reader`, `remove_reader`, `add_writer`, `remove_writer`
- `sock_recv`, `sock_recv_into`, `sock_sendall`, `sock_accept`, `sock_connect`
- `getaddrinfo`, `getnameinfo`
- `create_server`, `create_connection`
- `create_unix_server`, `create_unix_connection`
- `connect_accepted_socket`
- returned `Server` objects with `close()`, `is_serving()`, `get_loop()`,
  and `sockets()`
- returned `StreamTransport` objects with `write()`, `writelines()`, `close()`,
  `abort()`, `is_closing()`, `write_eof()`, `can_write_eof()`,
  `get_extra_info()`, `get_protocol()`, `set_protocol()`,
  `pause_reading()`, `resume_reading()`, `is_reading()`

## Pipes, subprocesses, and signals

- `connect_read_pipe`, `connect_write_pipe`
- `subprocess_exec`, `subprocess_shell`
- returned `ProcessTransport` and `ProcessPipeTransport` objects
- higher-level compatibility with `asyncio.create_subprocess_exec()` and
  `asyncio.create_subprocess_shell()`
- Unix subprocess options including `cwd`, `env`, `executable`, `pass_fds`,
  `start_new_session`, `process_group`, `user`, `group`, `extra_groups`,
  `umask`, and `restore_signals`
- `add_signal_handler`, `remove_signal_handler`

## Profiling and diagnostics

- Python 3.15's external `profiling.sampling` profiler
- opt-in transport counters through `transport_stats()` and
  `reset_transport_stats()`

Set `RSLOOP_TRANSPORT_STATS=1` before importing rsloop to enable the transport
counters. They report read completions and bytes, Python-thread read drains,
wakeups, staged and direct writes, and Windows completion-to-poll rebinds.
Counters remain disabled by default so diagnostics add only one predictable
branch to transport hot paths.

For profiling commands and profiler limitations, see
[Development: Profiling](development.md#profiling).
