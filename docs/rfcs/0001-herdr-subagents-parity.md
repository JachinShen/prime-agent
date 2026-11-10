# RFC-0001：原生 Agents 界面差分证据

2026-11-09 在 macOS 上执行。只读 TS reference 为 `/Users/jachinshen/prime-agent`，
`v0.9.8` 对应 commit `7d442aafa985f9342134fac16c2ef41f03fb45c1`；执行前后其 tracked working tree 均为空。
Rust 基线为 `a860d4520aa945d59c77812642251747c149dd70`，叠加 RFC-0001 当前工作树。

## 比较方法及结果

共享 fixture：[agents-view-native-parity.json](../../scripts/fixtures/agents-view-native-parity.json)。
TS adapter 导入生产 `AgentsViewMode` 的行渲染、open、reply arm/submit 方法，以及生产 roster 和 receipt 函数；
Rust 测试调用对应的生产方法。测试替换 UI/I/O 边界，使用固定时钟，不启动模型或修改真实会话。
原始输出与结果保存在 [0001-native-agents-parity.json](evidence/0001-native-agents-parity.json)。

| 范围 | 结果 |
| --- | --- |
| idle、running、saved 原生行，48/56 列，包含中文标题 | 六个行文本相同；仅忽略行尾空格与 ANSI 样式。两种宽度的完整列标题相同。 |
| 四个原生行的身份、roster ID、running/idle/inactive 分类 | 四项完整对象相同。 |
| live/working 行 open 和 reply、saved 行 resume 和 reply、原生 child open 和 reply guard | 四项完整动作对象相同，包括目标、ancestor、reply key 和提交内容。原生 nested child 仍不能直接 arm reply。 |
| native `agent_message.send` 回执目标 | Rust 真实 host handler 的 target 与 TS 生产 receipt 的 target，经键排序后 JSON UTF-8 字节相同；保留真实 activeSessionId/sessionId/runtimeKind。 |

回执比较只覆盖本次修改涉及的 `target` 对象，不声明整个回执 envelope 或 daemon 网络流量逐字节相同。
reply 比较覆盖生产 arm/submit 的选择和待发送内容，最终发送 I/O 使用测试边界；外部子会话的真实 daemon 路由另由 Unix socket 回归测试覆盖。

## 已有差异及边界

原生 child 的缩进已有差异：Rust 每层四格，TS 每层两格；两种宽度下都保留原始差分。
Rust 基线 `render.rs::depth_indent` 已是 `" ".repeat(4 * depth)`，本次未修改。
验证器只接受这一项精确差异，其他渲染差异会失败。

本次没有进行全帧样式、宽屏布局、真实终端按键时序或鼠标差分；已有具名标题样式、宽屏 Activity 列等差异不在此次行文本验证范围内。
因此这份证据证明上述原生分支的保留行为，不声明完整 TUI 与 TS 全面一致。
新增 Herdr backend 在 TS v0.9.8 中没有对应实现，其同 workspace pane、完成唤醒和追问闭环由独立 live verifier 验证，不能计作 TS parity。

## 重跑

```sh
python3 scripts/verify_agents_view_native_parity.py --ts-reference /Users/jachinshen/prime-agent
```

默认会构建并执行两个 focused Rust 测试，再通过 Bun 运行只读 TS adapter。
已有测试二进制时可传入 `--tui-test-binary` 和 `--core-test-binary`，避免占用 Cargo 构建锁；
`--output-dir` 保存本轮原始输出。此次使用已构建的 `pa_tui-1e93ca039b8c10ea` 和 `pa_core-d51188bf741643ea` 执行，两项测试均通过。

额外执行 `agents_view::tests::external::hanging_external_focus_never_delays_the_next_frame`，1/1 通过：
真实 Unix socket 收到 focus 命令后永不回复，后台 request 仍 pending 时下一帧成功渲染。
测试通过 readiness channel 等待可观察状态，不使用固定 sleep。
