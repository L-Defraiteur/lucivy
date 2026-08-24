// Runs playground/parity_panel.json against the index loaded in the page and
// returns the same report shape as lucivy_core/tests/test_playground_parity.rs
// (count, top-10 node ids / scores / span counts, elapsed ms). Evaluated on
// the page's main thread through the debug server:
//
//   python3 -c 'import json;print(json.dumps({"js":open("playground/parity_run.js").read()}))' > /tmp/req.json
//   curl -s localhost:9877/eval/main -d @/tmp/req.json > /tmp/parity_wasm.json
//
// Then: python3 playground/parity_diff.py /tmp/parity_native.json /tmp/parity_wasm.json
(async () => {
  const limit = 100000;
  const panel = await (await fetch('parity_panel.json')).json();
  const report = [];
  for (const entry of panel) {
    const t0 = performance.now();
    try {
      const hits = await window._playground.search(entry.query, { limit, highlights: true, fields: false });
      const ms = performance.now() - t0;
      const top = hits.slice(0, 10).map(h => ({
        node_id: h.docId,
        score: h.score,
        spans: Object.values(h.highlights || {}).reduce((n, v) => n + v.length, 0),
      }));
      report.push({ name: entry.name, count: hits.length, ms, top });
    } catch (e) {
      report.push({ name: entry.name, error: String(e && e.message || e) });
    }
  }
  return JSON.stringify(report);
})()
