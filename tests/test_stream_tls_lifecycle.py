"""Upgrade cancellation must not leak an eventual handshake result."""

import asyncio

import pytest
from rsloop._stream_tls import start_tls


@pytest.mark.parametrize("outcome", ["transport", "error", "none"])
def test_cancelled_upgrade_disposes_late_result(outcome):
    async def main():
        events = []
        upgrade = asyncio.get_running_loop().create_future()

        class Transport:
            def abort(self):
                events.append("abort")

        class Writer:
            transport = Transport()

            async def drain(self):
                events.append("drain")

            def _replace_transport(self, transport):
                pytest.fail("cancelled upgrade must not replace writer transport")

        class Loop:
            def start_tls(self, transport, protocol, sslcontext, **kwargs):
                assert events == ["drain"]
                assert kwargs == {
                    "server_side": True,
                    "server_hostname": None,
                    "ssl_handshake_timeout": 1,
                    "ssl_shutdown_timeout": 2,
                }
                events.append("upgrade")
                return upgrade

        class Protocol:
            def connection_lost(self, exc):
                assert exc is None

        task = asyncio.create_task(
            start_tls(Writer(), Loop(), Protocol(), True, object(), None, 1, 2)
        )
        await asyncio.sleep(0)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert events == ["drain", "upgrade", "abort"]
        assert not upgrade.cancelled()
        if outcome == "transport":
            upgrade.set_result(Transport())
        elif outcome == "error":
            upgrade.set_exception(ConnectionError("handshake failed"))
        else:
            upgrade.set_result(None)
        await asyncio.sleep(0)
        assert events.count("abort") == (2 if outcome == "transport" else 1)

    asyncio.run(main())
