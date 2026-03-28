## Regex search across 4,308 files in 22ms. In the browser. No server.

Most search engines treat regex as a last resort — scan every document, pray it finishes before timeout. We took a different approach.

`rag3.*ver` — find everything where "rag3" appears before "ver", with anything in between. Cross-token, cross-word boundaries. 20 results, ranked by BM25 relevance. **22 milliseconds.**

Here's what's happening under the hood:

**1. No full scan.** The regex is decomposed into literal fragments ("rag3", "ver"). Each literal is resolved through the suffix FST in O(results), not O(index_size).

**2. Multi-literal intersection.** Documents containing both literals are intersected using position ordering — byte offsets tell us "rag3" appears before "ver" in the text.

**3. DFA validation between positions.** A PosMap (position-to-ordinal reverse map) walks the gap between matched literals, feeding bytes to the regex DFA. Early return on accept.

**4. Real BM25 scoring.** Not a flat score. Term frequency (how many times the regex matches per document), document frequency (how many documents match across all index segments), field norms — the full formula, with cross-shard consistency via prescan aggregation.

**5. Runs entirely in WebAssembly.** The search engine compiles to WASM, runs in the browser tab. Zero network latency. Your code never leaves your machine.

The same engine handles substring search, fuzzy matching (Levenshtein automata), and now regex — all through the same suffix FST infrastructure. Substring search on 90K Linux kernel source files: ~700ms. Regex on 5K files: 22ms.

This is lucivy — the full-text search engine behind rag3db.

---

#lucivy #searchengine #regex #fst #suffixarray #bm25 #wasm #webassembly #fulltext #rust #opensource #rag3db
