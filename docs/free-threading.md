# Free-Threaded CPython

`rsloop` builds and runs on free-threaded CPython 3.14 and 3.15 (`3.14t`, `3.15t`). The extension
declares `#[pymodule(gil_used = false)]`, which keeps CPython from silently
switching the GIL back on for the whole process at import time:

```python
import sys

import rsloop

assert not sys._is_gil_enabled()
assert rsloop.build_info()["free_threaded"]
```

Separate `rsloop.Loop` instances on separate threads can therefore run
concurrently rather than taking turns. A loop is still single-threaded
internally, and asyncio objects are still not thread-safe, so the model is one
loop per thread — not one loop shared across threads. `call_soon_threadsafe()`
remains the supported way to hand work to a loop from another thread, and it
keeps its FIFO ordering guarantee.

The pieces that make this safe include:

- the generic stream-reader fast path writes into `StreamReader._buffer`
  through a raw pointer; the size read, resize, and copy run inside a critical
  section on that `bytearray`, so a concurrent mutation cannot leave the copy
  writing into a freed allocation
- the ready-queue refill preserves scheduling order when a drain slice leaves
  older callbacks in the batch. Under the GIL a cross-thread producer could
  only enqueue while the loop thread was parked, so the reordering was
  essentially unreachable; without the GIL producers append throughout the
  drain and it becomes routine

`tests/test_free_threading.py` covers parallel loops over both the native and
standard-library stream-reader paths, `call_soon_threadsafe()` fan-in from
eight threads, and a check that importing `rsloop` leaves the GIL off.

Wheels are built for `3.14t` and `3.15t` alongside the GIL builds, and the test
matrix runs each as its own entry. Windows ARM64 release jobs retain their
explicit GIL-only interpreter list.

The manual `Python 3.15t wheels` workflow builds release wheels on Linux, macOS
Intel, and Windows x64. It installs each wheel in a clean 3.15t environment,
checks import and `build_info()`, runs a callback/TCP smoke check with the GIL
disabled, and runs the focused free-threading and callback tests. Release wheel
jobs also check their installed 3.15t wheel before upload. To run this locally:

```bash
RSLOOP_PYTHON_VERSIONS=3.15t scripts/build-wheels.sh --out dist/315t -- --locked
uv run --no-project --python 3.15t python scripts/check-free-threaded-wheel.py dist/315t
```

Downstream PyO3 extensions should also follow the guidance in
[Free-threaded interpreters](rust-extensions.md#free-threaded-interpreters).
