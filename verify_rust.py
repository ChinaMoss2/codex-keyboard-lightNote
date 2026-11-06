#!/usr/bin/env python3
"""在临时目录验证Rust IPC与资源开销，不写生产状态或键盘。"""
import json
import os
from pathlib import Path
import resource
import shutil
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent
BINARY = ROOT / 'rust/target/release/codex-keyboard'


def cpu(pid):
    parts = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return (int(parts[11]) + int(parts[12])) / os.sysconf('SC_CLK_TCK')


def memory(pid):
    lines = Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()
    return {line.split(':')[0]: int(line.split()[1]) for line in lines
            if line.startswith(('Rss:', 'Pss:'))}


def main():
    with tempfile.TemporaryDirectory(prefix='keyboard-rust-check-') as tmp:
        path = Path(tmp)
        env = dict(os.environ, CODEX_KEYBOARD_ROOT=tmp, CODEX_HOME=tmp)
        process = subprocess.Popen([str(BINARY), 'daemon', '--dry-run'], env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            for _ in range(100):
                if (path / 'control.sock').exists():
                    break
                if process.poll() is not None:
                    raise RuntimeError(process.stderr.read().decode())
                time.sleep(.02)

            def status():
                return json.loads(subprocess.check_output([str(BINARY), 'status'], env=env))

            def event(name, agent=None, turn='p'):
                p = dict(hook_event_name=name, session_id='verification', turn_id=turn)
                if agent:
                    p['agent_id'] = agent
                subprocess.run([str(BINARY), 'hook'], input=json.dumps(p).encode(),
                               stdout=subprocess.DEVNULL, env=env, check=True)
                return status()['state']

            assert event('UserPromptSubmit') == 'running'
            assert event('SubagentStart', 'a', 'child1') == 'running_subagents'
            assert event('SubagentStart', 'b', 'child2') == 'running_subagents'
            assert event('SubagentStop', 'a', 'child1') == 'running_subagents'
            assert event('PermissionRequest', 'b', 'child2') == 'waiting'
            assert event('PostToolUse', 'b', 'child2') == 'running_subagents'
            assert event('SubagentStop', 'b', 'child2') == 'running'
            assert event('Stop') == 'done'
            time.sleep(5.2)
            assert status()['state'] == 'idle'
            rss = memory(process.pid)
            a = cpu(process.pid); start = time.monotonic()
            time.sleep(8)
            idle_cpu = cpu(process.pid) - a
            elapsed = time.monotonic() - start

            # 相同无效事件走完整Hook入口；不改变业务状态或接触硬件。
            payload = json.dumps({'hook_event_name': 'BenchmarkNoop', 'session_id': 'bench'}).encode()
            for name in ['keyboard.py', 'state_bridge.py']:
                shutil.copy2(ROOT / name, path / name)
            result = {}
            for label, command, environment in [
                ('python', ['/usr/bin/python3', str(path / 'keyboard.py'), 'hook'], dict(os.environ)),
                ('rust', [str(BINARY), 'hook'], env),
            ]:
                times = []
                before = resource.getrusage(resource.RUSAGE_CHILDREN)
                for _ in range(30):
                    t = time.perf_counter()
                    subprocess.run(command, input=payload, env=environment,
                                   stdout=subprocess.DEVNULL, check=True)
                    times.append((time.perf_counter() - t) * 1000)
                after = resource.getrusage(resource.RUSAGE_CHILDREN)
                result[label] = dict(median_ms=statistics.median(times), p95_ms=sorted(times)[28],
                                    client_cpu_ms=((after.ru_utime+after.ru_stime)-(before.ru_utime+before.ru_stime))*1000/30)
            print(json.dumps(dict(ipc='通过', samples=30, hook=result,
                                  daemon_memory_kib=rss, idle_seconds=elapsed,
                                  idle_cpu_seconds=idle_cpu), ensure_ascii=False, indent=2))
        finally:
            process.terminate()
            process.wait(timeout=5)


if __name__ == '__main__':
    main()
