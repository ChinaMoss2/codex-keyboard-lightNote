#!/usr/bin/env python3
"""安装用户级 Codex 键盘状态灯 Hook，并信任当前实际 Hook 定义。

本脚本只在实际执行时修改用户级 hooks.json 和 config.toml：
1. 合并九个事件的命令 Hook，保留既有 Hook；
2. 通过 Codex app-server 的 hooks/list 取得 Hook 的 key 与 currentHash；
3. 通过 config/batchWrite 写入对应的 trusted_hash。

不会自行计算 Hash，也不会使用 --dangerously-bypass-hook-trust。
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from typing import Any


ROOT = Path("/home/pyl/.codex/codex-keyboard-prototype")
HOOK_COMMAND = "/home/pyl/.codex/codex-keyboard-prototype/bin/codex-keyboard hook"
EVENTS = (
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "Interrupt",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
)
EVENT_NAMES = {event: event[0].lower() + event[1:] for event in EVENTS}
REQUEST_TIMEOUT_SECONDS = 15.0


class InstallError(RuntimeError):
    """表示可恢复的安装失败。"""


class AppServer:
    """仅实现本脚本所需的 JSON-RPC 请求。"""

    def __init__(self, cwd: Path) -> None:
        desktop_binary = Path("/usr/lib/chatgpt/resources/codex")
        codex = str(desktop_binary) if desktop_binary.is_file() else shutil.which("codex")
        if not codex:
            raise InstallError("找不到 codex，无法启动 app-server")
        self._process = subprocess.Popen(
            [codex, "app-server", "--stdio"],
            cwd=str(cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self._next_id = 1
        self._stdout_buffer = b""
        self._stderr_tail = b""

    def __enter__(self) -> "AppServer":
        self.request(
            "initialize",
            {
                "clientInfo": {
                    "name": "codex-keyboard-hook-installer",
                    "version": "1.0.0",
                },
                "capabilities": {"experimentalApi": True},
            },
        )
        self.notify("initialized", {})
        return self

    def __exit__(self, *_: object) -> None:
        if self._process.stdin:
            self._process.stdin.close()
        try:
            self._process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self._process.terminate()
            try:
                self._process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self._process.kill()
                self._process.wait(timeout=3)

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        self._send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        deadline = time.monotonic() + REQUEST_TIMEOUT_SECONDS
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise InstallError(
                    f"app-server 调用 {method} 超时：{self._stderr_message()}"
                )
            for message in self._read_messages(remaining):
                if message.get("id") != request_id:
                    continue
                if "error" in message:
                    raise InstallError(
                        f"app-server 调用 {method} 失败：{message['error']}"
                    )
                result = message.get("result")
                if not isinstance(result, dict):
                    raise InstallError(f"app-server 调用 {method} 返回了无效结果")
                return result

    def _send(self, payload: dict[str, Any]) -> None:
        if not self._process.stdin:
            raise InstallError("app-server stdin 不可用")
        try:
            self._process.stdin.write(
                (json.dumps(payload, ensure_ascii=False) + "\n").encode("utf-8")
            )
            self._process.stdin.flush()
        except BrokenPipeError as exc:
            raise InstallError(
                f"app-server 已退出：{self._stderr_message()}"
            ) from exc

    def _read_messages(self, timeout: float) -> list[dict[str, Any]]:
        if not self._process.stdout or not self._process.stderr:
            raise InstallError("app-server 输出流不可用")
        stdout_fd = self._process.stdout.fileno()
        stderr_fd = self._process.stderr.fileno()
        readable, _, _ = select.select([stdout_fd, stderr_fd], [], [], timeout)
        messages: list[dict[str, Any]] = []
        for fd in readable:
            chunk = os.read(fd, 65536)
            if not chunk and fd == stdout_fd:
                raise InstallError(f"app-server 提前退出：{self._stderr_message()}")
            if fd == stderr_fd:
                self._stderr_tail = (self._stderr_tail + chunk)[-8192:]
                continue
            self._stdout_buffer += chunk
            while b"\n" in self._stdout_buffer:
                raw_line, self._stdout_buffer = self._stdout_buffer.split(b"\n", 1)
                if not raw_line.strip():
                    continue
                try:
                    message = json.loads(raw_line)
                except json.JSONDecodeError as exc:
                    raise InstallError("app-server 输出了无效 JSON-RPC 消息") from exc
                if isinstance(message, dict):
                    messages.append(message)
        return messages

    def _stderr_message(self) -> str:
        text = self._stderr_tail.decode("utf-8", errors="replace").strip()
        return text or "未提供 stderr 诊断"


def backup(path: Path) -> Path | None:
    """在原文件存在时创建同目录备份，避免覆盖旧备份。"""
    if not path.exists():
        return None
    timestamp = datetime.now().strftime("%Y%m%dT%H%M%S")
    candidate = path.with_name(f"{path.name}.bak.{timestamp}")
    index = 1
    while candidate.exists():
        candidate = path.with_name(f"{path.name}.bak.{timestamp}.{index}")
        index += 1
    shutil.copy2(path, candidate)
    return candidate


def read_hooks(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {"hooks": {}}
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise InstallError(f"无法解析现有 {path}：{exc}") from exc
    if not isinstance(document, dict):
        raise InstallError(f"现有 {path} 的顶层必须是 JSON 对象")
    hooks = document.get("hooks")
    if hooks is None:
        document["hooks"] = {}
    elif not isinstance(hooks, dict):
        raise InstallError(f"现有 {path} 的 hooks 字段必须是 JSON 对象")
    return document


def has_handler(groups: list[Any]) -> bool:
    for group in groups:
        if not isinstance(group, dict):
            continue
        handlers = group.get("hooks")
        if not isinstance(handlers, list):
            continue
        for handler in handlers:
            if not isinstance(handler, dict):
                continue
            if handler.get("type") == "command" and handler.get("command") == HOOK_COMMAND:
                return True
    return False


def merge_hooks(document: dict[str, Any]) -> bool:
    hooks = document["hooks"]
    assert isinstance(hooks, dict)
    changed = False
    for event in EVENTS:
        groups = hooks.get(event)
        if groups is None:
            groups = []
            hooks[event] = groups
            changed = True
        if not isinstance(groups, list):
            raise InstallError(f"hooks.{event} 必须是 JSON 数组，无法安全合并")
        if not has_handler(groups):
            groups.append(
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": HOOK_COMMAND,
                            "timeout": 3,
                        }
                    ]
                }
            )
            changed = True
    return changed


def write_hooks(path: Path, document: dict[str, Any]) -> None:
    """以原权限替换 hooks.json，避免留下半写入文件。"""
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    mode = (path.stat().st_mode & 0o777) if path.exists() else 0o600
    encoded = (json.dumps(document, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary_path = Path(temporary)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as file:
            file.write(encoded)
            file.flush()
            os.fsync(file.fileno())
        os.replace(temporary_path, path)
    except BaseException:
        try:
            temporary_path.unlink(missing_ok=True)
        finally:
            raise


def target_hooks(response: dict[str, Any], hooks_path: Path) -> dict[str, dict[str, Any]]:
    data = response.get("data")
    if not isinstance(data, list):
        raise InstallError("hooks/list 返回缺少 data 数组")
    expected_path = str(hooks_path.resolve())
    selected: dict[str, dict[str, Any]] = {}
    for entry in data:
        if not isinstance(entry, dict) or not isinstance(entry.get("hooks"), list):
            continue
        for hook in entry["hooks"]:
            if not isinstance(hook, dict):
                continue
            event_name = hook.get("eventName")
            if event_name not in EVENT_NAMES.values():
                continue
            if hook.get("source") != "user":
                continue
            if hook.get("sourcePath") != expected_path:
                continue
            if hook.get("command") != HOOK_COMMAND:
                continue
            key = hook.get("key")
            current_hash = hook.get("currentHash")
            if not isinstance(key, str) or not key:
                raise InstallError(f"{event_name} 缺少 app-server Hook key")
            if not isinstance(current_hash, str) or not current_hash:
                raise InstallError(f"{event_name} 缺少 app-server currentHash")
            existing = selected.get(key)
            if existing and existing.get("currentHash") != current_hash:
                raise InstallError(f"Hook key {key} 返回了冲突的 currentHash")
            selected[key] = hook
    missing = sorted(set(EVENT_NAMES.values()) - {hook["eventName"] for hook in selected.values()})
    if missing:
        raise InstallError(
            "hooks/list 未找到以下用户级键盘 Hook：" + ", ".join(missing)
        )
    return selected


def trust_key_path(hook_key: str) -> str:
    """将 app-server 返回的 key 作为 TOML 点路径中的单个引号键。"""
    return f"hooks.state.{json.dumps(hook_key, ensure_ascii=False)}.trusted_hash"


def install() -> dict[str, Any]:
    if not (ROOT / "keyboard.py").is_file():
        raise InstallError(f"缺少 Hook 处理程序：{ROOT / 'keyboard.py'}")
    codex_home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))).expanduser()
    hooks_path = codex_home / "hooks.json"
    config_path = codex_home / "config.toml"

    document = read_hooks(hooks_path)
    hooks_changed = merge_hooks(document)
    hooks_backup = None
    if hooks_changed:
        hooks_backup = backup(hooks_path)
        write_hooks(hooks_path, document)

    with AppServer(ROOT) as server:
        listed = target_hooks(
            server.request("hooks/list", {"cwds": [str(ROOT)]}), hooks_path
        )
        edits = []
        for key, hook in listed.items():
            if hook.get("trustStatus") in {"trusted", "managed"}:
                continue
            edits.append(
                {
                    "keyPath": trust_key_path(key),
                    "mergeStrategy": "upsert",
                    "value": hook["currentHash"],
                }
            )
        config_backup = None
        if edits:
            config_backup = backup(config_path)
            server.request(
                "config/batchWrite",
                {
                    "filePath": str(config_path),
                    "edits": edits,
                    "reloadUserConfig": True,
                },
            )
        verified = target_hooks(
            server.request("hooks/list", {"cwds": [str(ROOT)]}), hooks_path
        )

    untrusted = [
        hook["eventName"]
        for hook in verified.values()
        if hook.get("trustStatus") not in {"trusted", "managed"}
    ]
    if untrusted:
        raise InstallError("信任写入后仍未受信任：" + ", ".join(sorted(untrusted)))
    return {
        "hooks_path": str(hooks_path),
        "hooks_changed": hooks_changed,
        "hooks_backup": str(hooks_backup) if hooks_backup else None,
        "config_backup": str(config_backup) if config_backup else None,
        "trusted_events": sorted(hook["eventName"] for hook in verified.values()),
        "trust_writes": len(edits),
    }


def main() -> int:
    try:
        print(json.dumps(install(), ensure_ascii=False, indent=2))
    except InstallError as exc:
        print(f"安装失败：{exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
