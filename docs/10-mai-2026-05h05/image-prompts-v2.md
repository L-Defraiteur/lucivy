# Image prompts — lucivy v2 LinkedIn post

Le mecanisme SFX : chaque token est decompose en tous ses suffixes. "Error" produit les entries: Error (SI=0), rror (SI=1), ror (SI=2), or (SI=3), r (SI=4). Chaque suffix est stocke dans un FST (Finite State Transducer) trie, partitionne par un prefix byte : 0x00 pour SI=0 (debut de token), 0x01 pour SI>0 (substring). Les tokens adjacents sont relies par des sibling links dans une sibling table, permettant de suivre les chaines cross-token sans graph/DP.

---

## Option A — Suffix decomposition + FST walk

A dark background technical illustration. On the left, the token "Error" is shown decomposed vertically into its suffixes with SI (suffix index) labels: SI=0 "Error", SI=1 "rror", SI=2 "ror", SI=3 "or", SI=4 "r". Each suffix flows as a glowing path into a central tree structure (the FST) — a trie where shared prefixes merge into single branches. The branches glow cyan for SI=0 entries and orange for SI>0 entries. On the right, a search query "ror" enters the FST and follows the orange SI=2 branch, lighting up the path to find "Error". Below, the text "24ms in WASM" in small monospace font. Clean minimalist flat design, developer aesthetic, 16:9 landscape.

---

## Option B — Cross-token sibling links

A dark navy background. Two token boxes sit side by side: "Error" (cyan glow) and "LucivyError" (orange glow). Below each token, its suffixes are shown as stacked translucent layers (like geological strata), labeled SI=0, SI=1, SI=2... Between the two tokens, glowing sibling links (thin luminous arcs) connect them — showing that "Error" is followed by "::" is followed by "LucivyError" in the original text. Above, a search query "ror::lucivyer" is shown as a beam that enters "Error" at SI=2 ("ror"), crosses through the sibling link (::), and continues into "LucivyError" at SI=0 ("lucivyer"). The matched path glows bright. The rest stays dim. Minimalist tech illustration, 16:9 landscape.

---

## Option C — Partitioned FST with SI=0 / SI>0

A stylized binary tree (FST) on a dark background, shown from the side like a root system or neural network. The tree has two distinct root branches, each starting from a labeled partition byte: "0x00 — token start" (cyan) and "0x01 — substring" (warm orange). Under the cyan branch, entries like "Error", "Lock", "Mutex" flow in sorted order. Under the orange branch, suffixes like "rror", "ock", "utex" branch out. A search query "ror" is shown as a bright beam entering the orange partition, walking down the trie node by node (r → o → r), arriving at a final node that lights up and reveals a pointer back to the parent token "Error" with SI=2, token_len=5. The pointer is a dotted arc going back up to the cyan partition. Clean vector style, code editor dark theme colors, 16:9 landscape.

---

## Option D — Falling walk animation style (split detection)

A horizontal timeline showing the query "ror::lucivyer" as a sequence of bytes, each in its own cell. Below it, two parallel tracks represent the FST walk. Track 1: the query enters the FST, walks byte by byte (r-o-r), hits a final node at byte 3 where SI + prefix_len == token_len (the suffix "ror" covers the rest of "Error"). This is the SPLIT POINT — marked with a bright vertical line. Track 2: from the split point, the sibling table provides the next token's ordinal. The remaining query bytes (l-u-c-i-v-y-e-r) restart the FST walk on the next token, matching "LucivyError" at SI=0. The two tracks are connected at the split point by a glowing bridge labeled "sibling link". Dark background, neon cyan and orange paths, terminal/monospace aesthetic, 16:9 landscape.

---

## Option E — Before/After with the real query

Split screen, dark background. Left side: a traditional search engine UI (muted gray). Search bar shows "ror::lucivyer". Below: "0 results found" with an empty state illustration. Right side: lucivy playground UI (vibrant). Same search bar "ror::lucivyer". Below: "7 results in 24ms". The top result "src/error.rs" is expanded showing code with highlighted matches — "Error" glows orange, "LucivyError" glows cyan, connected by a thin bright line through "::". Below the split, a simplified SFX diagram: the query decomposed into "ror" (enters suffix partition) + "::" (sibling link) + "lucivyer" (enters SI=0 partition). Modern UI mockup, code editor dark theme, 16:9 landscape.

---

## Option F — Suffix FST as a city map / metro map

An isometric metro map on a dark background. Two metro lines: the cyan line "SI=0" (token starts) and the orange line "SI>0" (substrings). Stations on the cyan line are labeled with full tokens: "Error", "Lucivy", "Mutex", "Lock". Stations on the orange line are labeled with suffixes: "rror", "or", "ock", "utex". Between certain stations on both lines, transfer bridges (sibling links) connect adjacent tokens — like metro transfer corridors. A bright passenger (the query "ror") boards at station "ror" on the orange line, rides to the transfer bridge "::", crosses to station "LucivyError" on the cyan line. The path glows. Other stations stay dim. Clean geometric metro map style, slightly 3D isometric, developer color palette, 16:9 landscape.

---

## Option G — Microscopic / molecular view

A dark space background. Tokens float as molecular clusters — each token is a chain of connected atom-like spheres (one per byte). "Error" is a 5-atom chain, "LucivyError" a 10-atom chain. Every atom has smaller orbital suffixes radiating from it — at position SI=2 of "Error", the suffix "ror" extends as a glowing tendril. Between the "Error" cluster and the "LucivyError" cluster, a bond (sibling link) connects them through "::" separator atoms. A search beam enters from the left, locks onto the "ror" tendril at SI=2, follows the bond through "::", and connects to the "lucivyer" surface of the next cluster at SI=0. Matched atoms glow bright cyan-orange. Unmatched atoms stay dim translucent. Scientific visualization style, slightly ethereal, 16:9 landscape.

---

## Notes

- **Option B** est la plus fidele au mecanisme reel (sibling links cross-token)
- **Option D** montre le falling_walk step by step — excellent pour un post technique
- **Option E** est la plus "pute a clic" — le before/after parle a tout le monde
- **Option F** est la plus creative et memorable (metro map)
- **Option C** est la plus precise techniquement (partitions SI=0/SI>0)
- Toutes evitent de mentionner trigrams (ancien moteur) — c'est bien le SFX/FST
- Le screenshot du playground reste le meilleur visuel supplementaire a joindre
