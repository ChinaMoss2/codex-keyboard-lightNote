#!/usr/bin/env python3
"""按需回退Python服务；保留Rust二进制接收旧会话缓存事件的能力需重启Codex。"""
import json
from pathlib import Path
import shutil
import subprocess

root = Path(__file__).resolve().parent
backup = root / 'rollback-python'
subprocess.run(['systemctl','--user','stop','codex-keyboard.service'],check=True)
(root / '.rust-enabled').unlink(missing_ok=True)
for name in ['keyboard.py','state_bridge.py','agent_completion.py','install_hooks.py','codex-keyboard.service']:
    shutil.copy2(backup/name,root/name)
shutil.copy2(root/'codex-keyboard.service',Path.home()/'.config/systemd/user/codex-keyboard.service')
p=Path.home()/'.codex/hooks.json'
d=json.loads(p.read_text())
for groups in d['hooks'].values():
    for group in groups:
        for handler in group['hooks']:
            if handler.get('command')==str(root/'bin/codex-keyboard')+' hook':
                handler['command']=f'/usr/bin/python3 {root}/keyboard.py hook'
p.write_text(json.dumps(d,ensure_ascii=False,indent=2)+'\n')
subprocess.run(['python3',str(root/'install_hooks.py')],check=True)
subprocess.run(['systemctl','--user','daemon-reload'],check=True)
subprocess.run(['systemctl','--user','start','codex-keyboard.service'],check=True)
print('已回退Python服务；请重启Codex加载Python Hook命令。')
