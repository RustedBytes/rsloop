from __future__ import annotations

import asyncio as __asyncio
import contextlib as __contextlib
import os as __os
import socket as __socket
import ssl as __ssl
import sys as __sys
import typing as __typing

from ._loop import PyLoop as Loop
from ._loop import __version__
from ._loop import new_event_loop
from ._loop import start_server as __start_server
from ._loop import open_connection as __open_connection
from ._loop import profiler_running
from ._loop import start_profiler
from ._loop import stop_profiler

__DLL_DIR_HANDLES: list[object] = []


def __configure_windows_dll_search_path() -> None:
    if __sys.platform != "win32" or not hasattr(__os, "add_dll_directory"):
        return

    import shutil as __shutil

    candidate_dirs: list[str] = []
    gcc = __shutil.which("gcc")
    if gcc is not None:
        candidate_dirs.append(__os.path.dirname(gcc))

    msys_prefix = __os.environ.get("MSYSTEM_PREFIX")
    if msys_prefix:
        candidate_dirs.append(__os.path.join(msys_prefix, "bin"))

    candidate_dirs.extend(
        [
            r"C:\msys64\ucrt64\bin",
            r"C:\msys64\mingw64\bin",
            r"C:\msys64\clang64\bin",
        ]
    )

    seen: set[str] = set()
    for directory in candidate_dirs:
        normalized = __os.path.normcase(__os.path.abspath(directory))
        if normalized in seen or not __os.path.isdir(directory):
            continue
        if not __os.path.exists(__os.path.join(directory, "libstdc++-6.dll")):
            continue
        __DLL_DIR_HANDLES.append(__os.add_dll_directory(directory))
        seen.add(normalized)


__configure_windows_dll_search_path()

__all__: tuple[str, ...] = (
    "Loop",
    "__version__",
    "new_event_loop",
    "profile",
    "profiler_running",
    "run",
    "start_profiler",
    "stop_profiler",
)
_T = __typing.TypeVar("_T")
__ORIG_SET_EVENT_LOOP = __asyncio.set_event_loop


def __get_event_loop_policy():
    getter = getattr(__asyncio.events, "_get_event_loop_policy", None)
    if getter is not None:
        return getter()
    return __asyncio.get_event_loop_policy()


def __set_event_loop(loop: Loop | None) -> None:
    try:
        __ORIG_SET_EVENT_LOOP(loop)
        return
    except (AssertionError, TypeError):
        if loop is None or not isinstance(loop, Loop):
            raise

    # Python 3.8 rejects non-stdlib loop objects in set_event_loop() with a
    # hard isinstance() assertion. Mirror the stdlib policy bookkeeping so
    # get_event_loop() still returns the current rsloop instance.
    policy = __get_event_loop_policy()
    local = getattr(policy, "_local", None)
    if local is None:
        raise
    local._set_called = True
    local._loop = loop


if __asyncio.set_event_loop is __ORIG_SET_EVENT_LOOP:
    __asyncio.set_event_loop = __set_event_loop
    __asyncio.events.set_event_loop = __set_event_loop


@__contextlib.contextmanager
def profile() -> __typing.Iterator[None]:
    """Context manager wrapper around ``start_profiler()`` / ``stop_profiler()``."""
    start_profiler()
    try:
        yield None
    finally:
        stop_profiler()


__ORIG_OPEN_CONNECTION = __asyncio.open_connection
__ORIG_START_SERVER = __asyncio.start_server
__ORIG_CREATE_SUBPROCESS_EXEC = __asyncio.create_subprocess_exec
__ORIG_CREATE_SUBPROCESS_SHELL = __asyncio.create_subprocess_shell
__ORIG_CREATE_CONNECTION = Loop.create_connection
__ORIG_CREATE_DATAGRAM_ENDPOINT = getattr(Loop, "create_datagram_endpoint", None)
__ORIG_SENDFILE = getattr(Loop, "sendfile", None)
__ORIG_SOCK_RECVFROM = getattr(Loop, "sock_recvfrom", None)
__ORIG_SOCK_RECVFROM_INTO = getattr(Loop, "sock_recvfrom_into", None)
__ORIG_SOCK_SENDTO = getattr(Loop, "sock_sendto", None)
__ORIG_SOCK_SENDFILE = getattr(Loop, "sock_sendfile", None)
__USE_FAST_STREAMS = __os.environ.get("RSLOOP_USE_FAST_STREAMS", "1") != "0"

if __USE_FAST_STREAMS and __asyncio.open_connection is __ORIG_OPEN_CONNECTION:
    __asyncio.open_connection = __open_connection
if __USE_FAST_STREAMS and __asyncio.start_server is __ORIG_START_SERVER:
    __asyncio.start_server = __start_server

_asyncio = __asyncio
_os = __os


def __cancel_all_tasks(loop: Loop) -> None:
    to_cancel = __asyncio.all_tasks(loop)
    if not to_cancel:
        return

    for task in to_cancel:
        task.cancel()

    loop.run_until_complete(__asyncio.gather(*to_cancel, return_exceptions=True))

    for task in to_cancel:
        if task.cancelled():
            continue
        exception = task.exception()
        if exception is None:
            continue
        loop.call_exception_handler(
            {
                "message": "unhandled exception during rsloop.run() shutdown",
                "exception": exception,
                "task": task,
            }
        )


class __RsloopDatagramTransport:
    max_size = 256 * 1024

    def __init__(self, loop: Loop, sock, protocol, address=None, waiter=None):
        import collections as __collections

        self._loop = loop
        self._sock = sock
        self._protocol = protocol
        self._address = address
        self._buffer = __collections.deque()
        self._buffer_size = 0
        self._closing = False
        self._conn_lost = 0
        self._writer_task = None
        self._extra = {
            "socket": sock,
            "sockname": sock.getsockname(),
            "peername": self.__peername(),
        }
        self._protocol_paused = False
        self._high_water = 64 * 1024
        self._low_water = 16 * 1024
        self._reader_task = self._loop.create_task(self._read_loop())
        self._loop.call_soon(self._protocol.connection_made, self)
        if waiter is not None:
            self._loop.call_soon(waiter.set_result, None)

    def __peername(self):
        try:
            return self._sock.getpeername()
        except OSError:
            return None

    def get_extra_info(self, name, default=None):
        return self._extra.get(name, default)

    def get_protocol(self):
        return self._protocol

    def set_protocol(self, protocol):
        self._protocol = protocol

    def is_closing(self):
        return self._closing

    def close(self):
        if self._closing:
            return
        self._closing = True
        self._reader_task.cancel()
        if not self._buffer:
            self._loop.call_soon(self._call_connection_lost, None)

    def abort(self):
        self._force_close(None)

    def get_write_buffer_size(self):
        return self._buffer_size

    def _maybe_pause_protocol(self):
        if self._buffer_size > self._high_water and not self._protocol_paused:
            self._protocol_paused = True
            self._protocol.pause_writing()

    def _maybe_resume_protocol(self):
        if self._protocol_paused and self._buffer_size <= self._low_water:
            self._protocol_paused = False
            self._protocol.resume_writing()

    def sendto(self, data, addr=None):
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise TypeError(
                f"data argument must be a bytes-like object, not {type(data).__name__!r}"
            )
        if self._address is not None:
            if addr not in (None, self._address):
                raise ValueError(f"Invalid address: must be None or {self._address}")
            addr = self._address

        if not self._buffer:
            try:
                if self._extra["peername"] is not None:
                    self._sock.send(data)
                else:
                    self._sock.sendto(data, addr)
                return
            except (BlockingIOError, InterruptedError):
                if self._writer_task is None:
                    self._writer_task = self._loop.create_task(
                        self._flush_write_buffer()
                    )
            except OSError as exc:
                self._protocol.error_received(exc)
                return

        payload = bytes(data)
        self._buffer.append((payload, addr))
        self._buffer_size += len(payload)
        self._maybe_pause_protocol()

    async def _read_loop(self):
        while not self._closing:
            try:
                data, addr = await self._loop.sock_recvfrom(self._sock, self.max_size)
            except _asyncio.CancelledError:
                return
            except OSError as exc:
                self._protocol.error_received(exc)
                continue
            self._protocol.datagram_received(data, addr)

    async def _flush_write_buffer(self):
        try:
            while self._buffer and not self._closing:
                data, addr = self._buffer[0]
                try:
                    if self._extra["peername"] is not None:
                        self._sock.send(data)
                    else:
                        self._sock.sendto(data, addr)
                except (BlockingIOError, InterruptedError):
                    await __wait_for_fd(self._loop, self._sock, readable=False)
                    continue
                except OSError as exc:
                    self._protocol.error_received(exc)
                    return
                self._buffer.popleft()
                self._buffer_size -= len(data)
                self._maybe_resume_protocol()
        finally:
            self._writer_task = None
            if self._closing and not self._buffer:
                self._call_connection_lost(None)

    def _force_close(self, exc):
        if self._conn_lost:
            return
        self._buffer.clear()
        self._buffer_size = 0
        self._closing = True
        self._reader_task.cancel()
        if self._writer_task is not None:
            self._writer_task.cancel()
        self._call_connection_lost(exc)

    def _call_connection_lost(self, exc):
        if self._conn_lost:
            return
        self._conn_lost += 1
        try:
            self._protocol.connection_lost(exc)
        finally:
            self._sock.close()


async def __wait_for_fd(loop: Loop, sock, *, readable: bool) -> None:
    fut = loop.create_future()
    callback = loop.add_reader if readable else loop.add_writer
    remove = loop.remove_reader if readable else loop.remove_writer

    def ready() -> None:
        if not fut.done():
            fut.set_result(None)

    callback(sock, ready)
    try:
        await fut
    finally:
        remove(sock)


async def __loop_sock_recvfrom(self, sock, bufsize):
    while True:
        try:
            return sock.recvfrom(bufsize)
        except (BlockingIOError, InterruptedError):
            await __wait_for_fd(self, sock, readable=True)


async def __loop_sock_recvfrom_into(self, sock, buf, nbytes=0):
    while True:
        try:
            if nbytes:
                return sock.recvfrom_into(buf, nbytes)
            return sock.recvfrom_into(buf)
        except (BlockingIOError, InterruptedError):
            await __wait_for_fd(self, sock, readable=True)


async def __loop_sock_sendto(self, sock, data, address):
    while True:
        try:
            return sock.sendto(data, address)
        except (BlockingIOError, InterruptedError):
            await __wait_for_fd(self, sock, readable=False)


async def __loop_sendfile(
    self, transport, file, offset=0, count=None, *, fallback=True
):
    if transport.is_closing():
        raise RuntimeError("Transport is closing")
    if not fallback:
        raise RuntimeError(
            f"fallback is disabled and native sendfile is not supported for transport {transport!r}"
        )

    if offset:
        file.seek(offset)
    blocksize = min(count, 16384) if count else 16384
    buf = bytearray(blocksize)
    total_sent = 0

    async def drain_transport() -> None:
        while transport.get_write_buffer_size() > 0:
            if transport.is_closing():
                raise ConnectionError("Connection closed by peer")
            await __asyncio.sleep(0)

    try:
        while True:
            if count is not None:
                blocksize = min(count - total_sent, blocksize)
                if blocksize <= 0:
                    return total_sent
            view = memoryview(buf)[:blocksize]
            read = await self.run_in_executor(None, file.readinto, view)
            if not read:
                return total_sent
            transport.write(view[:read])
            await drain_transport()
            total_sent += read
    finally:
        if total_sent > 0 and hasattr(file, "seek"):
            file.seek(offset + total_sent)


async def __loop_sock_sendfile(
    self, sock, file, offset=0, count=None, *, fallback=True
):
    if sock.gettimeout() != 0:
        raise ValueError("the socket must be non-blocking")
    if "b" not in getattr(file, "mode", "b"):
        raise ValueError("file should be opened in binary mode")
    if sock.type != __socket.SOCK_STREAM:
        raise ValueError("only SOCK_STREAM type sockets are supported")
    if count is not None:
        if not isinstance(count, int):
            raise TypeError(f"count must be a positive integer (got {count!r})")
        if count <= 0:
            raise ValueError(f"count must be a positive integer (got {count!r})")
    if not isinstance(offset, int):
        raise TypeError(f"offset must be a non-negative integer (got {offset!r})")
    if offset < 0:
        raise ValueError(f"offset must be a non-negative integer (got {offset!r})")
    if not fallback:
        raise RuntimeError(
            f"fallback is disabled and native sendfile is not supported for socket {sock!r}"
        )

    if offset:
        file.seek(offset)
    blocksize = min(count, 16384) if count else 16384
    buf = bytearray(blocksize)
    total_sent = 0
    try:
        while True:
            if count:
                blocksize = min(count - total_sent, blocksize)
                if blocksize <= 0:
                    break
            view = memoryview(buf)[:blocksize]
            read = await self.run_in_executor(None, file.readinto, view)
            if not read:
                break
            await self.sock_sendall(sock, view[:read])
            total_sent += read
        return total_sent
    finally:
        if total_sent > 0 and hasattr(file, "seek"):
            file.seek(offset + total_sent)


async def __loop_create_datagram_endpoint(
    self,
    protocol_factory,
    local_addr=None,
    remote_addr=None,
    *,
    family=0,
    proto=0,
    flags=0,
    reuse_port=None,
    allow_broadcast=None,
    sock=None,
):
    resolved_remote_addr = None
    if sock is not None and (local_addr is not None or remote_addr is not None):
        raise ValueError(
            "socket modifier keyword arguments can not be used when sock is specified"
        )
    if sock is None and local_addr is None and remote_addr is None:
        raise ValueError("unexpected address family")

    if sock is None:
        addrinfos = None
        if remote_addr is not None:
            addrinfos = await self.getaddrinfo(
                *remote_addr,
                family=family,
                type=__socket.SOCK_DGRAM,
                proto=proto,
                flags=flags,
            )
        elif local_addr is not None:
            addrinfos = await self.getaddrinfo(
                *local_addr,
                family=family,
                type=__socket.SOCK_DGRAM,
                proto=proto,
                flags=flags,
            )
        if not addrinfos:
            raise OSError("getaddrinfo() returned empty list")

        family, socktype, proto, _, sockaddr = addrinfos[0]
        if remote_addr is not None:
            resolved_remote_addr = sockaddr
        sock = __socket.socket(family=family, type=socktype, proto=proto)
        sock.setblocking(False)
        if reuse_port:
            if not hasattr(__socket, "SO_REUSEPORT"):
                raise ValueError("reuse_port not supported by socket module")
            sock.setsockopt(__socket.SOL_SOCKET, __socket.SO_REUSEPORT, 1)
        if allow_broadcast:
            sock.setsockopt(__socket.SOL_SOCKET, __socket.SO_BROADCAST, 1)
        if local_addr is not None:
            sock.bind(local_addr)
        elif remote_addr is not None:
            if family == __socket.AF_INET6:
                sock.bind(("::", 0))
            else:
                sock.bind(("0.0.0.0", 0))
    else:
        sock.setblocking(False)

    waiter = self.create_future()
    protocol = protocol_factory()
    address = resolved_remote_addr or remote_addr
    if address is None and sock is not None:
        try:
            address = sock.getpeername()
        except OSError:
            address = None

    transport = __RsloopDatagramTransport(
        self,
        sock,
        protocol,
        address=address,
        waiter=waiter,
    )
    await waiter
    return transport, protocol


def __subprocess_text_requested(kwds: dict[str, object]) -> bool:
    return bool(
        kwds.get("universal_newlines")
        or kwds.get("text") is True
        or kwds.get("encoding") is not None
        or kwds.get("errors") is not None
    )


class _TextStreamReader:
    def __init__(self, limit=_asyncio.streams._DEFAULT_LIMIT, loop=None):
        if limit <= 0:
            raise ValueError("Limit cannot be <= 0")

        self._limit = limit
        self._loop = _asyncio.events.get_event_loop() if loop is None else loop
        self._buffer = ""
        self._eof = False
        self._waiter = None
        self._exception = None
        self._transport = None
        self._paused = False

    def __repr__(self):
        return (
            f"<{self.__class__.__name__} "
            f"eof={self._eof} "
            f"limit={self._limit} "
            f"transport={self._transport!r}>"
        )

    def exception(self):
        return self._exception

    def set_exception(self, exc):
        self._exception = exc
        waiter = self._waiter
        if waiter is not None:
            self._waiter = None
            if not waiter.cancelled():
                waiter.set_exception(exc)

    def set_transport(self, transport):
        assert self._transport is None, "Transport already set"
        self._transport = transport

    def feed_eof(self):
        self._eof = True
        waiter = self._waiter
        if waiter is not None:
            self._waiter = None
            if not waiter.cancelled():
                waiter.set_result(None)

    def feed_data(self, data):
        assert not self._eof, "feed_data after feed_eof"

        if not data:
            return

        self._buffer += data
        waiter = self._waiter
        if waiter is not None:
            self._waiter = None
            if not waiter.cancelled():
                waiter.set_result(None)

        if (
            self._transport is not None
            and not self._paused
            and len(self._buffer) > 2 * self._limit
        ):
            try:
                self._transport.pause_reading()
            except NotImplementedError:
                self._transport = None
            else:
                self._paused = True

    def at_eof(self):
        return self._eof and not self._buffer

    def __aiter__(self):
        return self

    async def __anext__(self):
        value = await self.readline()
        if value == "":
            raise StopAsyncIteration
        return value

    def _maybe_resume_transport(self):
        if self._paused and len(self._buffer) <= self._limit:
            self._paused = False
            self._transport.resume_reading()

    async def _wait_for_data(self, func_name):
        if self._waiter is not None:
            raise RuntimeError(
                f"{func_name}() called while another coroutine is already "
                "waiting for incoming data"
            )

        assert not self._eof, "_wait_for_data after EOF"

        if self._paused:
            self._paused = False
            self._transport.resume_reading()

        self._waiter = self._loop.create_future()
        try:
            await self._waiter
        finally:
            self._waiter = None

    async def read(self, n=-1):
        if self._exception is not None:
            raise self._exception

        if n == 0:
            return ""

        if n < 0:
            blocks = []
            while True:
                block = await self.read(self._limit)
                if not block:
                    break
                blocks.append(block)
            return "".join(blocks)

        if not self._buffer and not self._eof:
            await self._wait_for_data("read")

        data = self._buffer[:n]
        self._buffer = self._buffer[n:]
        self._maybe_resume_transport()
        return data

    async def readline(self):
        try:
            return await self.readuntil("\n")
        except _asyncio.exceptions.IncompleteReadError as exc:
            return exc.partial
        except _asyncio.exceptions.LimitOverrunError as exc:
            if self._buffer.startswith("\n", exc.consumed):
                self._buffer = self._buffer[exc.consumed + 1 :]
            else:
                self._buffer = ""
            self._maybe_resume_transport()
            raise ValueError(exc.args[0]) from exc

    async def readuntil(self, separator="\n"):
        if not isinstance(separator, str):
            raise TypeError("separator must be str")
        if not separator:
            raise ValueError("Separator should be at least one-character string")

        if self._exception is not None:
            raise self._exception

        offset = 0
        separator_length = len(separator)

        while True:
            buffer_length = len(self._buffer)
            if buffer_length - offset >= separator_length:
                index = self._buffer.find(separator, offset)
                if index != -1:
                    break
                offset = buffer_length + 1 - separator_length
                if offset > self._limit:
                    raise _asyncio.exceptions.LimitOverrunError(
                        "Separator is not found, and chunk exceed the limit",
                        offset,
                    )

            if self._eof:
                chunk = self._buffer
                self._buffer = ""
                raise _asyncio.exceptions.IncompleteReadError(chunk, None)

            await self._wait_for_data("readuntil")

        if index > self._limit:
            raise _asyncio.exceptions.LimitOverrunError(
                "Separator is found, but chunk is longer than limit",
                index,
            )

        chunk = self._buffer[: index + separator_length]
        self._buffer = self._buffer[index + separator_length :]
        self._maybe_resume_transport()
        return chunk

    async def readexactly(self, n):
        if n < 0:
            raise ValueError("readexactly size can not be less than zero")

        if self._exception is not None:
            raise self._exception

        if n == 0:
            return ""

        while len(self._buffer) < n:
            if self._eof:
                partial = self._buffer
                self._buffer = ""
                raise _asyncio.exceptions.IncompleteReadError(partial, n)
            await self._wait_for_data("readexactly")

        data = self._buffer[:n]
        self._buffer = self._buffer[n:]
        self._maybe_resume_transport()
        return data


class _TextStreamWriter:
    def __init__(self, writer, encoding, errors):
        self._writer = writer
        self._encoding = encoding
        self._errors = errors

    def __repr__(self):
        return f"<{self.__class__.__name__} writer={self._writer!r}>"

    @property
    def transport(self):
        return self._writer.transport

    def write(self, data):
        if not isinstance(data, str):
            raise TypeError("text-mode subprocess stdin expects str input")
        self._writer.write(data.encode(self._encoding, self._errors))

    def writelines(self, data):
        for chunk in data:
            self.write(chunk)

    def write_eof(self):
        return self._writer.write_eof()

    def can_write_eof(self):
        return self._writer.can_write_eof()

    def close(self):
        return self._writer.close()

    def is_closing(self):
        return self._writer.is_closing()

    async def wait_closed(self):
        await self._writer.wait_closed()

    def get_extra_info(self, name, default=None):
        return self._writer.get_extra_info(name, default)

    async def drain(self):
        await self._writer.drain()


class _TextNewlineDecoder:
    def __init__(self, encoding, errors):
        import codecs as __codecs

        self._decoder = __codecs.getincrementaldecoder(encoding)(errors)
        self._pending = ""

    def decode(self, data, final=False):
        text = self._pending + self._decoder.decode(data, final)
        if final:
            self._pending = ""
            return self._translate(text)

        pending_len = 0
        for char in reversed(text[-2:]):
            if char != "\r":
                break
            pending_len += 1
        if pending_len:
            self._pending = text[-pending_len:]
            text = text[:-pending_len]
        else:
            self._pending = ""
        return self._translate(text)

    @staticmethod
    def _translate(text):
        return text.replace("\r\r\n", "\n").replace("\r\n", "\n").replace("\r", "\n")


class _TextSubprocessStreamProtocol(
    _asyncio.streams.FlowControlMixin,
    _asyncio.protocols.SubprocessProtocol,
):
    def __init__(self, limit, loop, encoding, errors):
        super().__init__(loop=loop)
        self._limit = limit
        self._encoding = encoding
        self._errors = errors
        self._stdout_decoder = None
        self._stderr_decoder = None
        self.stdin = self.stdout = self.stderr = None
        self._transport = None
        self._process_exited = False
        self._pipe_fds = []
        self._stdin_closed = self._loop.create_future()

    def __repr__(self):
        info = [self.__class__.__name__]
        if self.stdin is not None:
            info.append(f"stdin={self.stdin!r}")
        if self.stdout is not None:
            info.append(f"stdout={self.stdout!r}")
        if self.stderr is not None:
            info.append(f"stderr={self.stderr!r}")
        return "<{}>".format(" ".join(info))

    def connection_made(self, transport):
        self._transport = transport

        stdout_transport = transport.get_pipe_transport(1)
        if stdout_transport is not None:
            self.stdout = _TextStreamReader(limit=self._limit, loop=self._loop)
            self.stdout.set_transport(stdout_transport)
            self._stdout_decoder = _TextNewlineDecoder(self._encoding, self._errors)
            self._pipe_fds.append(1)

        stderr_transport = transport.get_pipe_transport(2)
        if stderr_transport is not None:
            self.stderr = _TextStreamReader(limit=self._limit, loop=self._loop)
            self.stderr.set_transport(stderr_transport)
            self._stderr_decoder = _TextNewlineDecoder(self._encoding, self._errors)
            self._pipe_fds.append(2)

        stdin_transport = transport.get_pipe_transport(0)
        if stdin_transport is not None:
            writer = _asyncio.streams.StreamWriter(
                stdin_transport,
                protocol=self,
                reader=None,
                loop=self._loop,
            )
            self.stdin = _TextStreamWriter(
                writer,
                encoding=self._encoding,
                errors=self._errors,
            )

    def pipe_data_received(self, fd, data):
        if fd == 1:
            reader = self.stdout
            decoder = self._stdout_decoder
        elif fd == 2:
            reader = self.stderr
            decoder = self._stderr_decoder
        else:
            reader = None
            decoder = None
        if reader is not None and decoder is not None:
            reader.feed_data(decoder.decode(data))

    def pipe_connection_lost(self, fd, exc):
        if fd == 0:
            pipe = self.stdin
            if pipe is not None:
                pipe.close()
            self.connection_lost(exc)
            if exc is None:
                self._stdin_closed.set_result(None)
            else:
                self._stdin_closed.set_exception(exc)
                self._stdin_closed._log_traceback = False
            return
        if fd == 1:
            reader = self.stdout
        elif fd == 2:
            reader = self.stderr
        else:
            reader = None
        if reader is not None:
            if exc is None:
                decoder = self._stdout_decoder if fd == 1 else self._stderr_decoder
                if decoder is not None:
                    tail = decoder.decode(b"", final=True)
                    if tail:
                        reader.feed_data(tail)
                reader.feed_eof()
            else:
                reader.set_exception(exc)

        if fd in self._pipe_fds:
            self._pipe_fds.remove(fd)
        self._maybe_close_transport()

    def process_exited(self):
        self._process_exited = True
        self._maybe_close_transport()

    def _maybe_close_transport(self):
        if len(self._pipe_fds) == 0 and self._process_exited:
            self._transport.close()
            self._transport = None

    def _get_close_waiter(self, stream):
        if self.stdin is stream or getattr(self.stdin, "_writer", None) is stream:
            return self._stdin_closed


def __subprocess_text_config(kwds: dict[str, object]) -> tuple[bool, str, str]:
    import locale as __locale

    text_enabled = __subprocess_text_requested(kwds)
    encoding = kwds.get("encoding")
    errors = kwds.get("errors")
    if encoding is None:
        encoding = __locale.getpreferredencoding(False)
    if errors is None:
        errors = "strict"
    return text_enabled, str(encoding), str(errors)


def __without_text_kwds(kwds: dict[str, object]) -> dict[str, object]:
    filtered = dict(kwds)
    filtered.pop("text", None)
    filtered.pop("encoding", None)
    filtered.pop("errors", None)
    filtered.pop("universal_newlines", None)
    return filtered


def __windows_command_line_to_argv(cmd: str) -> list[str]:
    import ctypes as __ctypes

    argc = __ctypes.c_int()
    command_line_to_argv = __ctypes.windll.shell32.CommandLineToArgvW
    command_line_to_argv.argtypes = [
        __ctypes.c_wchar_p,
        __ctypes.POINTER(__ctypes.c_int),
    ]
    command_line_to_argv.restype = __ctypes.POINTER(__ctypes.c_wchar_p)
    argv = command_line_to_argv(cmd, __ctypes.byref(argc))
    if not argv:
        raise OSError("CommandLineToArgvW failed")
    try:
        return [argv[index] for index in range(argc.value)]
    finally:
        __ctypes.windll.kernel32.LocalFree(argv)


async def __create_text_subprocess_exec(
    program,
    *args,
    stdin=None,
    stdout=None,
    stderr=None,
    limit=_asyncio.streams._DEFAULT_LIMIT,
    **kwds,
):
    loop = _asyncio.events.get_running_loop()
    text_enabled, encoding, errors = __subprocess_text_config(kwds)
    if not text_enabled or not isinstance(loop, Loop):
        return await __ORIG_CREATE_SUBPROCESS_EXEC(
            program,
            *args,
            stdin=stdin,
            stdout=stdout,
            stderr=stderr,
            limit=limit,
            **kwds,
        )

    def protocol_factory():
        return _TextSubprocessStreamProtocol(
            limit=limit,
            loop=loop,
            encoding=encoding,
            errors=errors,
        )

    raw_kwds = __without_text_kwds(kwds)
    transport, protocol = await loop.subprocess_exec(
        protocol_factory,
        program,
        *args,
        stdin=stdin,
        stdout=stdout,
        stderr=stderr,
        **raw_kwds,
    )
    return _asyncio.subprocess.Process(transport, protocol, loop)


async def __create_text_subprocess_shell(
    cmd,
    stdin=None,
    stdout=None,
    stderr=None,
    limit=_asyncio.streams._DEFAULT_LIMIT,
    **kwds,
):
    loop = _asyncio.events.get_running_loop()
    text_enabled, encoding, errors = __subprocess_text_config(kwds)
    if not text_enabled or not isinstance(loop, Loop):
        return await __ORIG_CREATE_SUBPROCESS_SHELL(
            cmd,
            stdin=stdin,
            stdout=stdout,
            stderr=stderr,
            limit=limit,
            **kwds,
        )

    def protocol_factory():
        return _TextSubprocessStreamProtocol(
            limit=limit,
            loop=loop,
            encoding=encoding,
            errors=errors,
        )

    raw_kwds = __without_text_kwds(kwds)
    if _os.name == "nt":
        try:
            argv = __windows_command_line_to_argv(cmd)
        except OSError:
            argv = None
        if argv:
            transport, protocol = await loop.subprocess_exec(
                protocol_factory,
                argv[0],
                *argv[1:],
                stdin=stdin,
                stdout=stdout,
                stderr=stderr,
                **raw_kwds,
            )
            return _asyncio.subprocess.Process(transport, protocol, loop)
    transport, protocol = await loop.subprocess_shell(
        protocol_factory,
        cmd,
        stdin=stdin,
        stdout=stdout,
        stderr=stderr,
        **raw_kwds,
    )
    return _asyncio.subprocess.Process(transport, protocol, loop)


if __asyncio.create_subprocess_exec is __ORIG_CREATE_SUBPROCESS_EXEC:
    __asyncio.create_subprocess_exec = __create_text_subprocess_exec
if __asyncio.create_subprocess_shell is __ORIG_CREATE_SUBPROCESS_SHELL:
    __asyncio.create_subprocess_shell = __create_text_subprocess_shell


def __interleave_addrinfos(addrinfos, first_address_family_count=1):
    import collections as __collections
    import itertools as __itertools

    grouped = __collections.OrderedDict()
    for addrinfo in addrinfos:
        grouped.setdefault(addrinfo[0], []).append(addrinfo)

    lists = list(grouped.values())
    reordered = []
    if first_address_family_count > 1 and lists:
        reordered.extend(lists[0][: first_address_family_count - 1])
        del lists[0][: first_address_family_count - 1]

    for addrinfo in __itertools.zip_longest(*lists):
        reordered.extend(item for item in addrinfo if item is not None)
    return reordered


def __flatten_connection_exceptions(exceptions):
    return [exc for group in exceptions for exc in group]


def __raise_connection_error(exceptions, *, all_errors):
    if all_errors:
        try:
            exc_group = ExceptionGroup
        except NameError:
            exc_group = None
        if exc_group is not None:
            raise exc_group("create_connection failed", exceptions)
    if len(exceptions) == 1:
        raise exceptions[0]

    model = str(exceptions[0])
    if all(str(exc) == model for exc in exceptions):
        raise exceptions[0]

    raise OSError("Multiple exceptions: " + ", ".join(str(exc) for exc in exceptions))


def __bind_error(address, exc):
    detail = exc.strerror.lower() if exc.strerror else str(exc)
    return OSError(
        exc.errno,
        f"error while attempting to bind on address {address!r}: {detail}",
    )


def __prepare_stream_socket(addrinfo, local_addrinfos):
    family, socktype, proto, _, address = addrinfo
    sock = __socket.socket(family=family, type=socktype, proto=proto)
    sock.setblocking(False)
    attempt_exceptions = []
    if local_addrinfos is None:
        return sock, address, attempt_exceptions

    for local_family, _, _, _, local_address in local_addrinfos:
        if local_family != family:
            continue
        try:
            sock.bind(local_address)
            return sock, address, attempt_exceptions
        except OSError as exc:
            attempt_exceptions.append(__bind_error(local_address, exc))

    sock.close()
    if attempt_exceptions:
        return None, None, attempt_exceptions
    return (
        None,
        None,
        [OSError(f"no matching local address with family={family} found")],
    )


def __consume_connection_attempts(done, pending, exceptions):
    for task in done:
        sock, attempt_exceptions = pending.pop(task)
        try:
            task.result()
        except OSError as exc:
            attempt_exceptions.append(exc)
            exceptions.append(attempt_exceptions)
            sock.close()
            continue
        except BaseException:
            sock.close()
            raise
        return sock
    return None


async def __connect_with_happy_eyeballs(
    loop,
    addrinfos,
    local_addrinfos,
    delay,
):
    import math as __math

    exceptions = []
    pending = {}
    if not __math.isfinite(delay) or delay <= 0:
        delay = 0.0

    for index, addrinfo in enumerate(addrinfos):
        sock, address, attempt_exceptions = __prepare_stream_socket(
            addrinfo, local_addrinfos
        )
        if sock is None:
            exceptions.append(attempt_exceptions)
        else:
            pending[__asyncio.create_task(loop.sock_connect(sock, address))] = (
                sock,
                attempt_exceptions,
            )

        if not pending:
            continue
        if index + 1 >= len(addrinfos):
            continue

        done, _ = await __asyncio.wait(
            tuple(pending),
            timeout=delay,
            return_when=__asyncio.FIRST_COMPLETED,
        )
        winner = __consume_connection_attempts(done, pending, exceptions)
        if winner is not None:
            return winner, pending, exceptions

    while pending:
        done, _ = await __asyncio.wait(
            tuple(pending),
            return_when=__asyncio.FIRST_COMPLETED,
        )
        winner = __consume_connection_attempts(done, pending, exceptions)
        if winner is not None:
            return winner, pending, exceptions

    return None, pending, exceptions


async def __loop_create_connection(
    self,
    protocol_factory,
    host=None,
    port=None,
    *,
    ssl=None,
    family=0,
    proto=0,
    flags=0,
    sock=None,
    local_addr=None,
    server_hostname=None,
    ssl_handshake_timeout=None,
    ssl_shutdown_timeout=None,
    happy_eyeballs_delay=None,
    interleave=None,
    all_errors=False,
):
    if server_hostname is not None and not ssl:
        raise ValueError("server_hostname is only meaningful with ssl")
    if server_hostname is None and ssl:
        if not host:
            raise ValueError(
                "You must set server_hostname when using ssl without a host"
            )
        server_hostname = host
    if ssl_handshake_timeout is not None and not ssl:
        raise ValueError("ssl_handshake_timeout is only meaningful with ssl")
    if ssl_shutdown_timeout is not None and not ssl:
        raise ValueError("ssl_shutdown_timeout is only meaningful with ssl")
    if happy_eyeballs_delay is not None and interleave is None:
        interleave = 1

    created_sock = None
    if host is not None or port is not None:
        if sock is not None:
            raise ValueError("host/port and sock can not be specified at the same time")

        addrinfos = await self.getaddrinfo(
            host,
            port,
            family=family,
            type=__socket.SOCK_STREAM,
            proto=proto,
            flags=flags,
        )
        if not addrinfos:
            raise OSError("getaddrinfo() returned empty list")

        if local_addr is not None:
            local_addrinfos = await self.getaddrinfo(
                *local_addr,
                family=family,
                type=__socket.SOCK_STREAM,
                proto=proto,
                flags=flags,
            )
            if not local_addrinfos:
                raise OSError("getaddrinfo() returned empty list")
        else:
            local_addrinfos = None

        if interleave:
            addrinfos = __interleave_addrinfos(addrinfos, interleave)

        if happy_eyeballs_delay is None:
            connection_exceptions = []
            for addrinfo in addrinfos:
                created_sock, address, attempt_exceptions = __prepare_stream_socket(
                    addrinfo, local_addrinfos
                )
                if created_sock is None:
                    connection_exceptions.append(attempt_exceptions)
                    continue
                try:
                    await self.sock_connect(created_sock, address)
                    break
                except OSError as exc:
                    attempt_exceptions.append(exc)
                    connection_exceptions.append(attempt_exceptions)
                    created_sock.close()
                    created_sock = None
                except BaseException:
                    created_sock.close()
                    raise
            if created_sock is None:
                __raise_connection_error(
                    __flatten_connection_exceptions(connection_exceptions),
                    all_errors=all_errors,
                )
        else:
            (
                created_sock,
                pending,
                connection_exceptions,
            ) = await __connect_with_happy_eyeballs(
                self,
                addrinfos,
                local_addrinfos,
                happy_eyeballs_delay,
            )
            for task, (pending_sock, _) in pending.items():
                task.cancel()
                pending_sock.close()
            if pending:
                await __asyncio.gather(*pending, return_exceptions=True)
            if created_sock is None:
                __raise_connection_error(
                    __flatten_connection_exceptions(connection_exceptions),
                    all_errors=all_errors,
                )

        sock = created_sock
    elif sock is None:
        raise ValueError("host and port was not specified and no sock specified")

    try:
        return await __ORIG_CREATE_CONNECTION(
            self,
            protocol_factory,
            ssl=ssl,
            family=family,
            proto=proto,
            flags=flags,
            sock=sock,
            server_hostname=server_hostname,
            ssl_handshake_timeout=ssl_handshake_timeout,
            ssl_shutdown_timeout=ssl_shutdown_timeout,
        )
    except BaseException:
        if created_sock is not None:
            created_sock.close()
        raise


if Loop.create_connection is __ORIG_CREATE_CONNECTION:
    Loop.create_connection = __loop_create_connection

if __ORIG_CREATE_DATAGRAM_ENDPOINT is None:
    Loop.create_datagram_endpoint = __loop_create_datagram_endpoint

if __ORIG_SENDFILE is None:
    Loop.sendfile = __loop_sendfile

if __ORIG_SOCK_RECVFROM is None:
    Loop.sock_recvfrom = __loop_sock_recvfrom

if __ORIG_SOCK_RECVFROM_INTO is None:
    Loop.sock_recvfrom_into = __loop_sock_recvfrom_into

if __ORIG_SOCK_SENDTO is None:
    Loop.sock_sendto = __loop_sock_sendto

if __ORIG_SOCK_SENDFILE is None:
    Loop.sock_sendfile = __loop_sock_sendfile

# Keep the Rust implementation on the hot path. It already handles task
# factories and keyword forwarding, while the Python wrapper adds measurable
# overhead in task-heavy workloads.


def __install_ssl_tracking() -> None:
    context_cls = __ssl.SSLContext
    if getattr(context_cls, "_rsloop_tracking_installed", False):
        return

    def mark_default_verify_paths(context):
        context.__dict__["_rsloop_use_default_verify_paths"] = True
        return context

    orig_create_default_context = __ssl.create_default_context
    orig_load_cert_chain = context_cls.load_cert_chain
    orig_load_default_certs = context_cls.load_default_certs
    orig_set_default_verify_paths = context_cls.set_default_verify_paths

    def create_default_context(*args, **kwargs):
        return mark_default_verify_paths(orig_create_default_context(*args, **kwargs))

    def load_cert_chain(self, certfile, keyfile=None, password=None):
        result = orig_load_cert_chain(
            self,
            certfile,
            keyfile=keyfile,
            password=password,
        )
        if callable(password):
            password_value = password()
        else:
            password_value = password
        if isinstance(password_value, str):
            password_value = password_value.encode()
        if password_value is not None and not isinstance(password_value, bytes):
            password_value = bytes(password_value)
        self.__dict__["_rsloop_certfile"] = __os.fspath(certfile)
        self.__dict__["_rsloop_keyfile"] = (
            __os.fspath(keyfile) if keyfile is not None else __os.fspath(certfile)
        )
        self.__dict__["_rsloop_key_password"] = password_value
        return result

    def load_default_certs(self, *args, **kwargs):
        result = orig_load_default_certs(self, *args, **kwargs)
        mark_default_verify_paths(self)
        return result

    def set_default_verify_paths(self):
        result = orig_set_default_verify_paths(self)
        mark_default_verify_paths(self)
        return result

    __ssl.create_default_context = create_default_context
    context_cls.load_cert_chain = load_cert_chain
    context_cls.load_default_certs = load_default_certs
    context_cls.set_default_verify_paths = set_default_verify_paths
    context_cls._rsloop_tracking_installed = True


__install_ssl_tracking()


if __typing.TYPE_CHECKING:

    def run(
        main: __typing.Coroutine[__typing.Any, __typing.Any, _T],
        *,
        loop_factory: __typing.Callable[[], Loop] = new_event_loop,
        debug: bool | None = None,
    ) -> _T: ...
else:

    def run(main, *, loop_factory=new_event_loop, debug=None, **run_kwargs):
        async def wrapper():
            loop = __asyncio._get_running_loop()
            if not isinstance(loop, Loop):
                raise TypeError("rsloop.run() uses a non-rsloop loop")
            return await main

        if __sys.version_info[:2] >= (3, 12):
            return __asyncio.run(
                wrapper(),
                loop_factory=loop_factory,
                debug=debug,
                **run_kwargs,
            )

        if __asyncio._get_running_loop() is not None:
            raise RuntimeError(
                "asyncio.run() cannot be called from a running event loop"
            )

        if not __asyncio.iscoroutine(main):
            raise ValueError(f"a coroutine was expected, got {main!r}")

        loop = loop_factory()
        try:
            __asyncio.set_event_loop(loop)
            if debug is not None:
                loop.set_debug(debug)
            return loop.run_until_complete(wrapper())
        finally:
            try:
                __cancel_all_tasks(loop)
                loop.run_until_complete(loop.shutdown_asyncgens())
                shutdown_default_executor = getattr(
                    loop, "shutdown_default_executor", None
                )
                if shutdown_default_executor is not None:
                    loop.run_until_complete(shutdown_default_executor())
            finally:
                __asyncio.set_event_loop(None)
                loop.close()
