I just published **Lucivy** — a full-text search engine with cross-token fuzzy matching.



It started as a fork of tantivy to solve a specific problem: searching substrings and phrases across word boundaries, with typo tolerance. Now it has bindings for Python, Node.js, WASM (browser), C++, and Rust.



Key features:

- Cross-token substring + fuzzy + regex on stored text

- BM25 scoring with trigram-accelerated candidate generation

- Snapshot export/import (.luce format)

- Highlights with byte offsets



Available on:

- PyPI: `pip install lucivy`

[ https://pypi.org/project/lucivy/ ]

- npm: `npm install lucivy` / `npm install lucivy-wasm` [ https://www.npmjs.com/package/lucivy ]

- crates.io: `cargo add ld-lucivy`

[ https://crates.io/crates/ld-lucivy ]

- GitHub: github.com /L-Defraiteur/lucivy

[ github.com/L-Defraiteur/lucivy ]

MIT licensed. Feedback, issues, and contributions welcome.



#opensource #search #rust #python #nodejs #wasm #bm25