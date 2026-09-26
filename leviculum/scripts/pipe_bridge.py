#!/usr/bin/env python3
"""Bidirectional stdio<->TCP bridge for PipeInterface interop tests.

Usage:
    pipe_bridge.py listen  <port>   # bind 127.0.0.1:<port>, accept one peer
    pipe_bridge.py connect <port>   # connect to 127.0.0.1:<port> (with retry)

Relays raw bytes between this process's stdin/stdout and a TCP peer, so two
Reticulum nodes -- each running a PipeInterface -- can exchange HDLC frames
through the pipe. The stream is copied verbatim; the bridge never inspects or
reframes the bytes.
"""
import os
import socket
import sys
import threading
import time

CHUNK = 65536


def writeall(fd, data):
    view = memoryview(data)
    while view:
        n = os.write(fd, view)
        view = view[n:]


def pump(read, write):
    try:
        while True:
            data = read()
            if not data:
                break
            write(data)
    except Exception:
        pass


def main():
    if len(sys.argv) != 3:
        sys.stderr.write("usage: pipe_bridge.py listen|connect <port>\n")
        sys.exit(2)

    mode, port = sys.argv[1], int(sys.argv[2])

    if mode == "listen":
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", port))
        srv.listen(1)
        conn, _ = srv.accept()
    elif mode == "connect":
        conn = None
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                conn = socket.create_connection(("127.0.0.1", port))
                break
            except OSError:
                time.sleep(0.05)
        if conn is None:
            sys.stderr.write("pipe_bridge: could not connect\n")
            sys.exit(1)
    else:
        sys.stderr.write("pipe_bridge: unknown mode %r\n" % mode)
        sys.exit(2)

    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    stdin_fd = sys.stdin.fileno()
    stdout_fd = sys.stdout.fileno()

    t_out = threading.Thread(
        target=pump,
        args=(lambda: os.read(stdin_fd, CHUNK), conn.sendall),
        daemon=True,
    )
    t_in = threading.Thread(
        target=pump,
        args=(lambda: conn.recv(CHUNK), lambda d: writeall(stdout_fd, d)),
        daemon=True,
    )
    t_out.start()
    t_in.start()
    # Exit as soon as either direction closes so a dropped peer takes the
    # bridge down (PipeInterface then respawns it).
    while t_out.is_alive() and t_in.is_alive():
        time.sleep(0.05)


if __name__ == "__main__":
    main()
