# LinkedIn Post — Lucivy Open Source Announcement (drafts)

---

## Option A — Technique + directe

I just open-sourced **Lucivy**, a full-text search engine I built on top of Tantivy.

The main feature: **cross-token fuzzy substring matching**. Most search engines match individual tokens — Lucivy searches stored text directly. It finds substrings, handles typos, matches across word boundaries, and supports regex on full text.

Built for code search, technical docs, and as a BM25 complement to vector databases.

Available everywhere:
- `pip install lucivy`
- `npm install lucivy` (Node.js native)
- `npm install lucivy-wasm` (browser)
- `cargo add ld-lucivy` (Rust)
- C++ static library

MIT licensed. Fork of tantivy v0.26.0.

GitHub: github.com/L-Defraiteur/lucivy

#opensource #search #rust #python #nodejs #bm25 #fulltext

---

## Option B — Storytelling + use case

I've been building a search engine for the past few months. Today I'm releasing it as open source.

**Lucivy** started as a need inside rag3db — I needed a BM25 engine that could find substrings across token boundaries, handle typos, and run everywhere (Python, Node.js, browser, C++, Rust).

Existing engines match individual tokens. Lucivy searches stored text directly:
- `"program"` matches `"programming"` (substring)
- `"programing"` matches `"programming"` (fuzzy, distance=1)
- `"programming language"` matches across word boundaries (cross-token)
- `"program.*language"` works as regex on full text

It's fast — trigram-accelerated candidate generation + BM25 scoring.

Install:
```
pip install lucivy
npm install lucivy
npm install lucivy-wasm
cargo add ld-lucivy
```

MIT licensed. Built on tantivy (Rust).

github.com/L-Defraiteur/lucivy

#opensource #search #rust #python #bm25

---

## Option C — Courte + punch

Open-sourcing **Lucivy** today.

A BM25 search engine that does what others can't: fuzzy substring matching across token boundaries. Search `"programing languag"` and it finds `"programming language"` — typos, substrings, cross-token, all handled.

`pip install lucivy` / `npm install lucivy` / `cargo add ld-lucivy`

MIT. Built in Rust, runs in Python, Node.js, browser (WASM), and C++.

github.com/L-Defraiteur/lucivy

#opensource #rust #search #bm25

---

## Option D — Focus communaute + appel a contribution

I just published **Lucivy** — a full-text search engine with cross-token fuzzy matching.

It started as a fork of tantivy to solve a specific problem: searching substrings and phrases across word boundaries, with typo tolerance. Now it has bindings for Python, Node.js, WASM (browser), C++, and Rust.

Key features:
- Cross-token substring + fuzzy + regex on stored text
- BM25 scoring with trigram-accelerated candidate generation
- Snapshot export/import (.luce format)
- Highlights with byte offsets

Available on:
- PyPI: `pip install lucivy`
- npm: `npm install lucivy` / `npm install lucivy-wasm`
- crates.io: `cargo add ld-lucivy`
- GitHub: github.com/L-Defraiteur/lucivy

MIT licensed. Feedback, issues, and contributions welcome.

#opensource #search #rust #python #nodejs #wasm #bm25

---

## Notes

- Adapter le ton selon si on veut plus "dev technique" (A/C) ou "storytelling" (B) ou "communaute" (D)
- On peut combiner : le storytelling de B + la liste clean de D par exemple
- Les hashtags peuvent etre ajustes (#ai, #rag, #vectordb si on veut attirer le public RAG)
- Mentionner rag3db ou pas ? Ca depend si on veut positionner lucivy comme produit standalone
