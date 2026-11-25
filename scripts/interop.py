#!/usr/bin/env python3
"""Bounded stock interoperability tests using owned PTYs and server processes."""
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import termios
import time

BIN = Path(sys.argv[1]).resolve()
ENV = dict(os.environ, LC_ALL="C.UTF-8", TERM="xterm-256color")
ENV.pop("SSH_CONNECTION", None)
COMMAND = "printf 'MORSH_READY\\r\\n'; read line; printf 'MORSH_RESPONSE_%s\\r\\n' \"$line\"; sleep 15"


def pairing(client, server, launch_shape=False, ip="127.0.0.1"):
    stock = server == "mosh-server"
    args = [server, "new", "-c", "256"]
    # OS chooses a free port for the launch-line test; no global fixed port.
    port = None
    if launch_shape:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        args += ["-s", "-p", str(port), "-l", "LANG=C.UTF-8", "-l", "LC_CTYPE=C.UTF-8"]
    else:
        args += ["-i", ip]
    if not stock:
        args += ["-D"]
    args += ["--", "/bin/sh", "-c", COMMAND]
    # Files avoid pipe EOF waits on descriptors inherited by the stock daemon.
    import tempfile
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(args, env=ENV, stdout=output, stderr=errors, start_new_session=True)
        daemon_pid = None
        child = None
        master = None
        try:
            deadline = time.monotonic() + 10
            connect = None
            while time.monotonic() < deadline:
                output.seek(0)
                connect = re.search(rb"MOSH CONNECT (\d+) (\S+)", output.read())
                if connect:
                    break
                time.sleep(0.05)
            if not connect:
                errors.seek(0)
                raise AssertionError(f"server never announced MOSH CONNECT: {errors.read()[-2000:]!r}")
            actual_port, key = (x.decode() for x in connect.groups())
            if port is not None:
                assert int(actual_port) == port, "server ignored -p"
            errors.seek(0)
            pid_match = re.search(rb"pid = (\d+)", errors.read())
            if pid_match:
                daemon_pid = int(pid_match[1])
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            host = f"[{ip}]" if ":" in ip else ip
            argv = ([client, ip, actual_port] if client == "mosh-client"
                    else [client, f"{host}:{actual_port}"])
            child = subprocess.Popen(argv, env=dict(ENV, MOSH_KEY=key, MORSH_KEY=key),
                                     stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
            transcript = b""
            sent = False
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    try:
                        transcript += os.read(master, 65536)
                    except OSError:
                        break
                if b"MORSH_READY" in transcript and not sent:
                    os.write(master, b"input-ok\n")
                    sent = True
                if b"MORSH_RESPONSE_input-ok" in transcript:
                    return
            raise AssertionError(f"input/response never rendered; tail={transcript[-1000:]!r}")
        finally:
            if stock and daemon_pid is None:
                errors.seek(0)
                pid_match = re.search(rb"pid = (\d+)", errors.read())
                if pid_match:
                    daemon_pid = int(pid_match[1])
            for proc in (child, process):
                if proc is not None:
                    try:
                        os.killpg(proc.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
            if daemon_pid:
                try:
                    os.kill(daemon_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if master is not None:
                os.close(master)


def wrapper_smoke():
    label = "SKIP wrapper over ssh: no passwordless ssh to localhost"
    if not shutil.which("ssh"):
        print(label, flush=True)
        return
    try:
        probe = subprocess.run(
            ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=5", "localhost", "true"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=8,
        )
    except subprocess.TimeoutExpired:
        print(label, flush=True)
        return
    if probe.returncode:
        print(label, flush=True)
        return
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    child = subprocess.Popen(
        [str(BIN / "morsh"), "--ssh-mode", "system", "--server", str(BIN / "morsh-server"),
         "localhost", "/bin/sh", "-c", "echo MORSH_WRAPPER_OK; sleep 3"],
        env=dict(ENV, PATH=str(BIN) + os.pathsep + ENV["PATH"]),
        stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
    )
    os.close(slave)
    transcript = b""
    try:
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    transcript += os.read(master, 65536)
                except OSError:
                    break
            if b"MORSH_WRAPPER_OK" in transcript:
                print("PASS wrapper over ssh", flush=True)
                return
        raise AssertionError(f"wrapper never rendered output; tail={transcript[-1000:]!r}")
    finally:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
        os.close(master)


def have_ipv6_loopback():
    try:
        with socket.socket(socket.AF_INET6, socket.SOCK_DGRAM) as sock:
            sock.bind(("::1", 0))
        return True
    except OSError:
        return False


failures = 0
MORSH_CLIENT, MORSH_SERVER = str(BIN / "morsh-client"), str(BIN / "morsh-server")
for client, server, shape, ip in [
    (MORSH_CLIENT, MORSH_SERVER, False, "127.0.0.1"),
    (MORSH_CLIENT, "mosh-server", False, "127.0.0.1"),
    ("mosh-client", MORSH_SERVER, False, "127.0.0.1"),
    (MORSH_CLIENT, MORSH_SERVER, True, "127.0.0.1"),
    (MORSH_CLIENT, "mosh-server", True, "127.0.0.1"),
    (MORSH_CLIENT, MORSH_SERVER, False, "::1"),
    (MORSH_CLIENT, "mosh-server", False, "::1"),
    ("mosh-client", MORSH_SERVER, False, "::1"),
]:
    label = f"{Path(client).name} -> {Path(server).name}" + (" (mosh launch argv)" if shape else "")
    if ip != "127.0.0.1":
        label += f" over {ip}"
    missing = [name for name in (client, server) if not shutil.which(name)]
    if missing:
        print(f"SKIP {label}: missing {', '.join(missing)}", flush=True)
        continue
    if ":" in ip and not have_ipv6_loopback():
        print(f"SKIP {label}: no IPv6 loopback", flush=True)
        continue
    try:
        pairing(client, server, shape, ip)
        print(f"PASS {label}", flush=True)
    except (AssertionError, OSError, subprocess.SubprocessError) as error:
        print(f"FAIL {label}: {error}", flush=True)
        failures += 1
try:
    wrapper_smoke()
except (AssertionError, OSError, subprocess.SubprocessError) as error:
    print(f"FAIL wrapper over ssh: {error}", flush=True)
    failures += 1
sys.exit(bool(failures))
