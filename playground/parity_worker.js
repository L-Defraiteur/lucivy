// Worker-side parity runner: opens an index persisted in OPFS in place and
// runs playground/parity_panel.json against it through the binding's
// exports (no page state involved). Evaluated inside lucivy-worker.js by the
// debug server (`POST /eval`), which sees the worker's module scope
// (`Module`, `callStr`). Same report shape as parity_run.js; the report
// lands in self._parityResult (poll it), the call itself returns at once.
//
//   python3 -c 'import json;print(json.dumps({"js":open("playground/parity_worker.js").read()}))' > /tmp/req.json
//   curl -s localhost:9877/eval -d @/tmp/req.json          # -> "started"
//   curl -s localhost:9877/eval -d '{"js":"self._parityResult"}' > /tmp/parity_wasm.json
self._parityResult = null;
(async () => {
  const report = [];
  try {
    const path = self._parityPath || '/user_index';
    const ctx = await Module.ccall('lucivy_open', 'number', ['string'], [path], { async: true });
    if (!ctx) { self._parityResult = JSON.stringify([{ name: 'open', error: 'lucivy_open failed for ' + path }]); return; }
    const numDocs = await Module.ccall('lucivy_num_docs', 'number', ['number'], [ctx], { async: true });
    const panel = await (await fetch('/parity_panel.json')).json();
    for (const entry of panel) {
      const t0 = performance.now();
      try {
        const json = await callStr('lucivy_search', ctx, JSON.stringify(entry.query), 100000, 1, 0);
        const r = JSON.parse(json);
        if (r.error) throw new Error(r.error);
        const ms = performance.now() - t0;
        const top = r.slice(0, 10).map(h => ({
          node_id: h.docId,
          score: h.score,
          spans: Object.values(h.highlights || {}).reduce((n, v) => n + v.length, 0),
        }));
        report.push({ name: entry.name, count: r.length, ms, top });
      } catch (e) {
        report.push({ name: entry.name, error: String(e && e.message || e) });
      }
    }
    report.push({ name: '_meta', numDocs });
  } catch (e) {
    report.push({ name: '_fatal', error: String(e && e.message || e) });
  }
  self._parityResult = JSON.stringify(report);
})();
'started'
