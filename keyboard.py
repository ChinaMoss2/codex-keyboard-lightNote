#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""CHERRY 046a:0079 最小状态灯原型，仅依赖 Python 标准库。

报文依据 OpenRGB CherryKeyboardController.cpp 的 SendKeyboardModeEx、
SendKeyboardParameter 与 ComputeChecksum（GPL-2.0-or-later）。
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import select
import sys
import time

ROOT = Path(__file__).resolve().parent
STATES = {
    "running": (0, 32, 127),
    "running_subagents": (0, 32, 127),
    "subagents_only": (0, 0, 0),
    "waiting": (127, 55, 0),
    "done": (0, 127, 16),
    "idle": (0, 0, 0),
}
EVENTS = {
    "UserPromptSubmit": "running",
    "PermissionRequest": "waiting",
    "PreToolUse": "running",
    "PostToolUse": "running",
    "Stop": "done",
    "Interrupt": "idle",
    "SessionEnd": "idle",
    "SubagentStart": "running",
    "SubagentStop": "done",
}

# OpenRGB CHERRY 6×21 矩阵中右侧数字小键盘的 17 个有效位置。
NUMPAD_LEDS = frozenset((103, 109, 115, 121, 104, 110, 116,
                         105, 111, 117, 122, 106, 112, 118, 113, 119, 124))


def checksum(report):
    value = sum(x if x < 128 else x - 256 for x in report[3:]) & 0xffff
    report[1:3] = value.to_bytes(2, "little")
    return report


def device():
    matches = []
    for node in Path("/sys/class/hidraw").glob("*"):
        metadata = (node / "device/uevent").read_text()
        if "HID_ID=0003:0000046A:00000079\n" not in metadata:
            continue
        descriptor = (node / "device/report_descriptor").read_bytes()
        if b"\x06\x1c\xff" in descriptor:
            matches.append(Path("/dev") / node.name)
    if len(matches) != 1:
        raise RuntimeError(f"需要恰好一把匹配键盘，实际找到 {len(matches)} 个接口")
    return matches[0]


def packet(state):
    # 空闲：Single Key、随机颜色、最高亮度、最慢速度；其余状态为静态色。
    if state == "idle":
        data = bytes([0, 0x15, 4, 4, 0, 1, *STATES[state]])
    elif state in ("running_subagents", "subagents_only"):
        data = bytes([0, 0x08, 4, 2, 0, 0, *STATES[state]])
    else:
        data = bytes([0, 3, 4, 2, 0, 0, *STATES[state]])
    report = bytearray(64)
    report[0] = 4
    report[3:8] = bytes([6, len(data), 0, 0, 0x55])
    report[8:8 + len(data)] = data
    return checksum(report)


def reports(state):
    if state in ("running_subagents", "subagents_only"):
        colors = bytes(component for index in range(126)
                       for component in STATES['done' if index in NUMPAD_LEDS else ('subagents_only' if state == 'subagents_only' else 'running')])
        for offset in range(0, len(colors), 54):
            data = colors[offset:offset + 54]
            report = bytearray(64)
            report[0] = 4
            report[3:7] = bytes([0x0b, len(data), offset & 255, offset >> 8])
            report[8:8 + len(data)] = data
            yield checksum(report)
    yield packet(state)


def set_state(state, event=None, session=None):
    with (ROOT / ".device.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        path = device()
        fd = os.open(path, os.O_RDWR | os.O_NONBLOCK)
        try:
            count = 0
            replies = []
            for report in reports(state):
                written = os.write(fd, report)
                if written != len(report):
                    raise RuntimeError(f"HID 写入不完整：{written}/64")
                count += written
                if not select.select([fd], [], [], 0.7)[0]:
                    raise TimeoutError("HID 写入已返回，但 700ms 内未收到设备响应")
                reply = os.read(fd, 64)
                if not reply or reply[0] != 4:
                    raise RuntimeError(f"意外的 HID 响应：{reply.hex()}")
                replies.append(reply.hex())
        finally:
            os.close(fd)
        record = {
            "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "state": state, "event": event, "session": session,
            "device": str(path), "written": count, "reply": reply.hex(),
            "replies": replies,
        }
        log_path = ROOT / "events.jsonl"
        if log_path.exists() and log_path.stat().st_size > 262144:
            log_path.replace(ROOT / "events.previous.jsonl")
        with log_path.open("a") as log:
            log.write(json.dumps(record, ensure_ascii=False) + "\n")
        return record


def main():
    # 已打开会话可能缓存旧命令；过渡期转交Rust，禁止两套状态写入器并行。
    native = ROOT / 'bin/codex-keyboard'
    if (ROOT / '.rust-enabled').exists() and len(sys.argv) > 1 and sys.argv[1] in ('hook', 'status', 'daemon'):
        os.execv(str(native), [str(native), *sys.argv[1:]])
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("hook")
    sub.add_parser("daemon")
    sub.add_parser("status")
    setter = sub.add_parser("set")
    setter.add_argument("state", choices=STATES)
    args = parser.parse_args()
    if args.command in ("daemon", "status"):
        import state_bridge
        if args.command == "daemon":
            state_bridge.run()
        else:
            print(json.dumps(state_bridge.snapshot(), ensure_ascii=False, indent=2))
        return
    if args.command == "hook":
        try:
            payload = json.load(sys.stdin)
            event = payload.get("hook_event_name")
            if event in EVENTS:
                import state_bridge
                state_bridge.submit(payload)
        except Exception as exc:
            # 灯光异常不能阻止 Codex 正常工作。
            print(f"键盘状态灯错误：{exc}", file=sys.stderr)
        print("{}")
        return
    print(json.dumps(set_state(args.state), ensure_ascii=False))


if __name__ == "__main__":
    main()
