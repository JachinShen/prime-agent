# RFC-0001：实现与验证记录

2026-11-09，macOS aarch64。实现位于 `personal-release` 工作树，基线为
`a860d4520aa945d59c77812642251747c149dd70`。只修改 Prime Agent；不修改 Herdr，
不安装或替换全局 Prime Agent，也不重启用户正在使用的 daemon。

## 真实产品路径

执行：

```sh
cargo build -p pa-daemon -p pa-cli
python3 scripts/verify_herdr_subagents.py --run-live \
  --kernel-python ~/.prime/agent/kernel-venv-py314-personal/bin/python
```

Herdr 0.9.5 / protocol 22、Codex 0.166.0、Python 3.14.4。
父会话使用可重复的 scripted provider，但运行真实 daemon worker 和持久 Python kernel；
子会话是 Herdr 启动的真实交互 Codex，使用 workspace-write sandbox 和 never approval。
临时目录的 trust 配置只通过该子进程参数设置，不修改用户 Codex 配置。

验证通过，原始 artifacts：`/private/tmp/pa-herdr-live-zzv50nye`。
持久证据：[0001-herdr-live.json](evidence/0001-herdr-live.json)，其中终端文本明确省略，完整捕获保留在上述本地目录。

| 检查 | 结果 |
| --- | --- |
| 同 workspace 创建相邻 pane，保持 cwd 和焦点 | `wW:p2` 位于测试父 pane 的 `wW` workspace，cwd 正确；创建与 spawn 后焦点均未改变。 |
| 第一轮后台完成、父会话自动唤醒 | 无额外 prompt；父会话执行 collect 并回复 acknowledgement。结果为 done/settled，文件内容与唯一测试标记相同。 |
| 原生 roster | 外部行 backend=herdr、runtimeResident=true、status=idle，没有虚构 child activeSessionId/sessionId。 |
| 原会话追问 | `agent_message.send` 路由到逻辑 child；第二轮复用 pane/terminal/agent name，读取独立 turns/002/answer.md。 |
| 第二次自动唤醒和通知去重 | 两轮各一条完成通知，轮次分别为 1 和 2。 |
| 删除与资源清理 | child 从 roster 消失；关闭测试 workspace、停止隔离 daemon，cleanup_errors 为空。 |

读取 Herdr socket 响应时使用实际 wire 名称 `recent_unwrapped`；pending 启动允许省略
agent kind 和 false readiness 标志。启动成功后只观察 readiness，不重复启动。
工作、blocked、unknown 时读取 visible，idle/done 时读取 recent_unwrapped。

## 自动化检查

### Subagents 底栏与列表实机验证（2026-11-10）

用户保留的 `Prime Herdr Playground` 现场中，真实 Codex `demo-review` 已完成且 pane
仍存活。此前底栏只显示 running 数量，实际 roster 的外部行又没有 Prime-session
`lifecycle` 字段，导致底栏显示 0、列表漏掉该 child；直接启动的聊天前端进入
Subagents 列表时还丢失了 scoped view 参数。

底栏现在统计 running + idle 的存活后代，inactive 仍在展开明细中；列表按 Herdr
逻辑运行时识别 child，不要求不存在的 Prime 会话生命周期。CLI 传递原始导航 scope。
原生会话的 draft 筛选保持原有行为，没有给外部 child 伪造 Prime session。

真实 Herdr 操作先重现问题，再仅退出并恢复 `w2D:p1` 的前端：底栏显示
`◆ 1 subagent`，Down 展开显示 `0 running · 1 idle · 0 inactive`，Enter 打开
`Herdr Playground › subagents`，其中显示 `[codex] demo-review`。原 parent worker
PID 61164、active session、独立 daemon、Python kernel 和 `w2D:p2` Codex 均保留。
列表保持打开供用户试用，完整终端证据在 `/private/tmp/prime-agent-subagent-dock-live`。

回归测试分别在修复前重现 0 计数及实际无 lifecycle 的外部行丢失。相关测试共 57 项
通过（chrome 27、后代计数 5、external UI 8、列表数据层 16、CLI scope 1），debug
编译、fmt 和 whitespace 检查通过。scoped clippy 的 lib 检查通过，命令行显式放行
已有的 unknown-lints、duration_suboptimal_units、cast_possible_wrap、collapsible_match；
未修改仓库 lint 配置，不代表完整 workspace 零警告门禁通过。

TS 原生列表差分复验通过（6 个顶层渲染行、4 个动作、4 个 roster 对象及实际消息
回执目标），证据在 `/private/tmp/prime-agent-subagent-dock-native-parity`。原有缩进
差异继续记录；TS 没有该多组 dock 或 Herdr 后端，底栏的 running + idle 统计按本次
用户要求实现。没有新增 public API、依赖或采用事件；沿用现有 Subagents 打开事件。
持久验证记录见 [0001-herdr-subagent-dock.json](evidence/0001-herdr-subagent-dock.json)。

### Herdr 实机复验（2026-11-10）

使用当前重新构建的 debug daemon，分别执行：

```sh
python3 scripts/verify_herdr_subagents.py --run-live \
  --kernel-python ~/.prime/agent/kernel-venv-py314-personal/bin/python
python3 scripts/verify_herdr_subagents.py --run-live --worktree \
  --kernel-python ~/.prime/agent/kernel-venv-py314-personal/bin/python
```

两次全部通过。Herdr 0.9.5 实际启动交互 Codex 0.166.0；父会话使用 scripted provider，
但 daemon worker、Python kernel、外部 Codex、文件修改与完成通知均走实际产品路径。

| 模式 | 实际子 pane | 两轮耗时 | 结果 |
| --- | --- | --- | --- |
| 同 workspace 分屏 | `w2A:p2` | 36.5s / 13.7s | 两轮分别自动唤醒父会话，追问复用 pane/terminal，每轮 collect 与 answer.md 内容一致。 |
| Worktree | `w2C:p1`，父 workspace 为 `w2B` | 35.9s / 12.2s | 两轮修改隔离 checkout，父文件保持原样；两次自动唤醒、追问复用及删除后保留未提交修改均通过。 |

两种模式的原生 roster 和逻辑 child 消息回执正确，每轮恰好一条完成通知。
普通模式期间观察到 IC workspace 内部焦点切换，未切入测试 workspace；该变化保留在证据中。
清理错误均为空，事后重新查询确认 `w2A` / `w2B` / `w2C` 已关闭，worktree 已由测试显式
移除，两个测试 daemon endpoint 均拒绝连接。临时目录保留 artifacts，包含停止监听后残留的
socket 文件；清理检查以实际连接结果验证服务退出。

持久证据：[0001-herdr-live-recheck.json](evidence/0001-herdr-live-recheck.json)，包含当前二进制
SHA-256 和两次运行记录。完整本地 artifacts 分别为 `/private/tmp/pa-herdr-live-efkq5gvd`
及 `/private/tmp/pa-herdr-live-ke5hoprv`。未改动全局安装或用户 daemon。

### 系统提示词实际装配验证（2026-11-10）

重新构建 debug 的 prime-agent / pa-daemon，并在隔离的 agent 目录和 cwd 中执行
`target/debug/prime-agent prompt --json`。实际输出包含 agent/agent_args、worktree/base、
完成自动唤醒和逐轮 answer.md 的说明。提取其中 7 个子 agent 接口的签名，与 Python
模块和 kernel namespace 两个 facade 比较参数名、位置限制、默认值及 async 属性，
14 次比较全部通过。完整输出与检查结果保存在
`/private/tmp/pa-subagent-prompt-verified-g_mfj57b`。

检查发现 `subagent_read` 的文档遗漏 keyword-only 的 `*`，且控制接口 bullet 未被
现有 tool-surface guard 识别；均已修正。新增的动态签名回归在修正前对两个 facade
失败，修正后通过。此次 Python external-subagent 测试 10 项、Rust prompt guards
7 项、pa-core `--lib external` 筛选的 12 项全部通过。构建仍有既有 vendor/crossterm
警告，不代表完整 workspace 零警告门禁通过。未替换全局版本。

### Worktree 扩展（2026-11-10）

原生 spawn 的两个 Python facade 支持 `worktree=True` / 分支名及 `base`。
Herdr 在独立 workspace 启动 checkout，普通 pane 创建方式不变。返回的 worktree
metadata 持久化并进入 list/collect；名册 cwd 使用 checkout 路径。旧 runtime capability
probe 会明确拒绝没有 worktree/base 的 facade。新共享请求与 checkout 类型归 pa-types，
校验归 pa-core，创建和持久化归 pa-daemon；没有新依赖或反向依赖。

真实产品验证：

```sh
cargo build -p pa-daemon -p pa-cli
python3 scripts/verify_herdr_subagents.py --run-live --worktree \
  --kernel-python ~/.prime/agent/kernel-venv-py314-personal/bin/python
```

通过。原始 artifacts 为 `/private/tmp/pa-herdr-live-ga4f_fzj`，持久证据见
[0001-herdr-worktree.json](evidence/0001-herdr-worktree.json)。Codex 两轮修改同一个
worktree 中的文件，父 checkout 内容保持不变；两轮各自动唤醒父 agent 一次，collect
读取对应 answer.md，追问复用同一 pane/terminal。删除子任务后，未提交修改仍存在。
全程保持用户焦点，测试末尾显式清理测试专属 checkout 和 workspace，cleanup_errors 为空。

Python 全套在包含声明依赖的隔离 Python 3.14 环境中运行：761 passed、104 subtests passed。
系统 Python 缺少 dill 等依赖的初次运行失败，没有以该结果替代完整检查。
Rust focused tests 覆盖 worktree 参数与无效参数、创建错误不退回父 checkout、创建结果身份
校验、manifest 读写和损坏拒绝、结果与名册元数据、关闭保留修改，以及既有外部生命周期。
检查期间修正了 fake Herdr 的 subscription ack 锁：其不能持有 stale-observer 回归刻意
冻结的 agent-response 锁，避免测试夹具互锁。

原生 UI/消息路径 TS 差分重新通过，证据目录
`/private/tmp/prime-agent-worktree-native-parity`；原有 Rust4/TS2 的 child 缩进差异仍显式记录。
TS 没有 Herdr worktree API，本扩展的行为由真实 Herdr/Codex verifier 覆盖。
构建、fmt、diff whitespace 和 scoped clippy 通过；clippy 命令沿用下文列出的既有
toolchain/lint 放行，不代表完整 workspace merge gates 已全部通过。

### Herdr 会话状态回归（2026-11-09）

IC 新 pane 的前端连接到仍绑定旧 pane 的存活 worker，随后原新会话退出并释放 pane，
导致侧栏丢失 Prime Agent。修复后，携带 Herdr env 的最新交互 attach 更新绑定；
该客户端 detach/disconnect 后旧 reporter 静默停止，observer 不改变绑定。
延迟退出的 worker 使用自己最后一个已确认 report 预留的 seq 释放，不能清掉更新的上报。
传输验证完整、匹配 id 的 JSON acknowledgement，拒绝、截断和损坏响应会记录明确错误。
状态发送仍全部在后台，不在前端 attach 或绘制路径等待 socket。

验证：新增回归测试在修复前因新 pane 收不到旧会话上报而失败，修复后 Herdr e2e 3 项、
reporter 单元测试 22 项通过，首帧挂起 sink 回归也通过。涉及的 lib 和 Herdr e2e clippy
通过，命令行放行与下文记录相同的既有 lint，未加入新的仓库 allow。
debug 的 prime-agent / pa-daemon 构建、cargo fmt --all --check 和 git diff --check 通过。

真实 Herdr 0.9.5 验证使用临时相邻 pane、真实 Prime Agent TUI、隔离 daemon 和 faux turn，
通过会话列表恢复关闭 pane 的旧会话，验证两种旧 worker 退出均不清掉新状态，
恢复后 turn 正确 settled。实际 socket 请求与 Herdr 回复均捕获，资源清理无错误。
证据见 [0001-herdr-reporting.json](evidence/0001-herdr-reporting.json)，完整本地 artifacts 为
`/private/tmp/pa-herdr-resume-8su_u91d`。

TS v0.9.8 extension hook 对照验证 idle → working → idle 与 replacement 静默关闭；
共用生命周期字段在规范化时间、pane/session 引用后逐字节相同。Rust 已有的 resume_argv
附加字段保留。本次有意修正 attach 所有权、release seq 和 acknowledgement 处理，
没有新的 Prime Agent 绘制改动。所有实现保持 pa-daemon 私有范围，无新依赖或 public API。

| 检查 | 结果 |
| --- | --- |
| Python runtime 全套，PYTHONPATH 指向本仓库 | 759 passed。 |
| 五个 crate 的 `--lib external` focused tests | 58 passed：core 11、daemon 37、telemetry 1、TUI 7、types 2。 |
| 关键失败分支 | 覆盖 stale idle、blocked 保持 pending、工作中读取、断线恢复不重发、错误/空答复、截断、损坏 manifest、替换身份、持久化失败、通知去重与恢复。 |
| TUI 挂起 focus socket | 收到请求后永不回复，下一帧仍可绘制；使用 observable readiness。 |
| 原生 TS 差分 | 六个顶层行、四个动作、四个 roster 对象和真实 host receipt target 比较通过；已有 child 缩进差异单独记录。见 [差分证据](0001-herdr-subagents-parity.md)。 |
| 格式与 whitespace | cargo fmt --all --check、git diff --check 通过。 |
| 编译 | debug 的 pa-daemon / prime-agent，以及 cargo build --release --workspace 均通过。 |
| 本次 crates 的 lib/bin clippy | 显式放行下述已存在的 lint 后通过；未在仓库加入 allow 或修改无关代码。 |

Focused 命令：

```sh
cargo test -p pa-daemon -p pa-core -p pa-tui -p pa-types -p pa-telemetry --lib external
PYTHONPATH=prime-agent-runtime/src \
  ~/.prime/agent/kernel-venv-py314-personal/bin/python -m unittest discover -s prime-agent-runtime/test
```

## 全量门禁的限制

`make check` 未通过，不能据此声明可直接 merge。当前 Rust 1.95 工具链和 macOS 暴露了已有问题：

- clippy 配置中的 `manual_assert_eq` 未被当前工具链识别；已有 duration_suboptimal_units、
  cast_possible_wrap、collapsible_match 和集成测试 needless_raw_string_hashes 也触发 -D warnings。
  本次 lib/bin 检查的命令行仅放行前三种已有代码 lint 和 unknown-lints。
- `cargo test --workspace` 在 pa-cli 的 interactive_daemon_e2e 编译失败：Linux 专属
  libc::prctl / PR_SET_PDEATHSIG 没有 macOS cfg。已核对基线存在相同代码。
- 分开运行 lib/bin：pa-agent 12、pa-ai 370、pa-cli 299 通过；pa-core 1254 通过，
  两个临时路径 canonicalization 测试在 macOS 的 /var 与 /private/var 差异下失败，
  设置 TMPDIR=/private/tmp 后两项单独重跑通过。
- daemon 全套 1209 通过；一个旧测试硬编码 Python 3.11 kernel，缺少当前 runtime；
  另一个 compaction 测试预期一次、实际五次。在未修改基线的独立 archive checkout
  重跑同一 compaction 测试也得到相同 (5, 5) 失败。
- 其余库中 models 50、telemetry 65 通过，TUI 1546 通过、8 失败、1 ignored；
  失败均为 macOS Option 与测试期望 Alt 的既有快捷键文案差异。

详细日志保留在 `/tmp/prime-agent-external-*.log`，真实 verifier 日志为
`/tmp/prime-agent-herdr-live-7.log`。这些限制与新增后端的 focused/live verifier 分别报告。

## 所有权与兼容性

新增 public/wire surface：pa-types 的外部 child 引用与控制、pa-core 的 host 契约、Python facade。
Herdr transport、状态机和持久化保持在 pa-daemon 的私有模块；TUI 只使用 wire 类型和 daemon 请求。
crate README 已更新，无第三方依赖变更、Cargo.lock 变更或跨 crate re-export。
采用事件只包含受约束 kind、outcome、duration 和 truncation 等原语，后台发送。

使用新功能需要运行本次构建的 daemon 和配套 Python runtime；runtime capability probe
会拒绝缺少 agent 参数或 controls 的旧 runtime。同一 kernel 已导入的旧 Python 模块需随 kernel 重启更新。
外部 agent 的权限必须允许写入本轮 answer.md；权限或文件读取失败会明确报告。
