#!/usr/bin/env python3
"""Measure graceful shutdown and reopen the same DB (Linux native or APE).

python3 zebrad/tests/shutdown.py /path/to/zebrad [--gui] [--window-close]
Uses an isolated temporary cache. GUI close needs xdotool and libX11; never
sends input to another window. Run GUI cases on an otherwise idle desktop.
"""
import argparse
import ctypes
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time


def close_window(pid):
    window = subprocess.check_output(
        ["xdotool", "search", "--pid", str(pid), "--name", "^Zcash Crosslink Visualizer$"],
        text=True,
    ).splitlines()
    assert len(window) == 1, f"expected this process's window, found {window}"
    class Message(ctypes.Structure):
        _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong),
                    ("send_event", ctypes.c_int), ("display", ctypes.c_void_p),
                    ("window", ctypes.c_ulong), ("message_type", ctypes.c_ulong),
                    ("format", ctypes.c_int), ("data", ctypes.c_long * 5)]
    class Event(ctypes.Union):
        _fields_ = [("message", Message), ("padding", ctypes.c_long * 24)]
    x = ctypes.CDLL("libX11.so.6")
    x.XOpenDisplay.restype = ctypes.c_void_p
    x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    x.XInternAtom.restype = ctypes.c_ulong
    x.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long, ctypes.POINTER(Event)]
    x.XCloseDisplay.argtypes = [ctypes.c_void_p]
    display = x.XOpenDisplay(None)
    assert display, "no X display"
    try:
        event = Event()
        event.message.type = 33  # ClientMessage
        event.message.window = int(window[0])
        event.message.message_type = x.XInternAtom(display, b"WM_PROTOCOLS", 0)
        event.message.format = 32
        event.message.data[0] = x.XInternAtom(display, b"WM_DELETE_WINDOW", 0)
        assert x.XSendEvent(display, int(window[0]), 0, 0, ctypes.byref(event))
    finally:
        x.XCloseDisplay(display)  # flushes the close request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--gui", action="store_true")
    parser.add_argument("--window-close", action="store_true")
    parser.add_argument("--sync", action="store_true", help="require real peer blocks before stopping and their state on reopen")
    parser.add_argument("--during-verification", action="store_true", help="stop during cold shielded-key initialization at block 6")
    parser.add_argument("--term", action="store_true", help="send SIGTERM instead of SIGINT")
    args = parser.parse_args()
    args.sync = args.sync or args.during_verification
    assert not args.window_close or args.gui, "window-close requires --gui"
    binary = args.binary.resolve()
    command = (["/bin/sh", str(binary)] if binary.suffix == ".com" else [str(binary)])
    with tempfile.TemporaryDirectory(prefix="zebrad-shutdown-") as directory:
        root = Path(directory)
        config = root / "zebrad.toml"
        subprocess.run(command + ["generate", "-o", str(config)], cwd=root, check=True)
        text = config.read_text()
        text = re.sub(r'^cache_dir = ".*"$', f'cache_dir = "{root / "state"}"', text, flags=re.M)
        text = text.replace('cache_dir = true', 'cache_dir = false')
        text = text.replace('listen_addr = "[::]:8233"', 'listen_addr = "127.0.0.1:38233"')
        text = text.replace('listen_addr = "127.0.0.1:8232"', 'listen_addr = "127.0.0.1:38232"')
        text = re.sub(r'bft_peers = \[[^\]]*\]', 'bft_peers = []', text)
        config.write_text(text)
        persisted_tip = 0
        for boot in [1, 2]:  # second boot verifies released locks and persisted state
            log = root / f"boot-{boot}.log"
            with log.open("w") as output:
                child = subprocess.Popen(command + ["-c", str(config), "--filters",
                                         "info,zebra_state::service::finalized_state::disk_db=debug"] + ([] if args.gui else ["--headless"]),
                                         cwd=root, stdout=output, stderr=output, start_new_session=True)
                try:
                    deadline = time.monotonic() + 45
                    while "NewNet: Starting at height=" not in log.read_text():
                        assert child.poll() is None, log.read_text()[-6000:]
                        assert time.monotonic() < deadline, "startup timed out: " + log.read_text()[-6000:]
                        time.sleep(0.02)
                    if args.sync and boot == 1:
                        deadline = time.monotonic() + 60
                        pattern = r"Committing: @ 6," if args.during_verification else r"committed!: @ [1-9]\d*,"
                        while not re.search(pattern, log.read_text()):
                            assert child.poll() is None, log.read_text()[-6000:]
                            assert time.monotonic() < deadline, "no peer blocks: " + log.read_text()[-6000:]
                            time.sleep(0.02)
                    time.sleep(0.05 if args.during_verification and boot == 1 else 2)
                    start = time.monotonic()
                    if args.window_close:
                        close_window(child.pid)
                    else:
                        os.killpg(child.pid, signal.SIGTERM if args.term else signal.SIGINT)
                    code = child.wait(timeout=120)
                    elapsed = time.monotonic() - start
                    result = log.read_text()
                    assert code == 0, result[-6000:]
                    assert "panicked" not in result, result[-6000:]
                    assert "writer stopped after completing in-flight commits" in result, result[-6000:]
                    assert "non-finalized backup saved latest committed state" in result, result[-6000:]
                    # Release builds compile out the DEBUG flush message. Completion
                    # is checked through backup drain, no flush errors, and exact-tip reopen.
                    assert "unexpected error flushing database" not in result, result[-6000:]
                    assert "stopping zebrad" in result, result[-6000:]
                    if args.gui:
                        assert "running headless" not in result, result[-6000:]
                    tip = re.findall(r"NewNet: Starting at height=(\d+)", result)
                    if args.sync and boot == 2:
                        assert int(tip[-1]) >= persisted_tip > 0, "latest commits were not preserved on shutdown"
                    committed = re.findall(r"committed!: @ (\d+),", result)
                    if boot == 1 and committed:
                        persisted_tip = max(map(int, committed))
                    print(f"boot {boot}: reopened tip={tip[-1]}, last commit={committed[-1] if committed else 'none'}, shutdown={elapsed:.3f}s, exit={code}, no panics", flush=True)
                    assert elapsed < 2, f"shutdown took {elapsed:.3f}s"
                except BaseException:
                    print(log.read_text()[-18000:], flush=True)
                    raise
                finally:
                    if child.poll() is None:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait()


if __name__ == "__main__":
    main()
