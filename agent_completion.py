"""只读核对子代理终态，弥补桌面端缺失 SubagentStop；不保存会话正文。"""
from datetime import datetime
import json
from pathlib import Path
import sqlite3


class CompletionReader:
    def __init__(self, database=None):
        self.database = database or Path.home() / '.codex/state_5.sqlite'
        self.cache = {}

    def latest(self, agent):
        try:
            with sqlite3.connect(f'file:{self.database}?mode=ro', uri=True, timeout=.1) as db:
                row = db.execute('SELECT rollout_path FROM threads WHERE id=?', (agent,)).fetchone()
            if not row:
                return None
            path = Path(row[0])
            stat = path.stat()
            cached = self.cache.setdefault(agent, {'offset': 0, 'identity': None, 'event': None})
            identity = (str(path), stat.st_ino)
            if cached['identity'] != identity or stat.st_size < cached['offset']:
                cached.update(offset=0, identity=identity, event=None)
            with path.open('rb') as stream:
                stream.seek(cached['offset'])
                while True:
                    line = stream.readline()
                    if not line or not line.endswith(b'\n'):
                        break
                    cached['offset'] = stream.tell()
                    try:
                        record = json.loads(line)
                        data = record.get('payload', {})
                        if record.get('type') == 'event_msg' and data.get('type') in ('task_started', 'task_complete'):
                            cached['event'] = (data['type'], data.get('turn_id'),
                                datetime.fromisoformat(record['timestamp'].replace('Z', '+00:00')).timestamp())
                    except (ValueError, KeyError, TypeError):
                        continue
            return cached['event']
        except (OSError, sqlite3.Error):
            # 缺记录不能证明结束，不按时间猜测。
            return None


def reconcile(sessions, reader):
    for parent, entry in sessions.items():
        # 外部multi_agent工具不一定发Start Hook；以当前父回合和实际子回合为准。
        if entry.get('state') in ('running', 'waiting'):
            parent_event = reader.latest(parent)
            if parent_event and parent_event[0] == 'task_started' and parent_event[1] == entry.get('turn'):
                try:
                    with sqlite3.connect(f'file:{reader.database}?mode=ro', uri=True, timeout=.1) as db:
                        children = db.execute('SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id=?', (parent,)).fetchall()
                    for (aid,) in children:
                        event = reader.latest(aid)
                        if event and event[0] == 'task_started' and event[2] >= parent_event[2]:
                            active = entry.setdefault('agents', {}).get(aid)
                            if not active or active.get('turn') != event[1]:
                                entry['agents'][aid] = {'state': 'running', 'turn': event[1], 'updated': event[2]}
                except sqlite3.Error:
                    pass
        agents = entry.get('agents', {})
        for aid, active in list(agents.items()):
            event = reader.latest(aid)
            if not event:
                continue
            kind, turn, timestamp = event
            # 只接受同一轮且晚于活动标记的完成证据，避免旧记录清理再次唤醒。
            if kind == 'task_complete' and timestamp >= active['updated'] and (
                    active.get('turn') is None or active['turn'] == turn):
                agents.pop(aid)
                if entry['state'] == 'done' and not agents:
                    entry['updated'] = timestamp
