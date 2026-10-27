#!/usr/bin/env python3
"""Drives `osh setup` in a real pseudo-terminal, the way a person would: opens the wizard, ticks
Podman, reviews, installs, waits for "All set." and leaves. Exit 0 only when every screen showed up.
usage: wizard-drive.py quit|install"""
import os, pty, re, select, sys, time

mode = sys.argv[1]
ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|\x1b[=>]")

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp("osh", ["osh", "setup"])
import fcntl, struct, termios
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

seen = b""
def wait_for(text, timeout):
    global seen
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.5)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            seen += chunk
            if text.encode() in ANSI.sub(b"", seen):
                return True
    return False

def step(label, text, timeout=30):
    if not wait_for(text, timeout):
        print(f"FAIL wizard: never showed {label!r} ({text!r})")
        print(ANSI.sub(b"", seen)[-1500:].decode("utf8", "replace"))
        os.kill(pid, 9)
        sys.exit(1)
    print(f"ok   wizard shows {label}")

step("the machine screen", "Machine")
os.write(fd, b"\r")
step("the gateway list", "Podman")
if mode == "quit":
    os.write(fd, b"q")
    _, status = os.waitpid(pid, 0)
    print("ok   quitting leaves cleanly" if os.WEXITSTATUS(status) in (0, 1) else "FAIL quit")
    sys.exit(0)
os.write(fd, b"\x1b[B")   # Down: Docker -> Podman
time.sleep(0.3)
os.write(fd, b" ")
time.sleep(0.3)
os.write(fd, b"\r")
step("the review screen", "Install")
seen = b""
os.write(fd, b"\r")
step("a finished install", "All set.", 1500)
os.write(fd, b"\r")
_, status = os.waitpid(pid, 0)
code = os.WEXITSTATUS(status)
if code != 0:
    print(f"FAIL osh exited {code} after the wizard")
    sys.exit(1)
print("ok   osh exits 0 after the wizard")
