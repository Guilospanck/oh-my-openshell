"""Drives the real `osh` dashboard in a pseudo-terminal the way a person would. The screen is a
terminal emulator (pyte), so assertions read what is on screen after redraws, not a byte stream.

    with Osh() as o:
        o.wait("Sandboxes")
        o.press("2")
        o.wait("Providers")
"""
import fcntl, os, pty, select, signal, struct, termios, time
import pyte

KEYS = {
    "enter": "\r", "esc": "\x1b", "tab": "\t", "backtab": "\x1b[Z", "up": "\x1b[A", "down": "\x1b[B",
    "right": "\x1b[C", "left": "\x1b[D", "space": " ", "backspace": "\x7f",
    "ctrl-]": "\x1d", "ctrl-5": "\x1d", "ctrl-c": "\x03",
}


class Timeout(Exception):
    pass


class Osh:
    def __init__(self, args=("osh",), cols=140, rows=40, env=None):
        self.cols, self.rows = cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            e = dict(os.environ, TERM="xterm-256color")
            e.update(env or {})
            os.execvpe(args[0], list(args), e)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.status = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.kill()

    def pump(self, secs=0.2):
        end = time.time() + secs
        while True:
            left = max(0.0, end - time.time())
            r, _, _ = select.select([self.fd], [], [], left)
            if not r:
                return True
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            self.stream.feed(data)
            # A real terminal answers cursor-position requests (osh asks after a job returns).
            for _ in range(data.count(b"\x1b[6n")):
                os.write(self.fd, b"\x1b[%d;%dR" % (self.screen.cursor.y + 1, self.screen.cursor.x + 1))
            if time.time() >= end:
                return True

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)

    def wait(self, needle, timeout=30):
        """Block until `needle` (a string, or any of a tuple) is on screen; return the screen."""
        needles = needle if isinstance(needle, tuple) else (needle,)
        end = time.time() + timeout
        while time.time() < end:
            alive = self.pump(0.2)
            t = self.text()
            if any(n in t for n in needles):
                return t
            if not alive:
                break
        raise Timeout(f"never showed {needle!r}; screen:\n{self.text()}")

    def wait_gone(self, needle, timeout=30):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.2)
            if needle not in self.text():
                return
        raise Timeout(f"{needle!r} never went away; screen:\n{self.text()}")

    def clear(self):
        """Forget the emulated screen. A job runs on the normal screen and osh then redraws the
        dashboard; pyte has no alternate screen, so the job's text would linger below it."""
        self.screen.reset()

    def settle(self, secs=0.4):
        self.pump(secs)

    def press(self, *keys):
        for k in keys:
            self.send(KEYS.get(k, k))
            self.settle(0.25)

    def send(self, s):
        os.write(self.fd, s.encode() if isinstance(s, str) else s)

    def type(self, s):
        for ch in s:
            self.send(ch)
            self.pump(0.02)
        self.settle(0.2)

    def exit_status(self, timeout=15):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.1)
            pid, st = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.WEXITSTATUS(st) if os.WIFEXITED(st) else -1
                return self.status
        raise Timeout("osh did not exit")

    def kill(self):
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except OSError:
                pass
            self.status = -9
        try:
            os.close(self.fd)
        except OSError:
            pass
