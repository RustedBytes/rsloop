"""Coroutine orchestration for the native stream writer's TLS upgrade."""

from __future__ import annotations

import asyncio


def _abort_upgrade(future):
    # A cancelled caller can leave a blocking handshake in flight. Its result
    # must still be observed and any eventual TLS transport disposed.
    try:
        transport = future.result()
    except BaseException:  # noqa: BLE001 - retrieve late handshake failure
        return
    if transport is not None:
        transport.abort()


async def start_tls(
    writer,
    loop,
    protocol,
    server_side,
    sslcontext,
    server_hostname,
    ssl_handshake_timeout,
    ssl_shutdown_timeout,
):
    await writer.drain()
    upgrade = asyncio.ensure_future(
        loop.start_tls(
            writer.transport,
            protocol,
            sslcontext,
            server_side=server_side,
            server_hostname=server_hostname,
            ssl_handshake_timeout=ssl_handshake_timeout,
            ssl_shutdown_timeout=ssl_shutdown_timeout,
        )
    )
    try:
        # wait() does not propagate caller cancellation into the handshake.
        # We own late-result disposal; shield() logs late failures on Python
        # versions where those exceptions would otherwise be unobserved.
        await asyncio.wait((upgrade,))
        transport = upgrade.result()
        if transport is None:
            raise ConnectionError("TLS upgrade returned no transport")
        writer._replace_transport(transport)
    except BaseException as exc:
        writer.transport.abort()
        upgrade.add_done_callback(_abort_upgrade)
        # The old transport is detached and cannot deliver connection_lost.
        # Complete reader/drain/close waiters even if a handshake finishes late.
        protocol.connection_lost(
            None if isinstance(exc, asyncio.CancelledError) else exc
        )
        raise
