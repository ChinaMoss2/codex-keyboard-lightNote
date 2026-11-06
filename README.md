# codex-keyboard-lightNote

基于 Rust 的 Codex 键盘状态灯，通过 CHERRY RGB 键盘显示主会话、子代理、等待输入与多会话 Spectrum 状态。

## 仓库内容与本机部署

仓库保留 Rust 源码、依赖锁文件、Python 兼容与辅助脚本、部署配置和验证文档。`.gitignore` 排除运行日志、会话状态、IPC/锁文件、待处理事件、编译缓存、生产二进制、本机 Rust 启用标记和 `rollback-python` 回滚快照；这些本地文件不会因上传而删除。

GitHub 仓库名为 `codex-keyboard-lightNote`，现有本机部署目录仍为 `/home/pyl/.codex/codex-keyboard-prototype`，不因上传改名。本文路径、systemd 服务和 udev 规则中的 `pyl` 是原机器配置，迁移到其他机器前需调整。全新环境若没有依赖缓存，构建时不要使用 `--offline`；构建产物须另行部署到 `bin/codex-keyboard`。仓库不包含本机回滚快照，克隆后不能直接运行 `rollback_rust.py` 完成本机历史版本回退。

以下保留开发与实机验证记录，历史测试结果不代表在新机器上的验证结果。

## 项目背景

2026-09-25 修复中断子代理残留：实际子回合以 `turn_aborted` 结束且未收到 SubagentStop，旧补偿仅识别 task_complete。Rust补偿现将同轮 turn_aborted 视为终态，保持回合与时间校验，避免清理新回合。新增回归先复现失败再通过，10项Rust测试全部通过；对问题会话的真实状态单独隔离重放，已回到idle，随后更新生产服务。空闲仍为Single Key随机渐灭，不改变为空闲常亮或强制全灭。

适用：Ubuntu、CHERRY MX BOARD 3.0S RGB，USB ID `046a:0079`。

项目实体已于2026-09-24迁入 `/home/pyl/.codex/codex-keyboard-prototype`。systemd服务、用户级9个Hook、安装器与本文命令均使用新路径，Hook信任已重新核对。旧主目录入口仅保留兼容软链接，供已启动且缓存旧Hook路径的Codex会话过渡，不是第二份项目；所有旧会话退出后可移除软链接。迁移后4项状态测试和一次真实Codex回合验证通过。

已完成Rust重构并切换生产服务。资源对比见 [RESOURCE_ASSESSMENT.md](RESOURCE_ASSESSMENT.md)。灯光规则保持，新增缺少SubagentStart的multi_agent_v1发现补偿。

通路：原生Rust Hook客户端 → Unix套接字 → Rust常驻服务内存状态机 → USB HID → 键盘颜色。

## Rust运行与构建

```bash
/home/pyl/.codex/codex-keyboard-prototype/bin/codex-keyboard status
cargo test --offline --manifest-path /home/pyl/.codex/codex-keyboard-prototype/rust/Cargo.toml
cargo build --release --offline --manifest-path /home/pyl/.codex/codex-keyboard-prototype/rust/Cargo.toml
```

源码为 `rust/src`，生产二进制为 `bin/codex-keyboard`。构建产物不会自动覆盖生产二进制。使用系统libsqlite3，只读连接复用。安装和一次性验收工具仍可用Python；日常新Hook和后台服务均使用Rust。

已打开的Codex会话可能缓存Python命令；`keyboard.py` 在 `.rust-enabled` 存在时将hook/status/daemon转交Rust。重启Codex后才完全消除旧入口的Python启动成本。已有主目录软链接继续兼容旧路径。

回退文件在 `rollback-python`；需要回退时运行 `python3 /home/pyl/.codex/codex-keyboard-prototype/rollback_rust.py`，随后重启Codex。回退脚本不覆盖其他Hook或模型配置。

## 日常行为

优先级：橙色等待审批 > 蓝色运行 > 绿色回合结束（5 秒）> 空闲 Single Key 随机颜色。
运行时存在活动子代理：主键区、功能键与方向键仍蓝色，右侧数字小键盘显示绿色。若主任务已停止但子代理仍运行，状态为 `subagents_only`：主键区、功能键与方向键熄灭，只保留数字小键盘绿色。多个子代理以活动集合聚合，最后一个结束后再进入绿色完成提示或空闲灯效。审批时仍全橙色。
2026-09-29：计划模式通过 `request_user_input` 等待用户回答时，主键区显示橙色常亮（最高亮度），优先于主区蓝色运行/Spectrum；回答后恢复当前聚合状态。数字小键盘有活动子代理时仍绿色，并保留按所属主会话数决定的Spectrum规则；没有子代理时为蓝色常亮。权限审批仍沿用全橙规则。

询问检测复用每秒一次的rollout增量读取：当前回合的 `response_item/function_call` 中 `name=request_user_input` 开始等待，同 `call_id` 的 `function_call_output` 结束等待。只缓存调用标识，不保存问题或回答正文。新回合、完成、中断会清理等待标记；不同回合不串用。通常在记录写入后的下一个维护周期生效（约1秒），实际延迟受Codex日志落盘影响。无需新增Hook或重启Codex。

2026-09-29：新增多主会话分区Spectrum，主区按运行中主会话数、小键盘按拥有运行中子代理的主会话数分别判断。每个主会话的子代理无论多少，最多计一组；父任务已完成而子代理仍运行时，该组仍计入。

| 运行主会话数 | 活动子代理所属主会话数 | 灯效 |
|---|---|---|
| 1 | 0 | 全蓝常亮 |
| ≥2 | 0 | 全键盘Spectrum |
| ≥2 | 1 | 主区Spectrum，小键盘绿色常亮 |
| ≥2 | ≥2 | 主区Spectrum，小键盘Spectrum |
| 1 | ≥2 | 主区蓝色常亮，小键盘Spectrum |
| 0 | 1 | 主区熄灭，小键盘绿色常亮 |
| 0 | ≥2 | 主区熄灭，小键盘Spectrum |

Spectrum默认4秒一周期，区域内所有键同步沿红→黄→绿→青→蓝→紫→红平滑变色，亮度档保持4/4，RGB最高通道保持127，不再周期性熄灭。审批仍全橙常亮，完成提示和空闲Single Key不参与Spectrum。数量回落到1时自动恢复对应区域常亮。

动画由Rust常驻服务通过Custom逐键模式实现，最多10帧/秒，跳过内容相同的HID包和模式包；不启动Python，不逐帧写状态文件或日志。只有需要Spectrum时启用动画定时器；生命周期核对仍最多每秒一次，空闲仍阻塞等待。软件Spectrum会增加必要的USB通信与CPU唤醒，实际流畅度需要目视确认。`status` 额外返回 `running_parents`、`subagent_parents`、`main_spectrum` 和 `numpad_spectrum`，便于核对触发条件。该功能在Rust生产服务生效，Python回退版本不支持新增Spectrum。

亮灯时使用键盘协议最高亮度档 `4/4`（2026-09-22 从原先 `2/4` 调整），保留各状态 RGB 配色。
2026-09-23：空闲模式改为 `Single Key`（`0x15`），随机颜色开启（`1`），最高亮度（`4`），最慢速度（`4`），由键盘固件处理按键亮起与慢速渐灭。实际渐灭时长取决于固件，未指定秒数。
一个任务完成不会覆盖其他任务的运行或等待状态。中断、会话关闭和已退出的 Codex 进程会清理对应活动状态。
只有 Hook 接入后的事件参与聚合，安装前已经运行的任务不会追溯导入。
目前只覆盖这台机器上加载本用户配置的 Codex；远程主机的任务尚未接入。

服务空闲时阻塞于套接字与设备事件，不再每250ms扫描状态和设备。收到Hook立即更新；存在运行主任务或子代理时每秒执行必要的本地生命周期核对，以补偿缺失启动/停止Hook。完成提示按5秒截止时间唤醒。HID句柄缓存，无关回应在700ms截止时间内跳过；设备事件触发重新发现，写入错误后3秒重试。物理重插仍需实测。
相同状态的工具前后事件不反复写sessions.json，不逐条追加工具事件日志。服务不可用时原生Hook可暂存最多256条精简事件，恢复启动后重放；不保存提示词或工具正文。
服务通过 `systemd --user` 管理，登录后启动，不要求 Codex 使用 root。

```bash
/home/pyl/.codex/codex-keyboard-prototype/bin/codex-keyboard status
systemctl --user status codex-keyboard.service
journalctl --user -u codex-keyboard.service -n 20
```

## 已验证结果

Rust验收：9项单测通过；隔离IPC验证多代理、审批优先、5秒完成提示和空闲恢复；5种灯效全部报文字节与Python一致。生产验证真实CLI回合及本桌面任务multi_agent_v1子代理运行发现与结束恢复，服务记录 running → running_subagents → running，全部状态文件兼容。物理拔插/系统重启未做，不能视为已验证。

2026-09-20，一次真实 Codex CLI 会话（标识已匿名化）：

- 18:20:15，`UserPromptSubmit` → 蓝色 `(0,32,127)`，完整写入 64 字节并收到设备响应。
- 18:20:17，`Stop` → 绿色 `(0,127,16)`，完整写入 64 字节并收到设备响应。
- Codex 返回“键盘通路验证完成。”并正常结束。
- 2026-09-22，用户目视确认键盘从蓝色变成绿色，且当前保持绿色。真实 Hook 事件、USB 响应与实际灯光一致，最小自动通路验证通过。

绿色表示回合结束，不表示任务质量验收通过。

2026-09-22 日常接入阶段：多会话优先级、完成超时、过期回合隔离、进程退出清理与 SessionEnd 保留绿色提示的程序检查通过；多会话模拟经过实际后台服务输出蓝色、橙色。真实审批弹窗和桌面端新回合仍需单独确认。

用户级 `/home/pyl/.codex/hooks.json` 已配置 7 个事件，桌面端 app-server 已返回全部为受信任状态。`config.toml` 语义比较确认只增加 7 项对应信任记录，原有设置保持不变。安装前备份为 `/home/pyl/.codex/config.toml.bak.20260922T134134`。

2026-09-24：新增 `SubagentStart`、`SubagentStop`，目前共 9 个已信任事件；原有 Hook 与其他配置核对保持。硬件分区测试发送 7 个逐键颜色包及 1 个 Custom 模式包，8 个响应均与请求一致；用户目视确认数字小键盘已为绿色。对应 17 个矩阵位置包括 Num Lock、数字、运算符、小数点及小键盘 Enter。亮度仍为 4/4。

真实子代理验证：一次会话（标识已匿名化）中，同一子代理执行两轮：22:26:32 首次启动变为 `running_subagents`，22:27:49 结束恢复 `running`；再次执行于22:28:00变为 `running_subagents`，22:28:05结束恢复 `running`。两轮正常完成，无 HID 错误。

已知边界：本机版本再次唤醒子代理时不重复发送 SubagentStart。已用带 agent_id 的工具事件恢复活动标记，并尝试对可唯一关联的任务名处理后续提交；本次实测再次提交于22:27:56，但实际变绿在22:28:00首次子代理工具事件，说明再次唤醒的纯思考阶段尚不能保证即时显示。并发启动导致名称关联不唯一时也不猜测映射。此处不能宣称所有恢复路径无延迟。

2026-09-24 23:11 修复绿色残留：两名真实桌面子代理分别在23:07:08和23:07:43记录 task_complete，但未收到 SubagentStop。新增 agent_completion.py，每秒只读核对活动子代理的本地回合事件，按文件偏移增量读取，只使用 task_started/task_complete 标识与时间，不保存正文。仅匹配当前回合且不早于活动标记的完成证据才移除，缺文件或读取失败不推断结束。实际残留状态重放及4项状态测试通过，服务已恢复running并收到蓝色报文响应。本地 state_5.sqlite 与 rollout 属内部格式，后续 Codex 升级若改变格式需适配；这不是稳定官方状态订阅接口。

三个纯状态测试覆盖多代理集合、父子回合隔离、橙色优先、重复事件、父任务先结束、恢复与旧Stop隔离。运行：`python3 -B /home/pyl/.codex/codex-keyboard-prototype/test_state_bridge.py`。
`hook-events.jsonl` 只记录事件名及会话、回合、子代理标识等诊断字段，不记录提示词或工具正文，超过256 KiB轮换。

实际安装后验证（均未跳过 Hook 信任）：

- PATH CLI `0.153.4`：13:41:59 蓝色，13:42:05 绿色，13:42:10 熄灯。
- 桌面内核 `/usr/lib/chatgpt/resources/codex`，`0.155.0-alpha.9.2`：13:42:41 蓝色，13:42:48 绿色，13:42:53 熄灯。
- 两个回合均正常返回“键盘通路验证完成。”，后台服务无 HID 错误。

桌面内核已通过独立回合验证。安装后的原桌面会话未即时接入；2026-09-22 用户完全重启 Codex 并再次发送“测试灯光”后，13:47:41 后台服务记录到真实桌面会话进入 running。状态文件确认当前会话（标识已匿名化）的 PreToolUse 事件来自新桌面进程。用户随后目视确认“蓝色 → 绿色约 5 秒 → 熄灯”，桌面端完整通路验收通过。

## 使用

运行一次真实 Codex 验证（消耗一次模型请求）：

```bash
python3 /home/pyl/.codex/codex-keyboard-prototype/verify_codex.py
```

验证器使用已安装并精确信任的用户级 Hook，继承现有登录与模型配置，不跳过 Hook 信任。
优先使用桌面端内核，缺少桌面端时使用 PATH 中的 Codex。

安装或重新核对 Hook 信任：

```bash
python3 /home/pyl/.codex/codex-keyboard-prototype/install_hooks.py
```

安装器保留已有 Hook，通过 app-server 返回的真实定义标识进行信任配置，不自行生成散列值。运行脚本会在必要时更新用户级配置并创建备份。

手动控制示例：

```bash
python3 /home/pyl/.codex/codex-keyboard-prototype/keyboard.py set running
python3 /home/pyl/.codex/codex-keyboard-prototype/keyboard.py set done
python3 /home/pyl/.codex/codex-keyboard-prototype/keyboard.py set waiting
python3 /home/pyl/.codex/codex-keyboard-prototype/keyboard.py set idle
```

分别是蓝、绿、橙、Single Key 随机颜色。手动测试前应先停止后台服务，避免自动状态覆盖手动灯光。灯效可能保留在键盘中；未实现原灯效读取与恢复。

设备权限由 `/etc/udev/rules.d/70-codex-cherry.rules` 持久配置，仅把 `046a:0079` 的 USB 接口 `01` 对应 HID 设备授权给 `pyl`。本次设备节点为 `/dev/hidraw5`，脚本自动识别，不固定编号。规则已重新加载并实际生效，物理拔插和重启后的行为尚未实测。

`sessions.json` 只保存会话/回合标识、事件、状态、时间及进程标识，不保存提示词或工具内容。灯光写入响应保存在 `events.jsonl`，超过 256 KiB 后轮换，最多保留一个旧文件。Hook 出错仅打印诊断，不阻止 Codex 工作。

## 暂停自动切换，保持 Single Key 灯效

```bash
systemctl --user disable --now codex-keyboard.service
python3 /home/pyl/.codex/codex-keyboard-prototype/keyboard.py set idle
```

暂停服务后 Hook 仍会更新本地状态文件，但不控制键盘。恢复时使用 `systemctl --user enable --now codex-keyboard.service`。

报文实现依据 OpenRGB 的 CHERRY 控制器，脚本遵循 GPL-2.0-or-later。

2026-09-25 新增 `subagents_only`：父任务进入 done 或中断后仍有活动子代理时，Custom 逐键模式将主键区写为黑色、数字小键盘写为绿色；没有子代理时保持原有 done/idle 流程。状态单测和 Rust/Python 逐字节报文对齐测试通过，生产二进制已更新。

2026-09-29 呼吸验证：17项Rust测试通过。一次隔离实机序列依次覆盖全蓝呼吸、蓝呼吸绿常亮、蓝绿同时呼吸、蓝常亮绿呼吸、主区熄灭绿呼吸，每阶段约4秒；全部收到HID匹配响应，无设备错误。约21.6秒短样本CPU平均约单核0.09%，不代表所有负载下的固定值。测试状态与临时文件已清理，正式服务已切换新版；目视效果待用户反馈。

2026-09-29 修复计划模式询问：真实rollout记录确认 `request_user_input` 是 `response_item/function_call`，回答是同call_id的 `function_call_output`，并非 `PermissionRequest` Hook。Rust服务现按当前回合增量识别：等待时主键区橙色常亮；小键盘继续保持蓝色或绿色及其呼吸规则；回答、新回合、完成或中断后清除。19项Rust测试通过，使用真实记录结构做隔离回放验证 `input_waiting_subagents → running_subagents`，两阶段均收到键盘响应。

2026-09-29：所有多会话呼吸效果替换为分区 Spectrum 色环循环，计数阈值与橙色优先级不变。状态查询和新日志字段改为 `main_spectrum`、`numpad_spectrum`；历史呼吸验证记录仅描述旧版本。
