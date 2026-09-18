#!/usr/bin/env python3
"""A RESP server that answers PING and swallows everything else.

Used by CI to prove that `connection.request_timeout` is enforced: the
precheck PING succeeds, then every GET/SET sits unanswered until the client
gives up on it. Nothing here parses RESP properly; it only needs to spot the
PING command in the byte stream.

Usage: blackhole-resp.py [port]   (default 6390, binds 127.0.0.1)
"""
import asyncio
import sys


async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            pongs = data.upper().count(b"\r\nPING\r\n")
            if pongs:
                writer.write(b"+PONG\r\n" * pongs)
                await writer.drain()
    except (ConnectionResetError, BrokenPipeError):
        pass
    finally:
        writer.close()


async def main(port: int) -> None:
    server = await asyncio.start_server(handle, "127.0.0.1", port)
    print(f"blackhole-resp listening on 127.0.0.1:{port}", flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1]) if len(sys.argv) > 1 else 6390))
