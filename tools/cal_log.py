#!/usr/bin/env python3
"""Bench calibration logger: HC-42 over BLE on one side, SDM3055X-E on the other.

Connects to the meter exactly the way the Android reader does -- Nordic-style
transparent serial, service 0xFFE0 / characteristic 0xFFE1, subscribe and
listen, no command written -- so whatever the firmware pushes out UART2 lands
here as lines. Every line that parses as a frame triggers one reading of the
bench multimeter, and the pair is appended to a CSV.

The reference reading is taken *when the frame arrives*, not on a timer: the
meter reports one measurement per press, and the current that matters is the
one flowing while that measurement was taken.

    pip install bleak                     # BLE
    pip install pyvisa pyvisa-py pyusb    # the DMM over USB
    brew install libusb                   # what pyusb talks to on macOS

    ./tools/cal_log.py --list        # 有哪些蓝牙设备
    ./tools/cal_log.py --list-dmm    # 有哪些仪器
    ./tools/cal_log.py --out cal-x32.csv

Keys while running:

    space   暂停 / 继续（暂停时蓝牙帧照收但不记，万用表也不读）
    q       退出
"""

import argparse
import asyncio
import contextlib
import csv
import os
import re
import sys
import termios
import time
import tty
from datetime import datetime

try:
    from bleak import BleakClient, BleakScanner
except ImportError:
    # Deferred rather than fatal at import, so `--help` still answers on a
    # machine that has not been set up yet.
    BleakClient = BleakScanner = None


def require_bleak():
    if BleakScanner is None:
        sys.exit("需要 bleak: pip install bleak")

# Same transparent-serial UUIDs the Android reader uses
# (MeterPollingService.java). The HC-42 exposes 0xFFE1 as notify+write; only
# notify is used here, as on the phone.
SERVICE_UUID = "0000ffe0-0000-1000-8000-00805f9b34fb"
CHAR_UUID = "0000ffe1-0000-1000-8000-00805f9b34fb"

# Advertised-name substrings that mark a candidate, matching
# MeterPollingService.isMeterCandidate so the two agree on what a meter is.
NAME_HINTS = ("HC-42", "HC42", "METER", "AMMETER", "BYJX_")

# What the multimeter calls itself in `*IDN?`. The bench has more than one
# Siglent instrument on USB and their resource strings differ only by product
# ID and serial, so picking by position would eventually send `CONF:CURR:AC`
# to the oscilloscope. Asking each candidate who it is costs one exchange and
# cannot be got wrong by reordering a USB hub.
DMM_IDN_HINT = "SDM"

# One row per logged frame. `raw` is last because it is the wide one, and it is
# always written even when nothing parsed: a frame this script did not
# understand is still evidence, and losing it would mean repeating the bench
# run to get it back.
CSV_FIELDS = [
    "time",
    "dmm_a",
    "lag_ms",
    "rms_lsb",
    "gain",
    "current_ma",
    "status",
    "addr",
    "mean",
    "pp",
    "extra",
    "raw",
]

# `TAG,KEY=VALUE,KEY=VALUE...`, which is the shape of the existing
# METER_TEST frame and of anything else the firmware may start sending. Keys
# are taken as they come rather than fixed here, so a frame that grows a field
# still logs it (into `extra`) instead of being dropped.
FRAME_RE = re.compile(r"^[A-Z][A-Z0-9_]*(,[A-Za-z_][A-Za-z0-9_]*=[^,]*)+$")

# Frame key -> CSV column. Calibration needs the raw RMS and the gain it was
# taken at; the already-scaled CURRENT_MA is the output of the table being
# fitted, so it is logged as context, not as the datum.
KEY_TO_FIELD = {
    "RMS": "rms_lsb",
    "RMS_LSB": "rms_lsb",
    "GAIN": "gain",
    "CURRENT_MA": "current_ma",
    "STATUS": "status",
    "ADDR": "addr",
    "MEAN": "mean",
    "MEAN_IN": "mean",
    "PP": "pp",
    "PP_IN": "pp",
}


def parse_frame(line):
    """Frame line -> dict of CSV columns, or None if the line is not a frame.

    Unknown keys go to `extra` verbatim rather than being discarded, so a
    firmware that adds a field does not silently drop it here.
    """
    if not FRAME_RE.match(line):
        return None
    tag, *pairs = line.split(",")
    row = {}
    extra = [tag]
    for pair in pairs:
        key, _, value = pair.partition("=")
        field = KEY_TO_FIELD.get(key.upper())
        if field:
            row[field] = value
        else:
            extra.append(pair)
    row["extra"] = ",".join(extra)
    return row


def split_lines(pending):
    """Drain whole lines out of `pending`, leaving any partial tail behind.

    BLE hands over MTU-sized chunks with no relation to line boundaries, so a
    frame routinely arrives split across two notifications and two frames
    routinely arrive in one. Only the bytes up to the last newline are
    consumed; the remainder stays buffered for the next chunk.
    """
    out = []
    while True:
        nl = pending.find(b"\n")
        if nl < 0:
            return out
        line = bytes(pending[:nl]).decode("ascii", errors="replace")
        del pending[: nl + 1]
        line = line.replace("\r", "").strip()
        if line:
            out.append(line)


async def drain_burst(lines, window):
    """Collect the lines that belong with one already-taken line.

    One measurement goes out as more than one frame -- the reader's
    `METER_TEST` and the bench's `METER_CAL` -- and at 9600 baud those take
    over a hundred milliseconds to clock out, so they arrive as a burst rather
    than together. Logging them as separate rows would mean half the CSV has
    the RMS and the other half has the amps, and each would carry its own
    reference reading taken at a different instant. They belong in one row.

    Bounded by a fixed window rather than by a frame count, because the number
    of frames per reading is the firmware's business, not this script's.
    """
    out = []
    deadline = time.monotonic() + window
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return out
        try:
            out.append(await asyncio.wait_for(lines.get(), remaining))
        except asyncio.TimeoutError:
            return out


def merge_frame(row, extra):
    """Fold a later frame of the same burst into the row being built.

    Later values win on collision. The frames of one reading disagree only on
    fields they share by coincidence (ADDR), and a disagreement there means the
    burst caught two different meters, which the `extra` column preserves for
    whoever has to work that out.
    """
    for key, value in extra.items():
        if key == "extra":
            row["extra"] = f"{row.get('extra', '')},{value}".strip(",")
        else:
            row[key] = value


class NullDmm:
    """No reference instrument: log the meter alone."""

    label = "none"

    async def open(self):
        pass

    async def read(self):
        return None

    async def close(self):
        pass


class TcpDmm:
    """SCPI over the SDM3055X-E's LAN port (raw socket, the usual 5025).

    Configured once and then read with bare `READ?` rather than a
    `MEAS:CURR:AC?` per sample, so the range and function cannot be
    renegotiated between two points of the same calibration run.
    """

    def __init__(self, host, port, conf_cmd, read_cmd, timeout):
        self.host = host
        self.port = port
        self.conf_cmd = conf_cmd
        self.read_cmd = read_cmd
        self.timeout = timeout
        self.label = f"tcp://{host}:{port}"
        self._r = self._w = None

    async def open(self):
        self._r, self._w = await asyncio.wait_for(
            asyncio.open_connection(self.host, self.port), self.timeout
        )
        idn = await self._ask("*IDN?")
        print(f"[dmm] {idn}")
        if self.conf_cmd:
            self._w.write((self.conf_cmd + "\n").encode())
            await self._w.drain()
            # Let the configure settle before the first READ?, which on a
            # freshly ranged DMM is the reading most likely to be stale.
            await asyncio.sleep(0.5)

    async def _ask(self, cmd):
        self._w.write((cmd + "\n").encode())
        await self._w.drain()
        line = await asyncio.wait_for(self._r.readline(), self.timeout)
        return line.decode(errors="replace").strip()

    async def read(self):
        return float(await self._ask(self.read_cmd))

    async def close(self):
        if self._w is not None:
            self._w.close()
            with contextlib.suppress(Exception):
                await self._w.wait_closed()


class VisaDmm:
    """The SDM3055X-E over its USB port, which enumerates as USB-TMC.

    pyvisa is blocking, so every exchange goes through a worker thread to keep
    the BLE notifications flowing while the DMM integrates -- an AC current
    reading is hundreds of milliseconds, and a frame arriving inside that
    window must not be delayed by it.

    `resource=None` means "find it": there is one instrument on the bench, and
    typing its serial number into a command line is a step that only exists to
    be got wrong.
    """

    def __init__(self, resource, conf_cmd, read_cmd, timeout):
        self.resource = resource
        self.conf_cmd = conf_cmd
        self.read_cmd = read_cmd
        self.timeout = timeout
        self.label = f"visa://{resource}" if resource else "usb"
        self._inst = None

    async def open(self):
        try:
            import pyvisa
        except ImportError:
            raise RuntimeError(
                "需要 pyvisa: pip install pyvisa pyvisa-py pyusb"
                "\n  只收蓝牙不接万用表的话用 --dmm none"
            ) from None

        rm = pyvisa.ResourceManager()
        resource = self.resource
        if resource is None:
            resource = discover_dmm(rm)
            self.label = f"visa://{resource}"
        self._inst = rm.open_resource(resource)
        self._inst.timeout = int(self.timeout * 1000)
        # Deterministic rather than whatever this pyvisa backend defaults to:
        # a stray CR on the wire is the kind of thing that works on one machine
        # and silently truncates a reply on the next.
        self._inst.write_termination = "\n"
        idn = await self._query("*IDN?")
        print(f"[dmm] {idn}")
        if self.conf_cmd:
            await asyncio.to_thread(self._inst.write, self.conf_cmd)
            await asyncio.sleep(0.5)

    async def _query(self, cmd):
        return (await asyncio.to_thread(self._inst.query, cmd)).strip()

    async def read(self):
        return float(await self._query(self.read_cmd))

    async def close(self):
        if self._inst is not None:
            await asyncio.to_thread(self._inst.close)


def make_dmm(spec, conf_cmd, read_cmd, timeout):
    if spec in (None, "", "none"):
        return NullDmm()
    if spec == "usb":
        return VisaDmm(None, conf_cmd, read_cmd, timeout)
    if spec.startswith("visa://"):
        return VisaDmm(spec[len("visa://") :], conf_cmd, read_cmd, timeout)
    # A bare VISA resource string, pasted straight out of --list-dmm.
    if "::" in spec and not spec.startswith("tcp://"):
        return VisaDmm(spec, conf_cmd, read_cmd, timeout)
    if spec.startswith("tcp://"):
        spec = spec[len("tcp://") :]
    host, _, port = spec.partition(":")
    return TcpDmm(host, int(port or 5025), conf_cmd, read_cmd, timeout)


def usb_resources(rm):
    """USB instruments only.

    pyvisa-py also probes for LAN instruments during `list_resources`, which on
    a bench with no network produces nothing but warnings about the optional
    packages that probe would have used. Suppressed rather than left to
    scroll: a warning that is always there is a warning nobody reads.
    """
    import warnings

    with warnings.catch_warnings():
        warnings.simplefilter("ignore", UserWarning)
        found = list(rm.list_resources())
    return [r for r in found if r.upper().startswith("USB")], found


def identify(rm, resource):
    """`*IDN?` on one resource, or the reason it could not be asked."""
    try:
        inst = rm.open_resource(resource)
        inst.timeout = 3000
        inst.write_termination = "\n"
        try:
            return inst.query("*IDN?").strip()
        finally:
            inst.close()
    except Exception as exc:
        return f"(问不出来: {exc})"


def discover_dmm(rm):
    """The one USB instrument that says it is a multimeter."""
    usb, found = usb_resources(rm)
    if not usb:
        raise RuntimeError(
            "没找到 USB 仪器。VISA 看到的是: "
            + (", ".join(found) or "(空)")
            + "\n  macOS 上通常缺 libusb: brew install libusb"
        )
    if len(usb) == 1:
        return usb[0]

    matches = [r for r in usb if DMM_IDN_HINT in identify(rm, r).upper()]
    if len(matches) == 1:
        return matches[0]
    raise RuntimeError(
        f"USB 上有 {len(usb)} 台仪器，认出 {len(matches)} 台像万用表的"
        f"（按 *IDN? 里的 {DMM_IDN_HINT}）。用 --list-dmm 看清楚，然后 --dmm <资源名> 指定：\n  "
        + "\n  ".join(usb)
    )


def list_dmm():
    """Print what VISA can see, with each instrument's own answer to who it is,
    so a resource string can be copied rather than guessed."""
    try:
        import pyvisa
    except ImportError:
        print("需要 pyvisa: pip install pyvisa pyvisa-py pyusb")
        return 1
    rm = pyvisa.ResourceManager()
    usb, found = usb_resources(rm)
    if not found:
        print("VISA 没看到任何仪器。macOS 上通常缺 libusb: brew install libusb")
        return 1
    for resource in found:
        who = identify(rm, resource) if resource in usb else ""
        print(f"  {resource}\n      {who}")
    return 0


class Keyboard:
    """Single-keypress control without blocking the event loop.

    cbreak rather than raw so ctrl-C still reaches the process as a signal --
    the terminal must come back sane even if this script dies badly, and
    leaving the exit path to the signal handler is what makes that true.
    Degrades to no-keys when stdin is not a terminal, so the script is still
    usable under `nohup` or a pipe.
    """

    def __init__(self, on_key):
        self.on_key = on_key
        self.fd = sys.stdin.fileno() if sys.stdin.isatty() else None
        self.saved = None

    def __enter__(self):
        if self.fd is None:
            return self
        self.saved = termios.tcgetattr(self.fd)
        tty.setcbreak(self.fd)
        asyncio.get_running_loop().add_reader(self.fd, self._ready)
        return self

    def _ready(self):
        key = os.read(self.fd, 1).decode(errors="ignore")
        if key:
            self.on_key(key)

    def __exit__(self, *exc):
        if self.fd is None:
            return
        asyncio.get_running_loop().remove_reader(self.fd)
        termios.tcsetattr(self.fd, termios.TCSADRAIN, self.saved)


class Logger:
    """CSV appender that flushes every row.

    A calibration run is long, interactive and easy to interrupt; a row that
    reached the file is a row that survives, and buffering would trade exactly
    that for nothing that matters at these rates.
    """

    def __init__(self, path):
        new = not os.path.exists(path) or os.path.getsize(path) == 0
        self.file = open(path, "a", newline="", encoding="utf-8")
        self.writer = csv.DictWriter(self.file, CSV_FIELDS, extrasaction="ignore")
        if new:
            self.writer.writeheader()
            self.file.flush()
        self.rows = 0

    def write(self, row):
        self.writer.writerow(row)
        self.file.flush()
        self.rows += 1

    def close(self):
        self.file.close()


async def find_device(name_filter, timeout):
    """Scan and return the best match, or None.

    macOS hands out per-host UUIDs instead of MAC addresses, so matching on the
    advertised name is the only portable way to pick the meter; --address
    exists for when the name is ambiguous.
    """
    require_bleak()
    print(f"[ble] 扫描 {timeout:.0f}s ...")
    found = await BleakScanner.discover(timeout=timeout)
    hints = (name_filter.upper(),) if name_filter else NAME_HINTS
    matches = [
        d for d in found if any(h in (d.name or "").upper() for h in hints)
    ]
    for d in found:
        mark = "*" if d in matches else " "
        print(f"  {mark} {d.address}  {d.name or '(无名)'}")
    if not matches:
        return None
    if len(matches) > 1:
        print(f"[ble] {len(matches)} 个候选，取第一个；要指定用 --address")
    return matches[0]


async def run(args):
    require_bleak()
    device = None
    if not args.address:
        device = await find_device(args.name, args.scan_timeout)
        if device is None:
            print("[ble] 没找到电流表。--list 看看扫到了什么，或者用 --address 指定。")
            return 1
    address = args.address or device.address

    dmm = make_dmm(args.dmm, args.dmm_conf, args.dmm_read, args.dmm_timeout)
    await dmm.open()

    log = Logger(args.out)
    print(f"[csv] {args.out}")

    state = {"paused": False, "quit": False, "dropped": 0}
    # Notification callbacks run on the BLE thread's loop; the queue is what
    # gets the line onto this task, where the DMM read can await without
    # stalling the notification handler.
    lines = asyncio.Queue()
    pending = bytearray()

    def on_key(key):
        if key == " ":
            state["paused"] = not state["paused"]
            if state["paused"]:
                print("\n[暂停] 空格继续")
            else:
                print(f"\n[继续] 暂停期间丢弃 {state['dropped']} 帧")
                state["dropped"] = 0
        elif key in ("q", "Q"):
            state["quit"] = True
            print("\n[退出]")

    def on_notify(_sender, data):
        pending.extend(data)
        now = time.monotonic()
        for line in split_lines(pending):
            lines.put_nowait((now, line))

    async def read_dmm():
        try:
            return await dmm.read()
        except Exception as exc:  # keep the run alive; log the gap
            print(f"  ! 万用表读数失败: {exc}")
            return None

    with Keyboard(on_key):
        async with BleakClient(address) as client:
            print(f"[ble] 已连接 {address}")
            await client.start_notify(CHAR_UUID, on_notify)
            print("[就绪] 空格暂停/继续，q 退出\n")

            while not state["quit"]:
                try:
                    arrived, line = await asyncio.wait_for(lines.get(), 0.2)
                except asyncio.TimeoutError:
                    continue

                if args.echo:
                    print(f"  < {line}")

                row = parse_frame(line)
                if row is None:
                    if not args.echo:
                        print(f"  ? 无法解析: {line}")
                    continue

                if state["paused"]:
                    state["dropped"] += 1
                    continue

                # Trigger the reference *before* waiting on the rest of the
                # burst, not after: every millisecond between the meter's
                # measurement window and the DMM's is a millisecond the load
                # had to change in, and the burst collection below is free
                # while the DMM integrates.
                dmm_task = asyncio.create_task(read_dmm())
                for _, more in await drain_burst(lines, args.burst_window):
                    if args.echo:
                        print(f"  < {more}")
                    extra = parse_frame(more)
                    if extra is None:
                        if not args.echo:
                            print(f"  ? 无法解析: {more}")
                    else:
                        merge_frame(row, extra)
                    line = f"{line} | {more}"
                amps = await dmm_task

                row["time"] = datetime.now().isoformat(timespec="milliseconds")
                row["dmm_a"] = "" if amps is None else f"{amps:.6g}"
                # How far the reference reading trails the frame that asked for
                # it. A row whose lag is a large fraction of the meter's period
                # was paired across a gap the bench should know about.
                row["lag_ms"] = f"{(time.monotonic() - arrived) * 1000:.0f}"
                row["raw"] = line
                log.write(row)

                # The meter free-runs; if it outpaces the DMM, frames queue up
                # and every later pairing drifts further from its own
                # measurement. Said out loud rather than absorbed silently,
                # because the symptom in the data is just a slow calibration
                # drift that looks like the front end.
                if lines.qsize():
                    print(f"  ! 积压 {lines.qsize()} 行，万用表跟不上电流表的节奏")

                shown = " ".join(
                    f"{f}={row[f]}" for f in ("rms_lsb", "gain", "current_ma") if row.get(f)
                )
                amps_txt = "----" if amps is None else f"{amps:.6g} A"
                print(f"  #{log.rows:<4} {amps_txt:>12}   {shown}")

            with contextlib.suppress(Exception):
                await client.stop_notify(CHAR_UUID)

    await dmm.close()
    log.close()
    print(f"\n[完成] {log.rows} 行 -> {args.out}")
    return 0


def main():
    ap = argparse.ArgumentParser(
        description="标定用：一边收 HC-42 的蓝牙帧，一边读 SDM3055X-E，成对存 CSV",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="运行时：空格 暂停/继续，q 退出",
    )
    ap.add_argument("--name", help="按广播名子串挑设备，默认认 HC-42/METER/BYJX_ 等")
    ap.add_argument("--address", help="直接指定 BLE 地址（macOS 上是 UUID），跳过扫描")
    ap.add_argument("--list", action="store_true", help="只扫描并列出设备，不连接")
    ap.add_argument("--scan-timeout", type=float, default=8.0, help="扫描秒数（默认 8）")
    ap.add_argument(
        "--list-dmm", action="store_true", help="列出 VISA 能看到的仪器，不连接"
    )
    ap.add_argument(
        "--dmm",
        default="usb",
        help=(
            "万用表：usb（默认，自动找 USB-TMC 仪器）、"
            "直接粘 VISA 资源名、tcp://IP[:5025]、或 none 只收蓝牙"
        ),
    )
    ap.add_argument(
        "--dmm-conf",
        default="CONF:CURR:AC",
        help="连上后配置一次的 SCPI（默认 CONF:CURR:AC；空字符串表示不配置）",
    )
    ap.add_argument(
        "--dmm-read",
        default="READ?",
        help="每次取数的 SCPI（默认 READ?；某些机型用 MEAS:CURR:AC?）",
    )
    ap.add_argument("--dmm-timeout", type=float, default=10.0, help="万用表超时秒数")
    ap.add_argument(
        "--out",
        default=time.strftime("cal-%Y%m%d-%H%M%S.csv"),
        help="CSV 输出路径，存在则追加",
    )
    ap.add_argument(
        "--burst-window",
        type=float,
        default=0.15,
        help="同一次读数的多行帧合并窗口，秒（默认 0.15，与万用表积分并行不额外耗时）",
    )
    ap.add_argument("--echo", action="store_true", help="打印收到的每一行原文")
    args = ap.parse_args()

    if args.list_dmm:
        return list_dmm()
    if args.list:
        asyncio.run(find_device(args.name, args.scan_timeout))
        return 0
    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        print("\n[中断]")
        return 130


if __name__ == "__main__":
    sys.exit(main())
