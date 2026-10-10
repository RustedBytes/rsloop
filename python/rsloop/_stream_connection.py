"""Keep native stream result assembly on the caller's event loop thread."""


async def open_connection(awaitable, finish):
    # A direct await propagates cancellation into the connection operation.
    # There is no suspension between receiving the transport and owning it in
    # a writer, nor a worker-thread protocol borrow racing data/EOF callbacks.
    return finish(await awaitable)
