#!/usr/bin/env python3
"""通过已安装并信任的用户级 Hook 运行一次真实 Codex 回合。"""
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parent
desktop_binary = Path('/usr/lib/chatgpt/resources/codex')
args = [
    str(desktop_binary) if desktop_binary.is_file() else "codex", "exec", "--ephemeral",
    "--skip-git-repo-check",
    "--sandbox", "read-only", "--cd", str(root),
    "-c", "features.plugins=false",
    "--json",
    "这是键盘状态灯通路验证。不要调用任何工具，不读写文件，不创建子代理。只回复：键盘通路验证完成。",
]
raise SystemExit(subprocess.call(args, stdin=subprocess.DEVNULL))
