// Baseline runner: SocratiCode MCP vs. CodeLore Atlas (text) on the fixed
// RogueDOD / UE control queries. Writes JSON + Markdown into docs/bench/.
//
// Usage (from the CodeLore worktree root):
//   Set SOCRATICODE_ENTRY, LORE_CACHE, ROGUEDOD_ROOT and UE_SOURCE_ROOT (see README.md).
//   node tools/bench/run_baseline.mjs [--artifact docs/bench/roguedod-baseline.json]
import { spawn, spawnSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import readline from "node:readline";
import os from "node:os";

const ROOT = resolve(import.meta.dirname, "../..");
const QUERIES_PATH = resolve(ROOT, "tools/bench/roguedod_queries.json");
const LORE_BIN = process.env.LORE_BIN || resolve(ROOT, process.platform === "win32" ? "target/debug/lore.exe" : "target/debug/lore");
const SOCRATICODE_ENTRY = process.env.SOCRATICODE_ENTRY ? resolve(process.env.SOCRATICODE_ENTRY) : null;
const ATLAS_CACHE = process.env.LORE_CACHE ? resolve(process.env.LORE_CACHE) : null;
const ROGUEDOD_ROOT = process.env.ROGUEDOD_ROOT;
const UE_SOURCE_ROOT = process.env.UE_SOURCE_ROOT;
const REPS = Number(process.env.BENCH_REPS || 3);

const argArtifact = process.argv.indexOf("--artifact");
const ARTIFACT = resolve(ROOT, argArtifact > 0 ? process.argv[argArtifact + 1] : "docs/bench/roguedod-baseline.json");

if (!SOCRATICODE_ENTRY) {
  console.error("set SOCRATICODE_ENTRY to the SocratiCode dist/index.js");
  process.exit(2);
}
if (!ATLAS_CACHE) {
  console.error("set LORE_CACHE to the private copy of the shared atlas cache");
  process.exit(2);
}
if (!ROGUEDOD_ROOT || !UE_SOURCE_ROOT) {
  console.error("set ROGUEDOD_ROOT to the RogueDOD checkout and UE_SOURCE_ROOT to the Engine/Source directory");
  process.exit(2);
}

const spec = JSON.parse(readFileSync(QUERIES_PATH, "utf8"));
spec.corpora.roguedod.atlas_root = resolve(ROGUEDOD_ROOT);
spec.corpora.roguedod.socraticode_path = resolve(ROGUEDOD_ROOT, "RogueDOD");
spec.corpora.ue.socraticode_path = resolve(UE_SOURCE_ROOT);
let coverage = null;
try {
  coverage = JSON.parse(readFileSync(resolve(ROOT, "tools/bench/coverage.json"), "utf8"));
} catch {}
const norm = (p, prefix = "") => {
  let s = String(p).replace(/\\/g, "/");
  if (prefix && s.toLowerCase().startsWith(prefix.toLowerCase())) s = s.slice(prefix.length);
  return s.replace(/^\/+/, "");
};
const containsLine = (r, line) => Number.isFinite(line) && r.start <= line && line <= r.end;

class SocraticodeSession {
  constructor(entry, cwd) {
    this.entry = entry;
    this.cwd = cwd;
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = "";
  }
  async start() {
    this.child = spawn(process.execPath, [this.entry], {
      cwd: this.cwd,
      env: { ...process.env, SOCRATICODE_AUTO_RESUME: "off", SOCRATICODE_WATCHER: "off" },
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child.stderr.on("data", (d) => {
      if (this.stderr.length < 20000) this.stderr += d.toString();
    });
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.rl.on("line", (line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve: ok, reject: bad } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? bad(new Error(JSON.stringify(msg.error))) : ok(msg.result);
      }
    });
    const init = await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "codelore-baseline", version: "1.0.0" },
    });
    this.version = init?.serverInfo?.version || "unknown";
    this.notify("notifications/initialized", {});
    const tools = await this.request("tools/list", {});
    this.tools = (tools.tools || []).map((t) => t.name);
    if (!this.tools.includes("codebase_search")) throw new Error("codebase_search tool missing");
  }
  request(method, params, timeoutMs = 120000) {
    const id = this.nextId++;
    return new Promise((ok, bad) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        bad(new Error(`timeout: ${method}`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer);
          ok(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          bad(e);
        },
      });
      this.child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  }
  notify(method, params) {
    this.child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  }
  async search(query, limit = 10) {
    const t0 = performance.now();
    const res = await this.request("tools/call", { name: "codebase_search", arguments: { query, limit } });
    const ms = performance.now() - t0;
    const text = (res.content || []).filter((c) => c.type === "text").map((c) => c.text).join("\n");
    return { ms, text };
  }
  close() {
    try {
      this.child.stdin.end();
    } catch {}
    setTimeout(() => {
      try {
        this.child.kill();
      } catch {}
    }, 300);
  }
}

function parseSocraticode(text) {
  const header = /^--- (.+?) \(lines (\d+)-(\d+)\) \[([^\]]+)\](?: \[[^\]]*\])? score: ([\d.]+) ---$/gm;
  const hits = [];
  let m;
  while ((m = header.exec(text)) !== null) hits.push({ path: m[1], start: Number(m[2]), end: Number(m[3]), lang: m[4], score: Number(m[5]), contentStart: header.lastIndex });
  return hits.map((h, i) => {
    const end = i + 1 < hits.length ? text.lastIndexOf("---", hits[i + 1].contentStart - 1) : text.length;
    const content = text.slice(h.contentStart, end).replace(/^\r?\n/, "").trimEnd();
    return { path: h.path, start: h.start, end: h.end, lang: h.lang, score: h.score, text: content };
  });
}

function runAtlas(corpus, query) {
  const root = corpus.atlas_root;
  const t0 = performance.now();
  const res = spawnSync(LORE_BIN, ["-C", root, "atlas", "search", query, "-n", "10", "--text-only", "--json"], {
    env: { ...process.env, LORE_CACHE: ATLAS_CACHE },
    encoding: "utf8",
    timeout: 120000,
    windowsHide: true,
  });
  const ms = performance.now() - t0;
  if (res.status !== 0) throw new Error(`atlas exit ${res.status}: ${(res.stderr || "").trim().slice(0, 500)}`);
  const parsed = JSON.parse(res.stdout);
  return {
    ms,
    results: (parsed.results || []).map((r) => ({
      path: r.path,
      start: r.start,
      end: r.end,
      lang: r.lang,
      score: r.score,
      via: r.via,
      symbols: r.symbols,
      text: r.text,
    })),
    notes: parsed.notes || [],
  };
}

function toolJson(cmd, args) {
  const res = spawnSync(cmd, args, { encoding: "utf8", timeout: 60000, windowsHide: true });
  if (res.status !== 0) return { error: `exit ${res.status}: ${(res.stderr || "").trim()}` };
  try {
    return JSON.parse(res.stdout);
  } catch {
    return { error: "not JSON", raw: res.stdout.slice(0, 500) };
  }
}

function scoreResults(query, results) {
  const prefix = spec.corpora[query.corpus].atlas_path_prefix || "";
  const targets = [query.expected, ...(query.alternates || [])];
  const normed = results.map((r) => ({ ...r, npath: norm(r.path, prefix) }));
  let answerRank = null;
  if (query.match === "path") {
    const i = normed.findIndex((r) => targets.some((t) => r.npath.toLowerCase() === norm(t.path).toLowerCase()));
    answerRank = i >= 0 ? i + 1 : null;
  } else {
    const i = normed.findIndex((r) => (query.keys || []).some((k) => r.text.includes(k)));
    answerRank = i >= 0 ? i + 1 : null;
  }
  let expectedRank = null;
  for (const t of targets) {
    const i = normed.findIndex(
      (r) =>
        r.npath.toLowerCase() === norm(t.path).toLowerCase() &&
        (query.match === "path" || !t.line || containsLine(r, t.line)),
    );
    if (i >= 0) {
      expectedRank = i + 1;
      break;
    }
  }
  return { normed, answerRank, expectedRank };
}

function top10(results) {
  // Location, rank, score and the symbol/via tags only: the result text is kept
  // in memory for scoring and never written to the tracked artifact.
  return results.slice(0, 10).map((r, i) => ({
    rank: i + 1,
    path: r.path,
    start: r.start,
    end: r.end,
    score: Number(r.score),
    ...(r.symbols ? { symbols: r.symbols } : {}),
    ...(r.via ? { via: r.via } : {}),
  }));
}

function median(values) {
  const v = values.filter(Number.isFinite).sort((a, b) => a - b);
  if (!v.length) return null;
  const mid = Math.floor(v.length / 2);
  return v.length % 2 ? v[mid] : (v[mid - 1] + v[mid]) / 2;
}

async function main() {
  const startedAt = new Date().toISOString();
  const env = {
    startedAt,
    platform: `${os.platform()} ${os.release()} ${os.arch()}`,
    node: process.version,
    lore: { binary: LORE_BIN, version: toolJson(LORE_BIN, ["--version"]).raw || null },
    socraticode: { entry: SOCRATICODE_ENTRY, version: null },
    atlasCache: ATLAS_CACHE,
    reps: REPS,
  };
  const probe = spawnSync(LORE_BIN, ["--version"], { encoding: "utf8" });
  env.lore.version = (probe.stdout || "").trim();

  const atlasStatus = (() => {
    const res = spawnSync(LORE_BIN, ["-C", spec.corpora.roguedod.atlas_root, "atlas", "status", "--json"], {
      env: { ...process.env, LORE_CACHE: ATLAS_CACHE },
      encoding: "utf8",
      timeout: 60000,
      windowsHide: true,
    });
    if (res.status !== 0) return { error: `exit ${res.status}: ${(res.stderr || "").trim().slice(0, 500)}` };
    try {
      return JSON.parse(res.stdout);
    } catch {
      return { error: "not JSON", raw: (res.stdout || "").slice(0, 500) };
    }
  })();
  const indexes = {
    roguedod_socraticode: toolJson("hgs", ["socraticode", "status", spec.corpora.roguedod.socraticode_path, "--json"]),
    ue_socraticode: toolJson("hgs", ["socraticode", "status", spec.corpora.ue.socraticode_path, "--json"]),
    roguedod_atlas: atlasStatus,
  };

  const sessions = {};
  const results = [];
  try {
    for (const [key, corpus] of Object.entries(spec.corpora)) {
      const s = new SocraticodeSession(SOCRATICODE_ENTRY, corpus.socraticode_path);
      await s.start();
      env.socraticode.version = s.version;
      sessions[key] = s;
      // one untimed warm-up so the latency figures describe a server already up
      await s.search("warmup");
    }

    for (const q of spec.queries) {
      const corpus = spec.corpora[q.corpus];
      const record = { ...q, systems: {} };

      const socRuns = [];
      for (let i = 0; i < REPS; i++) {
        const r = await sessions[q.corpus].search(q.query, 10);
        socRuns.push({ ms: Math.round(r.ms), results: parseSocraticode(r.text) });
      }
      {
        const scored = socRuns.map((run) => ({ run, ...scoreResults(q, run.results) }));
        const rankKey = (s) => [s.expectedRank ?? 99, s.answerRank ?? 99];
        scored.sort((a, b) => rankKey(a)[0] - rankKey(b)[0] || rankKey(a)[1] - rankKey(b)[1]);
        const best = scored[0];
        record.systems.socraticode = {
          status: "ok",
          latency_ms: Math.min(...socRuns.map((r) => r.ms)),
          latency_reps_ms: socRuns.map((r) => r.ms),
          answer_rank: best.answerRank,
          expected_rank: best.expectedRank,
          rank_reps: scored.map((s) => ({ expected: s.expectedRank, answer: s.answerRank })),
          top10: top10(best.normed),
        };
      }

      if (corpus.atlas_root) {
        const atlasRuns = [];
        let failed = null;
        for (let i = 0; i < REPS; i++) {
          try {
            const r = runAtlas(corpus, q.query);
            atlasRuns.push({ ms: Math.round(r.ms), results: r.results, notes: r.notes });
          } catch (e) {
            failed = { status: "error", error: String(e.message || e), latency_reps_ms: atlasRuns.map((r) => r.ms) };
            break;
          }
        }
        if (failed) {
          record.systems.atlas = failed;
        } else if (atlasRuns.length) {
          const scored = atlasRuns.map((run) => ({ run, ...scoreResults(q, run.results) }));
          const rankKey = (s) => [s.expectedRank ?? 99, s.answerRank ?? 99];
          scored.sort((a, b) => rankKey(a)[0] - rankKey(b)[0] || rankKey(a)[1] - rankKey(b)[1]);
          const best = scored[0];
          record.systems.atlas = {
            status: "ok",
            latency_ms: Math.min(...atlasRuns.map((r) => r.ms)),
            latency_reps_ms: atlasRuns.map((r) => r.ms),
            answer_rank: best.answerRank,
            expected_rank: best.expectedRank,
            rank_reps: scored.map((s) => ({ expected: s.expectedRank, answer: s.answerRank })),
            notes: best.run.notes,
            top10: top10(best.normed),
          };
        }
      } else {
        record.systems.atlas = {
          status: "unavailable",
          reason: "no Atlas index for this corpus; Atlas has no cross-repository search",
        };
      }
      results.push(record);
    }
  } finally {
    for (const s of Object.values(sessions)) s.close();
  }

  const measured = results.filter((q) => !q.gap);
  const summary = {};
  for (const corpus of Object.keys(spec.corpora)) {
    for (const system of ["socraticode", "atlas"]) {
      const rows = measured.filter((q) => q.corpus === corpus).map((q) => q.systems[system]).filter((r) => r?.status === "ok");
      if (!rows.length) continue;
      const top = (n) => rows.filter((r) => r.expected_rank && r.expected_rank <= n).length;
      const answer = rows.filter((r) => r.answer_rank && r.answer_rank <= 10).length;
      summary[`${corpus}/${system}`] = {
        queries: rows.length,
        expected_at_1: top(1),
        expected_at_3: top(3),
        expected_at_10: top(10),
        answered_at_10: answer,
        latency_median_ms: median(rows.map((r) => r.latency_ms)),
      };
    }
  }

  const artifact = {
    spec: QUERIES_PATH,
    env,
    indexes,
    coverage,
    summary,
    results,
  };
  mkdirSync(dirname(ARTIFACT), { recursive: true });
  writeFileSync(ARTIFACT, JSON.stringify(artifact, null, 2));

  // Markdown report beside the JSON.
  const mdPath = ARTIFACT.replace(/\.json$/, ".md");
  const lines = [];
  lines.push("# RogueDOD / UE control baseline - SocratiCode vs. CodeLore Atlas");
  lines.push("");
  lines.push(`Measured ${startedAt} on ${env.platform}. Raw results: \`${ARTIFACT.split("\\").join("/").replace(ROOT.split("\\").join("/") + "/", "")}\`.`);
  lines.push("");
  lines.push("## Tools and corpora");
  lines.push("");
  lines.push(`- lore: \`${env.lore.version}\` (${LORE_BIN}), Atlas text search (\`--text-only\`).`);
  lines.push(`- SocratiCode: \`${env.socraticode.version}\` via MCP \`codebase_search\` (hybrid BM25 + dense, RRF), server already up.`);
  const soc = indexes.roguedod_socraticode;
  if (soc?.qdrant) lines.push(`- Node ${env.node}; Qdrant ${soc.qdrant.native?.version || "?"} (${soc.qdrant.mode}, ${soc.qdrant.url}); Ollama ${soc.ollama?.provider}/${soc.ollama?.model}.`);
  lines.push(`- RogueDOD revision: \`${spec.corpora.roguedod.git_revision}\` (${spec.corpora.roguedod.git_branch}); dirty: ${spec.corpora.roguedod.git_dirty.join(", ")}.`);
  const build = spec.corpora.ue.build;
  lines.push(`- Unreal Engine ${build.MajorVersion}.${build.MinorVersion}.${build.PatchVersion} (CL ${build.Changelist}, ${build.BranchName}).`);
  const rd = coverage?.roguedod;
  const ue = coverage?.ue;
  if (rd) {
    const a = rd.atlas;
    const s = rd.socraticode;
    lines.push(`- RogueDOD Atlas: ${a.files} files, ${a.pieces} pieces, ${a.symbols} symbols, ${a.edges} import edges, ${a.vectors} vectors (text-only run; semantic configured but not embedded). DB: \`${a.path}\`.`);
    lines.push(`  - extensions: ${Object.entries(a.file_extensions).map(([e, n]) => `${e} ${n}`).join(", ")}; docs ${a.docs}, C++ ${a.cpp_plus_h}, plugin files ${a.plugin_files}.`);
    lines.push(`  - \`Plugins/VaCuus/**\` indexed: **${a.vacuus_plugin_indexed ? "yes" : "no"}** (${a.vacuus_note}).`);
    lines.push(`- RogueDOD SocratiCode: ${s.files_reported}/${s.files_total} files, ${s.chunks} chunks, indexed ${s.last_indexed_at}, built by ${s.built_by_version}, queried with ${s.query_version}; embedding ${s.embedding.provider}/${s.embedding.model} (${s.embedding.dimensions}d, \`${s.embedding.query_prefix}\`).`);
    lines.push(`  - unique paths in payload: ${s.unique_files_in_payload}; top dirs: ${Object.entries(s.top_dirs).map(([d, n]) => `${d} ${n}`).join(", ")}. Plugins: ${Object.entries(s.plugins).map(([p, n]) => `${p} ${n}`).join(", ")}.`);
    lines.push(`  - \`Plugins/VaCuus/**\` indexed: **${s.vacuus_plugin_indexed ? "yes" : "no"}** (${s.vacuus_note}).`);
  }
  if (ue) {
    const u = ue.socraticode;
    lines.push(`- UE SocratiCode: ${u.files_reported}/${u.files_total} files, ${u.chunks} chunks, indexed ${u.last_indexed_at}. CoreMinimal.h is present (${u.coreminimal_chunks} chunks), ActorComponent.cpp ${u.actorcomponent_chunks} chunks, Actor.cpp ${u.actor_cpp_chunks} chunks.`);
    lines.push(`  - ${u.stale_note}`);
    lines.push("- UE Atlas was outside this baseline; engine queries were measured on SocratiCode only.");
  }
  for (const [name, idx] of Object.entries(indexes)) {
    if (idx?.error) lines.push(`- ${name} status error: ${idx.error}`);
  }
  lines.push("");
  lines.push(`Latency: wall clock of one call, ${REPS} repetitions, minimum kept; the SocratiCode server is warm (one untimed search before the run), each Atlas call is a fresh \`lore\` process. Ranking: every repetition is scored; the table reports the best expected rank over them and the JSON keeps all repetitions (\`rank_reps\`), because Qdrant's fused order moves by a rank between identical runs.`);
  lines.push("");
  lines.push("Scoring follows docs/BENCHMARK.md: a result *answers* when its text holds one of the query's keys (exact, case-sensitive substring); file-location queries answer on the expected path. *Expected* is the first result on the expected path (and line, when given), with alternates counted. The VaCuus-plugin probe is excluded from the totals because neither corpus indexes `Plugins/VaCuus`.");
  lines.push("");
  lines.push("## Reproduce");
  lines.push("");
  lines.push("See [benchmark setup](../../tools/bench/README.md#run) for local paths and the isolated cache.");
  lines.push("");
  lines.push("Index metadata: `hgs socraticode status <path> --json`; Qdrant at `http://127.0.0.1:16333` (native, 1.19.1); Atlas counts from `lore atlas status --json`. The shared index and the RogueDOD working tree were not modified; only read-only searches ran.");
  lines.push("");
  lines.push("## Results");
  lines.push("");
  lines.push("| query | corpus | kind | SOC expected | SOC answer | SOC ms | Atlas expected | Atlas answer | Atlas ms |");
  lines.push("|---|---|---|---|---|---|---|---|---|");
  for (const q of results) {
    const s = q.systems.socraticode;
    const a = q.systems.atlas;
    const fmt = (r) => (r?.status === "ok" ? (r.expected_rank ?? "-") : "n/a");
    const fmtA = (r) => (r?.status === "ok" ? (r.answer_rank ?? "-") : "n/a");
    lines.push(
      `| \`${q.id}\` | ${q.corpus} | ${q.kind}${q.gap ? " (gap probe)" : ""} | ${fmt(s)} | ${fmtA(s)} | ${s?.latency_ms ?? "-"} | ${fmt(a)} | ${fmtA(a)} | ${a?.latency_ms ?? "-"} |`,
    );
  }
  lines.push("");
  lines.push("## Summary (measured queries only)");
  lines.push("");
  lines.push("| corpus/system | queries | expected@1 | expected@3 | expected@10 | answered@10 | median ms |");
  lines.push("|---|---|---|---|---|---|---|");
  for (const [k, v] of Object.entries(summary)) {
    lines.push(`| ${k} | ${v.queries} | ${v.expected_at_1} | ${v.expected_at_3} | ${v.expected_at_10} | ${v.answered_at_10} | ${v.latency_median_ms} |`);
  }
  lines.push("");
  lines.push("## Top-10");
  lines.push("");
  for (const q of results) {
    lines.push(`### ${q.id} - \`${q.query}\``);
    lines.push("");
    lines.push(`Expected: \`${q.expected.path}\`${q.expected.line ? `:${q.expected.line}` : ""}.`);
    if (q.edges?.length) lines.push(`Call edges: ${q.edges.map((e) => `\`${e.from}\` -> \`${e.to}\` (${e.call})`).join("; ")}.`);
    lines.push("");
    for (const system of ["socraticode", "atlas"]) {
      const r = q.systems[system];
      lines.push(`**${system}**${r?.status === "ok" ? ` (${r.latency_ms} ms)` : ""}:`);
      lines.push("");
      if (r?.status !== "ok") {
        lines.push(`- ${r?.status}: ${r?.reason || r?.error || ""}`);
      } else {
        const rows = r.top10.slice(0, 10);
        for (const hit of rows) lines.push(`${hit.rank}. \`${hit.path}:${hit.start}-${hit.end}\`${hit.score != null ? ` score ${hit.score}` : ""}`);
        if (!rows.length) lines.push("- (no results)");
      }
      lines.push("");
    }
  }
  writeFileSync(mdPath, lines.join("\n"));

  console.log(JSON.stringify({ artifact: ARTIFACT, markdown: mdPath, summary }, null, 2));
}

await main();
