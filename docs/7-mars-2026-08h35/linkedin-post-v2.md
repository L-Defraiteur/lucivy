# LinkedIn Post v2 — Lucivy Open Source Announcement

---

## English version

I've been building a search engine for the past few months. Today I'm open-sourcing it.

**Lucivy** is a BM25 full-text search engine built in Rust, with one key difference from existing engines: **cross-token fuzzy substring matching**.

Most search engines tokenize your text and match individual words. That works fine for simple queries, but it breaks down fast — try searching for a substring like "program" inside "programming", or a phrase with a typo like "programing language", and you get nothing. The words cross token boundaries, the engine gives up.

Lucivy searches stored text directly. It handles:
- **Substrings**: `"program"` matches `"programming"`
- **Typos**: `"programing"` matches `"programming"` (fuzzy, distance=1)
- **Cross-token phrases**: `"programming language"` matches across word boundaries
- **Regex on full text**: `"program.*language"` spans tokens
- **Code-friendly**: `"std::collections"` matches `"use std::collections::HashMap"`

Under the hood, it uses trigram-accelerated candidate generation on character n-grams, then verifies against stored text and scores with BM25. It's fast enough for real-time search over tens of thousands of documents.

I built it because I needed a BM25 complement to vector search inside rag3db — a RAG engine I'm also working on. Semantic search is great for "find me something about X", but when users search for a specific function name, an error message, or a code snippet, you need exact/fuzzy text matching. Lucivy fills that gap.

It runs everywhere:

- PyPI: `pip install lucivy`
[ https://pypi.org/project/lucivy/ ]

- npm: `npm install lucivy` / `npm install lucivy-wasm`
[ https://www.npmjs.com/package/lucivy ]

- crates.io: `cargo add ld-lucivy`
[ https://crates.io/crates/ld-lucivy ]

- GitHub: github . com /L-Defraiteur/lucivy
[ github.com/L-Defraiteur/lucivy ]

- C++ static library (via CXX bridge)

MIT licensed. Fork of tantivy v0.26.0.

Feedback, issues, PRs welcome. If you're building search into a product, RAG pipeline, or dev tool — give it a try and let me know how it goes.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag

---

## Version française

Ça fait quelques mois que je construis un moteur de recherche. Aujourd'hui je le passe en open source.

**Lucivy** est un moteur de recherche full-text BM25, écrit en Rust, avec une différence clé par rapport aux moteurs existants : le **matching fuzzy cross-token sur les sous-chaînes**.

La plupart des moteurs de recherche tokenisent le texte et matchent les mots individuellement. Ça marche pour les requêtes simples, mais ça casse vite — essayez de chercher une sous-chaîne comme "program" dans "programming", ou une phrase avec une faute de frappe comme "programing language", et vous n'obtenez rien. Les mots traversent les frontières de tokens, le moteur abandonne.

Lucivy cherche directement dans le texte stocké :
- **Sous-chaînes** : `"program"` matche `"programming"`
- **Fautes de frappe** : `"programing"` matche `"programming"` (fuzzy, distance=1)
- **Phrases cross-token** : `"programming language"` matche à travers les frontières de mots
- **Regex sur le texte complet** : `"program.*language"` traverse les tokens
- **Adapté au code** : `"std::collections"` matche `"use std::collections::HashMap"`

Sous le capot, le moteur utilise une génération de candidats accélérée par trigrammes sur des n-grammes de caractères, puis vérifie contre le texte stocké et score avec BM25. C'est assez rapide pour de la recherche en temps réel sur des dizaines de milliers de documents.

Je l'ai construit parce que j'avais besoin d'un complément BM25 à la recherche vectorielle dans rag3db — un moteur RAG sur lequel je travaille aussi. La recherche sémantique est top pour "trouve-moi quelque chose à propos de X", mais quand les utilisateurs cherchent un nom de fonction précis, un message d'erreur, ou un extrait de code, il faut du matching textuel exact/fuzzy. Lucivy comble ce vide.

Ça tourne partout :

- PyPI : `pip install lucivy`
[ https://pypi.org/project/lucivy/ ]

- npm : `npm install lucivy` / `npm install lucivy-wasm`
[ https://www.npmjs.com/package/lucivy ]

- crates.io : `cargo add ld-lucivy`
[ https://crates.io/crates/ld-lucivy ]

- GitHub : github.com/L-Defraiteur/lucivy
[ github.com/L-Defraiteur/lucivy ]

- Bibliothèque statique C++ (via CXX bridge)

Licence MIT. Fork de tantivy v0.26.0.

Feedback, issues, PRs bienvenus. Si vous intégrez de la recherche dans un produit, un pipeline RAG, ou un outil dev — testez et dites-moi ce que vous en pensez.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag

---

## Notes

- Les deux versions disent la même chose, adaptées au ton de chaque langue
- On mentionne rag3db brièvement comme contexte (positionne lucivy comme standalone mais montre le use case réel)
- Le post est assez long pour LinkedIn (bien détaillé) mais structuré avec des bullet points pour rester lisible
- Les hashtags incluent #rag pour capter le public RAG/AI
- On peut raccourcir si besoin en coupant le paragraphe "I built it because..." / "Je l'ai construit parce que..."
