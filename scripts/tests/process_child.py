"""Deterministic child program for real process/pipe integration checks."""

import os
import subprocess
import sys
import threading
import time


def main():
    mode, *arguments = sys.argv[1:]
    if mode == "echo":
        sys.stdout.buffer.write(sys.stdin.buffer.read())
    elif mode == "args":
        sys.stdout.buffer.write("\n".join(arguments).encode("utf-8"))
    elif mode == "env":
        sys.stdout.write(os.environ.get("FAI_PROCESS_VALUE", "<absent>"))
    elif mode == "cwd":
        sys.stdout.write(os.getcwd())
    elif mode == "exit":
        sys.exit(int(arguments[0]))
    elif mode == "flood":
        stderr = threading.Thread(target=lambda: sys.stderr.buffer.write(b"e" * 131072))
        stderr.start()
        sys.stdout.buffer.write(b"o" * 131072)
        sys.stdout.buffer.flush()
        data = sys.stdin.buffer.read()
        stderr.join()
        if data != b"i" * 131072:
            sys.exit(42)
    elif mode == "hold":
        sys.stdout.write("ready\n")
        sys.stdout.flush()
        time.sleep(120)
    elif mode == "descendant":
        child = subprocess.Popen([sys.executable, __file__, "hold"])
        sys.stdout.write(str(child.pid) + "\n")
        sys.stdout.flush()
        # The supervisor must retain group identity after this leader exits.
    elif mode == "upper":
        sys.stdout.buffer.write(sys.stdin.buffer.read().upper())
    elif mode == "count":
        sys.stdout.write(str(len(sys.stdin.buffer.read())))
    else:
        raise ValueError(mode)


if __name__ == "__main__":
    main()
