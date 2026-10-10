"""Async iteration for the native reader, using its existing line semantics."""


async def next_line(reader):
    # Only clean EOF ends iteration. Read errors and cancellation propagate,
    # and an unterminated final line is delivered before StopAsyncIteration.
    line = await reader.readline()
    if not line:
        raise StopAsyncIteration
    return line
