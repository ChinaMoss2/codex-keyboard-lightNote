"""本地会话聚合与灯光服务，只保存事件标识，不保存对话或工具内容。"""
from contextlib import contextmanager
import fcntl
import copy
import json
import os
from pathlib import Path
import time

import keyboard

ROOT = Path(__file__).resolve().parent
STATE = ROOT / "sessions.json"
DONE_SECONDS = 5


def process_identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return int(fields[1]), fields[19]
    except (OSError, ValueError, IndexError):
        return None


def owner():
    pid = os.getppid()
    for _ in range(20):
        identity = process_identity(pid)
        if not identity or pid <= 1:
            break
        try:
            name = Path(f"/proc/{pid}/comm").read_text().strip()
        except OSError:
            break
        if name == "codex":
            return [pid, identity[1]]
        pid = identity[0]
    return None


def alive(entry):
    process = entry.get("owner")
    if not process:
        return True
    identity = process_identity(process[0])
    return identity is not None and identity[1] == process[1]


def object_value(value):
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except ValueError:
            return {}
    return value if isinstance(value, dict) else {}


def bind_aliases(entry):
    # 仅在候选唯一时关联工具返回的任务名，避免并发启动时猜测对应关系。
    aliases = entry.setdefault('aliases', {})
    unbound_names = [k for k,v in aliases.items() if v is None]
    unbound_ids = set(entry.get('known_agents', [])) - set(aliases.values())
    if len(unbound_names) == len(unbound_ids) == 1:
        aliases[unbound_names[0]] = unbound_ids.pop()


def reduce_state(sessions, payload, now, process):
    # 已结束事件短暂保留；运行进程退出后自动移除其活动状态。
    sessions = {key: copy.deepcopy(value) for key, value in sessions.items()
                if (value.get('agents') and alive(value))
                or (value["state"] == "done" and now - value["updated"] < DONE_SECONDS)
                or (value["state"] != "done" and alive(value))}
    if payload:
        session = payload.get("session_id")
        event = payload.get("hook_event_name")
        turn = payload.get("turn_id")
        if session and event in keyboard.EVENTS:
            previous = sessions.get(session, {})
            agents = previous.get('agents', {})
            agent = payload.get('agent_id')
            if event == 'PostToolUse' and not agent:
                name = payload.get('tool_name')
                args = object_value(payload.get('tool_input'))
                response = object_value(payload.get('tool_response'))
                if name == 'collaborationspawn_agent' and response.get('task_name'):
                    previous.setdefault('aliases', {})[response['task_name']] = None
                    bind_aliases(previous)
                if name == 'collaborationfollowup_task' and not response.get('error') and not response.get('isError'):
                    target = args.get('target')
                    target = previous.get('aliases', {}).get(target, target)
                    if target in previous.get('known_agents', []):
                        agents[target] = {'state': 'running', 'turn': None, 'updated': now}
            if event in ('SubagentStart', 'SubagentStop') or agent:
                if not agent:
                    return sessions, select_state(sessions)
                old = agents.get(agent, {})
                if event in ('SubagentStop', 'Stop', 'Interrupt', 'SessionEnd'):
                    if not old:
                        return sessions, select_state(sessions)
                    if turn and old.get('turn') and turn != old['turn']:
                        return sessions, select_state(sessions)
                    agents.pop(agent, None)
                elif event == 'SubagentStart' and old.get('turn') == turn:
                    return sessions, select_state(sessions)
                else:
                    agents[agent] = {'updated': now, 'turn': turn,
                                     'state': 'waiting' if event == 'PermissionRequest' else 'running'}
                entry = dict(previous) if previous else {
                    'state': 'running', 'turn': None, 'owner': process}
                entry['known_agents'] = sorted(set(entry.get('known_agents', [])) | {agent})
                bind_aliases(entry)
                entry['agents'] = agents
                entry['updated'] = now
                sessions[session] = entry
                return sessions, select_state(sessions)
            stale = (event in ("Stop", "Interrupt") and turn
                     and previous.get("turn") and previous["turn"] != turn)
            if not stale:
                state = keyboard.EVENTS[event]
                if event == "SessionEnd" and previous.get("state") == "done" and not agents:
                    pass
                elif state == "idle":
                    sessions.pop(session, None)
                else:
                    sessions[session] = {**previous, "state": state, "updated": now,
                                         "owner": process, "turn": turn,
                                         "event": event, 'agents': agents}
    return sessions, select_state(sessions)


def select_state(sessions):
    states = {entry['state'] for entry in sessions.values()}
    if 'waiting' in states or any(a.get('state') == 'waiting'
            for e in sessions.values() for a in e.get('agents', {}).values()):
        return 'waiting'
    if any(entry.get('agents') for entry in sessions.values()):
        return 'running_subagents'
    return next((s for s in ('running', 'done') if s in states), 'idle')


@contextmanager
def locked():
    with (ROOT / ".state.lock").open("w") as lock:
        deadline = time.monotonic() + 1
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("状态锁等待超时")
                time.sleep(0.01)
        yield


def load():
    return json.loads(STATE.read_text()) if STATE.exists() else {}


def save(sessions):
    tmp = STATE.with_suffix(".tmp")
    tmp.write_text(json.dumps(sessions, ensure_ascii=False))
    tmp.chmod(0o600)
    tmp.replace(STATE)


def submit(payload):
    with locked():
        sessions, _ = reduce_state(load(), payload, time.time(), owner())
        save(sessions)
        record = {k: payload[k] for k in ('hook_event_name', 'session_id', 'agent_id',
                   'turn_id', 'tool_name', 'stop_hook_active') if k in payload}
        record['time'] = time.time()
        path = ROOT / 'hook-events.jsonl'
        if path.exists() and path.stat().st_size > 262144:
            path.replace(ROOT / 'hook-events.previous.jsonl')
        with path.open('a') as log:
            log.write(json.dumps(record) + '\n')


def snapshot():
    with locked():
        sessions, selected = reduce_state(load(), None, time.time(), None)
    return {"state": selected, "sessions": sessions}


def run():
    from agent_completion import CompletionReader, reconcile
    completion_reader = CompletionReader()
    next_check = 0
    # 单实例，防止多个进程争抢同一 HID 接口。
    with (ROOT / ".daemon.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        last = None
        last_error = None
        while True:
            try:
                with locked():
                    previous = load()
                    sessions, selected = reduce_state(previous, None, time.time(), None)
                    if time.monotonic() >= next_check:
                        reconcile(sessions, completion_reader)
                        selected = select_state(sessions)
                        next_check = time.monotonic() + 1
                    if sessions != previous:
                        save(sessions)
                path = keyboard.device()
                # inode 与 sysfs 实际路径让重新插入的设备重新接收当前状态。
                identity = (selected, path.stat().st_ino,
                            str((Path('/sys/class/hidraw') / path.name).resolve()))
                if identity != last:
                    keyboard.set_state(selected, "aggregate")
                    print(json.dumps({"state": selected, "sessions": len(sessions)}), flush=True)
                    last = identity
                last_error = None
            except (OSError, RuntimeError, ValueError) as exc:
                last = None
                message = str(exc)
                if message != last_error:
                    print(f"键盘服务等待恢复：{message}", flush=True)
                    last_error = message
            time.sleep(0.25)
