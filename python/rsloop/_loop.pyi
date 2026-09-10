from asyncio import (
    AbstractEventLoop,
    Future,
    Server,
    StreamReader,
    StreamWriter,
    Task,
)
from collections.abc import Awaitable, Callable, Coroutine, Sequence
from contextvars import Context
from typing import Any, TypeVar

_T = TypeVar("_T")

__version__: str

def build_info() -> dict[str, str | bool]: ...
def transport_stats() -> dict[str, int | bool]: ...
def reset_transport_stats() -> None: ...

class PyLoop(AbstractEventLoop):
    slow_callback_duration: float
    def __init__(self) -> None: ...
    def create_future(self) -> Future[Any]: ...
    def create_task(
        self,
        coro: Coroutine[Any, Any, _T],
        *,
        name: object | None = None,
        context: Context | None = None,
        eager_start: bool | None = None,
        **kwargs: Any,
    ) -> Task[_T]: ...
    def set_slow_callback_duration(self, value: float) -> None: ...

async def start_server(
    client_connected_cb: Callable[[StreamReader, StreamWriter], Awaitable[None]]
    | Callable[[StreamReader, StreamWriter], None],
    host: str | Sequence[str] | None = None,
    port: int | str | None = None,
    *,
    limit: int = 65536,
    ssl_handshake_timeout: float | None = None,
    **kwds: Any,
) -> Server: ...
async def open_connection(
    host: str | None = None,
    port: int | str | None = None,
    *,
    limit: int = 65536,
    ssl_handshake_timeout: float | None = None,
    **kwds: Any,
) -> tuple[StreamReader, StreamWriter]: ...
