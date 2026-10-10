#!/usr/bin/env python3
"""Exercise native TUI input, resize, submission and restoration on a real Unix PTY."""

import argparse
import fcntl
import os
from pathlib import Path
import pty
import re
import resource
import select
import signal
import struct
import subprocess
import termios
import time


ROOT = Path(__file__).resolve().parents[2]
CSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


def child_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def exercise(command, title, keys, finish=None, expected=None, timeout=30):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    original = termios.tcgetattr(slave)
    process = subprocess.Popen(command, cwd=ROOT, stdin=slave, stdout=slave,
                               stderr=slave, preexec_fn=child_terminal)
    output = bytearray()
    entered = None
    sent_finish = False
    deadline = time.monotonic() + timeout
    try:
        while process.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                output.extend(os.read(master, 65536))
            if len(output) > 8 * 1024 * 1024:
                raise AssertionError("TUI produced unbounded output")
            plain = CSI.sub(b"", output)
            if entered is None and title in plain:
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 96, 0, 0))
                os.killpg(process.pid, signal.SIGWINCH)
                os.write(master, keys)
                entered = time.monotonic()
            if entered is not None and finish and not sent_finish and time.monotonic() - entered >= 1:
                os.write(master, finish)
                sent_finish = True
        if process.poll() is None:
            raise AssertionError("TUI did not exit before the test deadline")
        while select.select([master], [], [], 0)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            output.extend(chunk)
        assert process.returncode == 0, (process.returncode, output[-2000:])
        assert entered is not None, output[-2000:]
        assert termios.tcgetattr(slave) == original, "terminal modes were not restored"
        assert b"\x1b[?1049h" in output and b"\x1b[?1049l" in output
        assert b"\x1b[?25h" in output, "cursor was not restored"
        if expected:
            assert expected in output, output[-2000:]
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    exercise([str(args.directory / "tui-FormDemo.exe")], b"Connection settings", b"\t\t\r",
             expected=b"Connected to localhost:5432")
    exercise([str(args.directory / "tui-AgentDemo.exe")], b"Fai agent console", b"hello\r", b"\x03")
    exercise([str(args.directory / "tui-DataBrowser.exe")], b"SQLite data browser", b"\x1b[F", b"\x1b")
    exercise([args.fai, "run", "--no-daemon", "-C", "packages", "tui/examples/FormDemo.fai"],
             b"Connection settings", b"\t\t\r", expected=b"Connected to localhost:5432")
    print("TUI PTY input, resize, and restoration checks passed")


if __name__ == "__main__":
    main()
