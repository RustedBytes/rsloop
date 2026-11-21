"""Async iteration for the native reader, using its existing line semantics."""


async def next_line(reader):
    # Only clean EOF ends iteration. Read errors and cancellation propagate,
    # and an unterminated final line is delivered before StopAsyncIteration.
    line = await reader.readline()
    if not line:
        raise StopAsyncIteration
    return line


import asyncio


class ReadFuture(asyncio.Future):
    """Future of a native stream read that had to wait for data.

    The native reader completes it with the bytes it took from its buffer.
    asyncio's own StreamReader leaves bytes in its buffer until the reading
    coroutine resumes, so a task cancelled after the data arrived (an
    ``asyncio.wait_for`` timeout firing in the same loop iteration, for
    example) loses nothing. Awaiting this Future keeps that guarantee: when
    an exception such as the cancellation is thrown into the ``await`` after
    the Future completed with data, the bytes go back to the front of the
    reader's buffer before the exception propagates.
    """

    __slots__ = ("_rsloop_reader",)

    def __init__(self, *, loop=None):
        super().__init__(loop=loop)
        # A weak reference: a pending read must not keep its reader alive.
        self._rsloop_reader = None

    def __await__(self):
        if not self.done():
            self._asyncio_future_blocking = True
            try:
                yield self
            except BaseException:
                self._restore_undelivered()
                raise
        if not self.done():
            raise RuntimeError("await wasn't used with future")
        return self.result()

    __iter__ = __await__

    def _restore_undelivered(self):
        # Unfinished and cancelled waiters consumed nothing (the reader
        # reconciles cancellation itself); errors carry no data.
        if not self.done() or self.cancelled() or self.exception() is not None:
            return
        ref = self._rsloop_reader
        reader = ref() if ref is not None else None
        if reader is not None:
            reader._restore_undelivered(self.result())
