"use client";

import { useEffect, useMemo, useRef, useState } from "react";

function valueOrDash(value, digits = 1) {
  return Number.isFinite(value) ? Number(value).toFixed(digits) : "—";
}

function metricLabel(metric) {
  return {
    ttftMs: "TTFT",
    totalMs: "Total",
    outputTokens: "Output tokens",
    endToEndTps: "End-to-end tok/s",
    decodeTps: "Post-TTFT tok/s",
  }[metric] || metric;
}

function metricValue(metric, value) {
  if (!Number.isFinite(value)) return "—";
  if (metric.endsWith("Ms")) return `${Math.round(value)} ms`;
  if (metric.endsWith("Tps")) return `${value.toFixed(2)} tok/s`;
  return Math.round(value).toLocaleString();
}

function signedMetricValue(metric, value) {
  if (!Number.isFinite(value)) return "—";
  const sign = value > 0 ? "+" : "";
  return `${sign}${metricValue(metric, value)}`;
}

function percentage(value) {
  if (!Number.isFinite(value)) return "—";
  return `${value > 0 ? "+" : ""}${value.toFixed(1)}%`;
}

async function consumeSse(response, onEvent) {
  const reader = response.body?.getReader();
  if (!reader) throw new Error("Benchmark response did not include a stream");
  const decoder = new TextDecoder();
  let buffer = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const frames = buffer.split(/\r?\n\r?\n/);
    buffer = frames.pop() || "";
    for (const frame of frames) {
      let name = "message";
      let data = "";
      for (const line of frame.split(/\r?\n/)) {
        if (line.startsWith("event:")) name = line.slice(6).trim();
        if (line.startsWith("data:")) data += line.slice(5).trim();
      }
      if (!data) continue;
      onEvent(name, JSON.parse(data));
    }
  }
}

export default function BenchmarkPanel() {
  const [targets, setTargets] = useState([]);
  const [targetId, setTargetId] = useState("");
  const [connectionId, setConnectionId] = useState("");
  const [prompt, setPrompt] = useState("Write a detailed explanation of how streaming token delivery works in an AI API.");
  const [system, setSystem] = useState("");
  const [pairs, setPairs] = useState(5);
  const [maxOutputTokens, setMaxOutputTokens] = useState(256);
  const [temperature, setTemperature] = useState(0);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState("");
  const [samples, setSamples] = useState([]);
  const [result, setResult] = useState(null);
  const [error, setError] = useState("");
  const abortRef = useRef(null);

  useEffect(() => {
    let cancelled = false;
    fetch("/api/playground/targets", { cache: "no-store" })
      .then(async (response) => {
        const data = await response.json();
        if (!response.ok) throw new Error(data.error || "Failed to load benchmark targets");
        if (cancelled) return;
        const nextTargets = Array.isArray(data.targets) ? data.targets : [];
        setTargets(nextTargets);
        const preferred = nextTargets.find((target) => target.benchmarkEligible) || nextTargets[0];
        setTargetId(preferred?.id || "");
        setConnectionId(preferred?.connections?.[0]?.id || "");
      })
      .catch((cause) => !cancelled && setError(cause.message));
    return () => { cancelled = true; };
  }, []);

  const target = useMemo(() => targets.find((item) => item.id === targetId) || null, [targets, targetId]);
  const requestCount = (Number(pairs) + 1) * 2;

  const selectTarget = (id) => {
    const next = targets.find((item) => item.id === id);
    setTargetId(id);
    setConnectionId(next?.connections?.[0]?.id || "");
  };

  const run = async () => {
    if (!target?.benchmarkEligible || !connectionId || !prompt.trim()) return;
    abortRef.current?.abort();
    abortRef.current = new AbortController();
    setRunning(true);
    setError("");
    setSamples([]);
    setResult(null);
    setProgress("Starting benchmark…");
    try {
      const response = await fetch("/api/playground/benchmark", {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
        body: JSON.stringify({
          model: target.id,
          connectionId,
          prompt,
          system,
          pairs: Number(pairs),
          maxOutputTokens: Number(maxOutputTokens),
          temperature: Number(temperature),
        }),
        signal: abortRef.current.signal,
      });
      if (!response.ok) {
        const data = await response.json().catch(() => ({}));
        throw new Error(data.error || `Benchmark failed (${response.status})`);
      }
      await consumeSse(response, (name, data) => {
        if (name === "progress") setProgress(`${data.message}: ${data.mode}`);
        if (name === "sample") setSamples((current) => [...current, data]);
        if (name === "complete") {
          setResult(data);
          setProgress("Benchmark complete");
        }
        if (name === "error") setError(data.error || "Benchmark failed");
      });
    } catch (cause) {
      if (cause.name === "AbortError") {
        setProgress("Cancelled — completed samples are retained");
      } else {
        setError(cause.message || String(cause));
      }
    } finally {
      setRunning(false);
      abortRef.current = null;
    }
  };

  const stop = () => abortRef.current?.abort();
  const scoredSamples = samples.filter((sample) => !sample.warmup);
  const summary = result?.summary;
  const metrics = ["ttftMs", "totalMs", "outputTokens", "endToEndTps", "decodeTps"];

  return (
    <div className="h-full overflow-y-auto custom-scrollbar">
      <div className="mx-auto grid max-w-7xl gap-5 p-4 lg:grid-cols-[minmax(300px,380px)_1fr] lg:p-6">
        <section className="space-y-4 rounded-2xl border border-white/10 bg-white/[0.035] p-4">
          <div>
            <h2 className="text-sm font-semibold">Benchmark setup</h2>
            <p className="mt-1 text-xs leading-5 text-white/45">The same canonical request and account are used for both wire formats.</p>
          </div>

          <label className="block text-xs text-white/60">
            Model
            <select value={targetId} onChange={(event) => selectTarget(event.target.value)} disabled={running} className="mt-1.5 w-full rounded-xl border border-white/10 bg-[#292929] px-3 py-2.5 text-sm text-white outline-none">
              {targets.map((item) => <option key={`${item.type}:${item.id}`} value={item.id}>{item.name} · {item.nativeFormat || item.type}</option>)}
            </select>
          </label>

          {target?.connections?.length > 0 ? (
            <label className="block text-xs text-white/60">
              Pinned account
              <select value={connectionId} onChange={(event) => setConnectionId(event.target.value)} disabled={running} className="mt-1.5 w-full rounded-xl border border-white/10 bg-[#292929] px-3 py-2.5 text-sm text-white outline-none">
                {target.connections.map((connection) => <option key={connection.id} value={connection.id}>{connection.name || connection.id}</option>)}
              </select>
            </label>
          ) : null}

          {!target?.benchmarkEligible ? (
            <div className="rounded-xl border border-amber-400/20 bg-amber-400/10 px-3 py-2 text-xs leading-5 text-amber-100">{target?.benchmarkReason || "Select an eligible explicit model."}</div>
          ) : (
            <div className="rounded-xl border border-blue-400/20 bg-blue-400/10 px-3 py-2 text-xs text-blue-100">OpenAI wrapper vs native {target.nativeFormat}</div>
          )}

          <label className="block text-xs text-white/60">Prompt<textarea value={prompt} onChange={(event) => setPrompt(event.target.value)} disabled={running} rows={5} className="mt-1.5 w-full resize-y rounded-xl border border-white/10 bg-[#292929] px-3 py-2.5 text-sm leading-6 text-white outline-none" /></label>
          <label className="block text-xs text-white/60">System prompt <span className="text-white/30">(optional)</span><textarea value={system} onChange={(event) => setSystem(event.target.value)} disabled={running} rows={2} className="mt-1.5 w-full resize-y rounded-xl border border-white/10 bg-[#292929] px-3 py-2.5 text-sm text-white outline-none" /></label>

          <div className="grid grid-cols-3 gap-2">
            <label className="text-xs text-white/60">Pairs<input type="number" min="1" max="10" value={pairs} onChange={(event) => setPairs(Math.min(10, Math.max(1, Number(event.target.value))))} disabled={running} className="mt-1.5 w-full rounded-xl border border-white/10 bg-[#292929] px-3 py-2 text-sm text-white" /></label>
            <label className="text-xs text-white/60">Max tokens<input type="number" min="1" max="8192" value={maxOutputTokens} onChange={(event) => setMaxOutputTokens(Math.min(8192, Math.max(1, Number(event.target.value))))} disabled={running} className="mt-1.5 w-full rounded-xl border border-white/10 bg-[#292929] px-3 py-2 text-sm text-white" /></label>
            <label className="text-xs text-white/60">Temperature<input type="number" min="0" max="2" step="0.1" value={temperature} onChange={(event) => setTemperature(Math.min(2, Math.max(0, Number(event.target.value))))} disabled={running} className="mt-1.5 w-full rounded-xl border border-white/10 bg-[#292929] px-3 py-2 text-sm text-white" /></label>
          </div>

          <div className="rounded-xl border border-white/10 bg-black/20 px-3 py-2 text-xs leading-5 text-white/55">1 warm-up pair + {pairs} scored pairs = <strong className="text-white">{requestCount} billable requests</strong>. Runs alternate order to reduce provider drift.</div>

          {running ? (
            <button type="button" onClick={stop} className="w-full rounded-xl bg-rose-500 px-4 py-2.5 text-sm font-semibold text-white hover:bg-rose-400">Cancel benchmark</button>
          ) : (
            <button type="button" onClick={run} disabled={!target?.benchmarkEligible || !connectionId || !prompt.trim()} className="w-full rounded-xl bg-white px-4 py-2.5 text-sm font-semibold text-black transition hover:bg-white/90 disabled:cursor-not-allowed disabled:opacity-30">Run {requestCount} requests</button>
          )}
        </section>

        <section className="min-w-0 space-y-4">
          <div className="flex min-h-10 items-center justify-between rounded-2xl border border-white/10 bg-white/[0.035] px-4 py-3">
            <span className="text-sm text-white/70">{progress || "Ready"}</span>
            {running ? <span className="size-4 animate-spin rounded-full border-2 border-white/20 border-t-white" /> : null}
          </div>
          {error ? <div className="rounded-2xl border border-rose-500/20 bg-rose-500/10 p-4 text-sm text-rose-100">{error}</div> : null}

          {summary ? (
            <div className="overflow-x-auto rounded-2xl border border-white/10 bg-white/[0.035]">
              <div className="grid min-w-[760px] grid-cols-[1.25fr_repeat(2,1fr)_repeat(2,1fr)_1.15fr] border-b border-white/10 bg-black/20 px-4 py-2 text-[11px] uppercase tracking-wider text-white/40"><span>Metric</span><span>Wrapper median</span><span>Wrapper p95</span><span>Native median</span><span>Native p95</span><span>Wrapper − native</span></div>
              {metrics.map((metric) => {
                const overhead = summary.overhead?.[metric];
                const wrapperWorse = metric.endsWith("Tps") ? Number(overhead?.absolute) < 0 : Number(overhead?.absolute) > 0;
                return <div key={metric} className="grid min-w-[760px] grid-cols-[1.25fr_repeat(2,1fr)_repeat(2,1fr)_1.15fr] items-center border-b border-white/5 px-4 py-3 text-xs last:border-0"><span className="text-white/60">{metricLabel(metric)}</span><span>{metricValue(metric, summary.wrapper?.[metric]?.median)}</span><span>{metricValue(metric, summary.wrapper?.[metric]?.p95)}</span><span className="text-emerald-300">{metricValue(metric, summary.native?.[metric]?.median)}</span><span className="text-emerald-300">{metricValue(metric, summary.native?.[metric]?.p95)}</span><span className={wrapperWorse ? "text-amber-300" : "text-emerald-300"}>{signedMetricValue(metric, overhead?.absolute)} · {percentage(overhead?.percent)}</span></div>;
              })}
            </div>
          ) : null}

          <div className="overflow-hidden rounded-2xl border border-white/10 bg-white/[0.035]">
            <div className="border-b border-white/10 px-4 py-3"><h2 className="text-sm font-semibold">Samples</h2><p className="mt-1 text-xs text-white/40">Warm-up results are shown while running but excluded from the summary.</p></div>
            {samples.length === 0 ? <div className="p-10 text-center text-sm text-white/35">Run a benchmark to see paired measurements.</div> : (
              <div className="overflow-x-auto"><table className="w-full min-w-[700px] text-left text-xs"><thead className="bg-black/20 text-white/40"><tr><th className="px-4 py-2">Pair</th><th className="px-3 py-2">Mode</th><th className="px-3 py-2">TTFT</th><th className="px-3 py-2">Total</th><th className="px-3 py-2">Tokens</th><th className="px-3 py-2">E2E tok/s</th><th className="px-3 py-2">Decode tok/s</th></tr></thead><tbody>{samples.map((sample, index) => <tr key={`${sample.pair}-${sample.mode}-${index}`} className={`border-t border-white/5 ${sample.warmup ? "text-white/35" : "text-white/75"}`}><td className="px-4 py-2.5">{sample.warmup ? "Warm-up" : sample.pair}</td><td className="px-3 py-2.5 capitalize">{sample.mode}</td><td className="px-3 py-2.5">{sample.error || (sample.ttftMs == null ? "—" : `${sample.ttftMs} ms`)}</td><td className="px-3 py-2.5">{sample.totalMs} ms</td><td className="px-3 py-2.5">{sample.outputTokens ?? "—"}</td><td className="px-3 py-2.5">{valueOrDash(sample.endToEndTps, 2)}</td><td className="px-3 py-2.5">{valueOrDash(sample.decodeTps, 2)}</td></tr>)}</tbody></table></div>
            )}
          </div>
          {scoredSamples.length > 0 && !result ? <p className="text-xs text-white/35">{scoredSamples.length} scored samples retained.</p> : null}
        </section>
      </div>
    </div>
  );
}
