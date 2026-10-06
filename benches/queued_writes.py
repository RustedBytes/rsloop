"""Measure immutable writes under backpressure, including complete delivery.

Run with the same release build and arguments before and after a change.
The peer starts reading after enqueue timing, forcing writes into the queue.
"""

import argparse
import asyncio
import json
import platform
import socket
import statistics
import time

import rsloop


async def measure(size, count):
    loop = asyncio.get_running_loop()
    sender, receiver = socket.socketpair()
    sender.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
    sender.setblocking(False)
    receiver.setblocking(False)
    transport = None
    payload = b"x" * size
    try:
        transport, _ = await loop.create_connection(asyncio.Protocol, sock=sender)
        started = time.perf_counter()
        for _ in range(count):
            transport.write(payload)
        queued = time.perf_counter()
        transport.write_eof()
        received = 0
        while chunk := await loop.sock_recv(receiver, 256 * 1024):
            assert chunk == b"x" * len(chunk)
            received += len(chunk)
        finished = time.perf_counter()
        assert received == size * count
        return {"enqueue_s": queued - started, "delivery_s": finished - started}
    finally:
        if transport is not None:
            transport.close()
        sender.close()
        receiver.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--size", type=int, default=1024 * 1024)
    parser.add_argument("--count", type=int, default=32)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--repeat", type=int, default=9)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    samples = []
    for index in range(args.warmups + args.repeat):
        result = rsloop.run(asyncio.wait_for(measure(args.size, args.count), 60))
        if index >= args.warmups:
            samples.append(result)
    result = {
        "settings": vars(args),
        "platform": platform.platform(),
        "python": platform.python_version(),
        "samples": samples,
        "medians": {
            key: statistics.median(sample[key] for sample in samples)
            for key in samples[0]
        },
    }
    with open(args.output, "w") as output:
        json.dump(result, output, indent=2)
    print(json.dumps(result["medians"], indent=2))


if __name__ == "__main__":
    main()
