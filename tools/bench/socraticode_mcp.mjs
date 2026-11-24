// Minimal MCP stdio client for SocratiCode, used for baseline measurements.
// Usage: node socraticode_mcp.mjs <projectPath> <query...>
// Prints one JSON object per line: initialize result, tools/list names, then the
// codebase_search text result.
import { spawn } from "node:child_process";
import readline from "node:readline";

const serverEntry = process.env.SOCRATICODE_ENTRY;
if (!serverEntry) {
  console.error("SOCRATICODE_ENTRY must point at SocratiCode dist/index.js");
  process.exit(2);
}
const [projectPath, ...queryParts] = process.argv.slice(2);
const query = queryParts.join(" ");
if (!projectPath || !query) {
  console.error("usage: node socraticode_mcp.mjs <projectPath> <query...>");
  process.exit(2);
}

const child = spawn(process.execPath, [serverEntry], {
  cwd: projectPath,
  env: {
    ...process.env,
    SOCRATICODE_AUTO_RESUME: "off",
    SOCRATICODE_WATCHER: "off",
  },
  stdio: ["pipe", "pipe", "pipe"],
});

let nextId = 1;
const pending = new Map();
const rl = readline.createInterface({ input: child.stdout });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(JSON.stringify(msg.error)));
    else resolve(msg.result);
  }
});

child.stderr.on("data", (d) => process.stderr.write(d));

function request(method, params) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
}
function notify(method, params) {
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
}

const timeout = setTimeout(() => {
  console.error("timeout waiting for MCP response");
  child.kill();
  process.exit(3);
}, 120000);

try {
  const init = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "codelore-bench", version: "1.0.0" },
  });
  console.log(JSON.stringify({ kind: "initialize", ...init }));
  notify("notifications/initialized", {});

  const tools = await request("tools/list", {});
  console.log(JSON.stringify({
    kind: "tools",
    names: (tools.tools || []).map((t) => t.name),
  }));

  const res = await request("tools/call", {
    name: "codebase_search",
    arguments: { query, limit: 10 },
  });
  const text = (res.content || []).filter((c) => c.type === "text").map((c) => c.text).join("\n");
  console.log(JSON.stringify({ kind: "search", query, text }));
} catch (err) {
  console.error("ERROR", err.message);
  process.exitCode = 1;
} finally {
  clearTimeout(timeout);
  child.stdin.end();
  setTimeout(() => child.kill(), 500);
}
