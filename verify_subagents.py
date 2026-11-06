#!/usr/bin/env python3
"""一次真实子代理启动、结束、再次唤醒验证；会产生模型调用。"""
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parent
prompt = """验证 Codex 子代理生命周期。用户明确授权创建一个子代理并再次唤醒。
只创建一个 default 子代理，fork_context=false，不覆盖模型。
子代理任务：不读写文件、不调用其他子代理；可执行一次 sleep 4 便于观察键盘，随后只回答“第一轮完成”。
等待它完成，然后向同一个子代理发送后续消息：同样等待4秒，然后只回答“第二轮完成”。
再次等待完成，关闭这个测试子代理（若有关闭工具），主代理最后只报告两轮是否完成。
除上述最小测试外，不修改任何配置或文件，不读取其他项目，不调用任何联网服务。
"""
args = ["/usr/lib/chatgpt/resources/codex", "exec", "--ephemeral",
        "--skip-git-repo-check", "--sandbox", "read-only", "--cd", str(root),
        "-c", "features.plugins=false", "--json", prompt]
raise SystemExit(subprocess.call(args, stdin=subprocess.DEVNULL))
