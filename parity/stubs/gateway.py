#!/usr/bin/env python3
"""TCP forwarder that publishes services living on the parity's internal network.

The internal network has no route out (so nothing reaches the real Deezer, GitHub, ...),
and docker cannot publish ports from an internal-only container. This process sits on
both networks and forwards raw TCP, so the host reaches Octo and Navidrome unchanged.

Usage: gateway.py LISTEN_PORT:HOST:PORT [...]
"""
import asyncio
import sys


async def pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    except (ConnectionError, asyncio.CancelledError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


def handler(host: str, port: int):
    async def handle(client_r: asyncio.StreamReader, client_w: asyncio.StreamWriter) -> None:
        try:
            up_r, up_w = await asyncio.open_connection(host, port)
        except OSError:
            client_w.close()
            return
        await asyncio.gather(pipe(client_r, up_w), pipe(up_r, client_w))

    return handle


async def main() -> None:
    servers = []
    for spec in sys.argv[1:]:
        listen, host, port = spec.split(":")
        servers.append(await asyncio.start_server(handler(host, int(port)), "0.0.0.0", int(listen)))
        print(f"gateway: :{listen} -> {host}:{port}", flush=True)
    await asyncio.gather(*(s.serve_forever() for s in servers))


if __name__ == "__main__":
    asyncio.run(main())
