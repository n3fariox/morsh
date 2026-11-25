#!/usr/bin/env python3
"""Capture stock/stock traffic on Unix; requires PyCryptodome for independent OCB decoding."""
import base64
from Crypto.Cipher import AES
import zlib
import json
import hashlib
import shutil
import fcntl
import struct
import termios
import os
from pathlib import Path
import pty
import select
import re
import signal
import socket
import subprocess
import time

out = Path(__file__).resolve().parents[1] / "tests/fixtures/stock-mosh"
out.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, LC_ALL="C.UTF-8", TERM="xterm-256color")
env.pop("SSH_CONNECTION", None)
# Stock daemon exits after the command; never kill unrelated mosh processes.
server = subprocess.run(
    ["mosh-server", "new", "-c", "256", "-i", "127.0.0.1", "--",
     "/bin/sh", "-c", "printf 'STOCK_FIXTURE_READY\\r\\n'; read line; printf 'REPLY:%s\\r\\n' \"$line\"; sleep 1"],
    env=env, capture_output=True, text=True, timeout=10)
daemon = re.search(r"pid = (\d+)", server.stderr)
connect = next(line for line in server.stdout.splitlines() if line.startswith("MOSH CONNECT "))
_, _, port, key = connect.split()
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind(("127.0.0.1", 0))
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
client = subprocess.Popen(["mosh-client", "127.0.0.1", str(sock.getsockname()[1])],
                          env=dict(env, MOSH_KEY=key), stdin=slave, stdout=slave,
                          stderr=slave, start_new_session=True)
os.close(slave)
remote = ("127.0.0.1", int(port))
peer = None
packets = []
start = time.monotonic()
sent_input = False
try:
    while time.monotonic() - start < 4:
        ready, _, _ = select.select([sock, master], [], [], 0.05)
        if master in ready:
            try:
                os.read(master, 65536)
            except OSError:
                break
        if sock in ready:
            wire, source = sock.recvfrom(65536)
            direction = "server" if source == remote else "client"
            if direction == "client":
                peer = source
            packets.append({"direction": direction, "wire": wire.hex()})
            if direction == "client" or peer:
                sock.sendto(wire, remote if direction == "client" else peer)
        if not sent_input and time.monotonic() - start > 0.5:
            os.write(master, b"fixture-input\n")
            sent_input = True
finally:
    try:
        os.killpg(client.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    if daemon:
        try:
            os.kill(int(daemon[1]), signal.SIGTERM)
        except ProcessLookupError:
            pass
    client.wait(timeout=5)
    os.close(master)
    sock.close()
version = subprocess.run(["mosh-client", "--version"], capture_output=True, text=True)
metadata = {"binary_sha256": {name: hashlib.sha256(Path(shutil.which(name)).read_bytes()).hexdigest()
                               for name in ("mosh-client", "mosh-server")},
            "version": (version.stdout + version.stderr).splitlines()[0],
            "key_hex": base64.b64decode(key + "==").hex(), "packets": packets}
assert packets, "stock client produced no datagrams"
for packet in packets:
    wire = bytes.fromhex(packet["wire"])
    cipher = AES.new(bytes.fromhex(metadata["key_hex"]), AES.MODE_OCB,
                     nonce=b"\0" * 4 + wire[:8])
    plain = cipher.decrypt_and_verify(wire[8:-16], wire[-16:])
    # This short exchange uses single fragments. Fail rather than save partial protos.
    assert plain[12:14] == b"\x80\x00", "capture needs multi-fragment decoding"
    packet["plaintext"] = plain.hex()
    packet["protobuf"] = zlib.decompress(plain[14:]).hex()
(out / "capture.json").write_text(json.dumps(metadata, indent=2) + "\n")
print(f"Captured {len(packets)} datagrams to {out / 'capture.json'}")
