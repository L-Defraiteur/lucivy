"""Playwright smoke test for the lucivy playground.

Validates:
1. WASM loads and demo index imports (snapshot)
2. Search returns results with highlights
3. User file import: create index, add docs, commit, search

Run:
    cd playground
    python3 -m http.server 8787 &
    python3 test_playground.py
"""

import subprocess
import time
import signal
import sys
from playwright.sync_api import sync_playwright, TimeoutError as PwTimeout

PORT = 8787
URL = f"http://localhost:{PORT}/"
PLAYGROUND_DIR = __file__.rsplit("/", 1)[0]

# Timeout for the commit step (ms). If commit deadlocks, we want to know fast.
COMMIT_TIMEOUT_MS = 30_000


def start_server():
    proc = subprocess.Popen(
        ["python3", "-m", "http.server", str(PORT)],
        cwd=PLAYGROUND_DIR,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)
    return proc


def dump_logs(logs, label="Browser console"):
    if logs:
        print(f"\n--- {label} (last 40) ---")
        for line in logs[-40:]:
            print(f"  {line}")


def main():
    server = start_server()
    ok = False
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            context = browser.new_context()
            page = context.new_page()

            logs = []
            page.on("console", lambda msg: logs.append(f"[{msg.type}] {msg.text}"))
            page.on("pageerror", lambda err: logs.append(f"[PAGE_ERROR] {err}"))

            # ── Load page and wait for coi-serviceworker reload ──────
            print(f"[test] Loading {URL} ...")
            page.goto(URL, wait_until="domcontentloaded")

            print("[test] Waiting for coi-serviceworker reload...")
            page.wait_for_load_state("networkidle", timeout=10_000)
            page.wait_for_selector(".status.ready", timeout=60_000)

            # ── Test 1: WASM init + demo index loads ─────────────────
            status = page.text_content("#status")
            print(f"[test] Status: {status}")
            assert "indexed" in status.lower(), f"Expected 'indexed' in: {status}"

            # ── Test 2: Search the demo index ────────────────────────
            print("[test] Searching for 'scheduler'...")
            page.fill("#query", "scheduler")
            page.press("#query", "Enter")
            page.wait_for_selector(".result", timeout=15_000)
            results = page.query_selector_all(".result")
            print(f"[test] Got {len(results)} results")
            assert len(results) > 0

            header = page.text_content("#resultsHeader")
            print(f"[test] {header}")

            # ── Test 3: Programmatic create/add/commit/search ────────
            print("[test] Testing create -> add -> commit -> search via JS API...")
            print(f"[test] (commit timeout: {COMMIT_TIMEOUT_MS}ms)")

            # Add step-by-step logging in JS to see exactly where it blocks.
            # Playwright evaluate() doesn't accept timeout kwarg, so we
            # wrap in a JS-level timeout + use page.set_default_timeout.
            page.set_default_timeout(COMMIT_TIMEOUT_MS)
            try:
                result = page.evaluate("""async () => {
                    const lv = window._lucivy;
                    if (!lv) throw new Error('lucivy not initialized on window');

                    console.log('[test3] creating index...');
                    const idx = await lv.create('/test_index', [
                        { name: 'title', type: 'text' },
                        { name: 'body', type: 'text' },
                    ]);
                    console.log('[test3] index created');

                    console.log('[test3] adding 3 docs...');
                    await idx.add(0, { title: 'Hello World', body: 'The quick brown fox' });
                    await idx.add(1, { title: 'Lucivy Engine', body: 'Full-text search with fuzzy matching' });
                    await idx.add(2, { title: 'Rust WASM', body: 'Running natively in the browser' });
                    console.log('[test3] docs added, committing...');

                    const t0 = performance.now();
                    await idx.commit();
                    const commitMs = (performance.now() - t0).toFixed(0);
                    console.log(`[test3] commit done in ${commitMs}ms`);

                    const numDocs = await idx.numDocs();
                    console.log(`[test3] numDocs: ${numDocs}`);

                    const results = await idx.search(
                        { type: 'contains', field: 'body', value: 'fuzzy' },
                        { limit: 10, highlights: true, fields: true }
                    );
                    console.log(`[test3] search returned ${results.length} results`);

                    return { numDocs, results };
                }""")
            except PwTimeout:
                print(f"\n[FAIL] Test 3 timed out after {COMMIT_TIMEOUT_MS}ms — likely deadlock in commit")
                dump_logs(logs)
                raise

            print(f"[test] numDocs: {result['numDocs']}")
            print(f"[test] search 'fuzzy': {len(result['results'])} results")
            assert result["numDocs"] == 3, f"Expected 3 docs, got {result['numDocs']}"
            assert len(result["results"]) > 0, "Expected results for 'fuzzy'"
            first = result["results"][0]
            assert "fuzzy" in first.get("fields", {}).get("body", "").lower(), \
                f"Expected 'fuzzy' in result body: {first}"
            print(f"[test] First result: score={first['score']:.4f}, body={first['fields']['body'][:50]}")

            print()
            print("=" * 50)
            print("ALL TESTS PASSED")
            print("=" * 50)
            ok = True
            browser.close()

    except Exception as e:
        if not str(e).startswith("Test 3 timed out"):
            print(f"\n[FAIL] {e}")
            dump_logs(logs)
        raise
    finally:
        server.send_signal(signal.SIGTERM)
        server.wait()
        sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
