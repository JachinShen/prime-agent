// Test-only adapter around the read-only TS production view; no TS source is changed.
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const [reference, fixturePath, output] = process.argv.slice(2);
if (!reference || !fixturePath || !output) throw new Error("reference fixture output required");
const load = (path: string) => import(pathToFileURL(resolve(reference, path)).href);
const { AgentsViewMode, buildCompactAgentsViewLayout } = await load("packages/coding-agent/src/modes/agents-view/agents-view-mode.ts");
const { getAgentsViewSummaryIdentity } = await load("packages/coding-agent/src/modes/agents-view/agents-view-state.ts");
const { classifySessionRosterStatus, rosterAgentIdForSummary } = await load("packages/coding-agent/src/modes/daemon/agent-roster.ts");
const { initTheme } = await load("packages/coding-agent/src/modes/interactive/theme/theme.ts");
const { createAgentSessionMessageReceipt } = await load("packages/coding-agent/src/core/agent-messages.ts");
initTheme("dark", false);
Date.now = () => Date.parse("2024-01-01T00:00:01.000Z");
const fixture = JSON.parse(readFileSync(fixturePath, "utf8"));
const rows = fixture.rows.map((row: any) => ({ ...row, selectable: true, recursiveCost: row.cost,
    runningSubagentCount: 0, descendantCount: 0, subtitle: "", statusLabel: "" }));
const mode: any = Object.create(AgentsViewMode.prototype);
let text = "";
let opened: any;
Object.assign(mode, { rows, selectedIndex: 0, workingIconFrame: 0, unifiedRecords: [],
    unifiedIndex: undefined, persistentState: {}, ui: { requestRender() {} },
    editor: { getText: () => text, setText(value: string) { text = value; }, setPlaceholder() {} },
    finish(value: any) { opened = value; },
    async getLastAssistantText() { return "Last response"; },
});
const clean = (value: string) => value.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "");
const rendered = fixture.widths.map((width: number) => {
    const layout = buildCompactAgentsViewLayout(rows, width);
    return { width, legend: clean(layout.legend), rows: rows.map((row: any) =>
        clean(mode.finalizeRenderedLine(mode.renderRow(row, width, layout), width))) };
});
const actions = [];
for (let index = 0; index < rows.length; index++) {
    mode.selectedIndex = index;
    opened = undefined;
    mode.openSelected();
    if (!opened || opened.type !== "open") throw new Error("Expected production open result");
    const open = { selection: opened.summary.activeSessionId ? "attach" : "resume",
        target: opened.summary.activeSessionId ?? opened.summary.sessionFile,
        ancestors: opened.expandedAncestorSessionIds ?? [] };
    mode.replyTarget = undefined;
    await mode.toggleReplyTarget();
    const target = mode.replyTarget;
    let reply = null;
    if (target) {
        // Keep the real submit's arm/current-target logic; intercept only the final I/O boundary.
        mode.sendReply = async (selected: any, message: string) => {
            reply = { key: selected.key, text: message, activeSessionId: selected.summary.activeSessionId ?? null,
                sessionFile: selected.summary.sessionFile };
            return false;
        };
        await mode.submit("Follow up");
    }
    actions.push({ identity: rows[index].identity, open, replyArmed: !!target,
        replyKey: target?.key ?? null, reply });
}
const roster = rows.map((row: any) => ({ identity: getAgentsViewSummaryIdentity(row.summary),
    agentId: rosterAgentIdForSummary(row.summary), status: classifySessionRosterStatus(row.summary) }));
const nativeReceiptTarget = createAgentSessionMessageReceipt({ id: "agentmsg_fixture", source: "agent_message",
    target: { activeSessionId: "sib-1", sessionId: "sib-1-session", runtimeKind: "top-level" },
    message: "proceed" }, "delivered", "2024-01-01T00:00:00.000Z").target;
writeFileSync(output, JSON.stringify({ rendered, actions, roster, nativeReceiptTarget }, null, 2) + "\n");
