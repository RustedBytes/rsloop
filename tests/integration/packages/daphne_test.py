from __future__ import annotations

import asyncio
import sys
from pathlib import Path
from typing import Any

import rsloop
from _smoke import reserve_port, wait_for_http


async def application(scope: dict[str, Any], receive: Any, send: Any) -> None:
    assert scope["type"] == "http"
    loop_name = f"{type(asyncio.get_running_loop()).__module__}"
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain")],
        }
    )
    await send(
        {
            "type": "http.response.body",
            "body": f"daphne-{loop_name}".encode(),
        }
    )


def serve(port: int) -> None:
    rsloop.install()
    # Daphne installs Twisted's asyncio reactor while importing its CLI. Install
    # rsloop first so that reactor captures an rsloop loop instead of creating a
    # stdlib selector loop before the policy is active.
    from daphne.cli import CommandLineInterface

    CommandLineInterface().run(
        [
            "--bind",
            "127.0.0.1",
            "--port",
            str(port),
            "--verbosity",
            "0",
            "daphne_test:application",
        ]
    )


async def main() -> None:
    port = reserve_port()
    process = await asyncio.create_subprocess_exec(
        sys.executable,
        str(Path(__file__).resolve()),
        "--serve",
        str(port),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        response = await wait_for_http(port)
        assert b"daphne-rsloop" in response, response
        print("daphne ok")
    finally:
        if process.returncode is None:
            process.terminate()
        try:
            await asyncio.wait_for(process.wait(), 5)
        except TimeoutError:
            process.kill()
            await process.wait()
        if process.returncode not in (0, -15):
            stdout, stderr = await process.communicate()
            raise RuntimeError(
                f"Daphne exited with {process.returncode}: "
                f"{stdout.decode()}\n{stderr.decode()}"
            )


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--serve":
        serve(int(sys.argv[2]))
    else:
        rsloop.run(main())
