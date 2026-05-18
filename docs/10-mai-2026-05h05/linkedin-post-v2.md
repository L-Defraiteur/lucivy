# LinkedIn Post — lucivy v2 release

---

## Version française (recommandée — ton réseau est FR)

Cherchez "ror::lucivyer" dans un moteur de recherche classique. Vous obtenez 0 résultats.

lucivy trouve "Error::LucivyError". En 24ms. Dans le navigateur.

Je viens de publier la v2 de lucivy — un moteur de recherche full-text BM25 qui fait ce qu'aucun autre ne fait : du matching de sous-chaînes cross-token, avec fuzzy et regex, sur n'importe quelle plateforme.

La plupart des moteurs tokenisent votre texte et matchent des mots individuels. Cherchez "mutex" et vous trouvez "mutex" — mais pas "getMutexHandle", pas "pthread_mutex_lock". Le tokenizer les voit comme un seul token opaque.

lucivy matche à l'intérieur des tokens. Et à travers les frontières de tokens. "ror::lucivyer" matche "Error::LucivyError" parce que le moteur SFX suit les liens entre tokens adjacents.

Ce qui est nouveau en v2 :

- Moteur SFX unifié — substring, fuzzy, regex, phrase, prefix, tout passe par un seul engine
- Recherche distribuée — export_stats / merge_stats / search_with_global_stats pour du BM25 multi-machine avec IDF correct
- Sync incrémental — seuls les segments modifiés sont transférés
- BM25 cross-shard exact — scores identiques que vous utilisiez 1 ou 4 shards
- 5 bindings — Python, Node.js, C++, WASM, Rust

Testez dans le playground (tout tourne dans votre navigateur, zéro serveur) :
https://l-defraiteur.github.io/lucivy/

Installez :
pip install lucivy
npm install lucivy
npm install lucivy-wasm
cargo add lucivy-core

GitHub : https://github.com/L-Defraiteur/lucivy
MIT License.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag #codesearch

---

## English version

Search "ror::lucivyer" in any search engine. You get 0 results.

lucivy finds "Error::LucivyError". In 24ms. In the browser.

I just released v2 of lucivy — a BM25 full-text search engine that does what no other engine does: cross-token substring matching, with fuzzy and regex, on any platform.

Most search engines tokenize your text and match individual words. Search "mutex" and you find "mutex" — but not "getMutexHandle", not "pthread_mutex_lock". The tokenizer sees them as single opaque tokens.

lucivy matches inside tokens. And across token boundaries. "ror::lucivyer" matches "Error::LucivyError" because the SFX engine follows sibling links between adjacent tokens.

What's new in v2:

- Unified SFX engine — substring, fuzzy, regex, phrase, prefix, all through one engine
- Distributed search — export_stats / merge_stats / search_with_global_stats for multi-machine BM25 with correct IDF
- Incremental sync — only modified segments are transferred
- Exact cross-shard BM25 — identical scores whether you use 1 or 4 shards
- 5 bindings — Python, Node.js, C++, WASM, Rust

Try the playground (runs entirely in your browser, zero server):
https://l-defraiteur.github.io/lucivy/

Install:
pip install lucivy
npm install lucivy
npm install lucivy-wasm
cargo add lucivy-core

GitHub: https://github.com/L-Defraiteur/lucivy
MIT License.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag #codesearch

---

## Notes

- L'accroche "0 résultats" est le hook — montre le problème que tout le monde a, puis la solution
- Le screenshot du playground avec la recherche "ror::lucivyer" → "Error::LucivyError" est le visuel parfait à joindre au post
- Ne PAS mentionner tantivy ou fork — lucivy est sa propre lib
- Mentionner rag3db seulement si on veut positionner le use case RAG
- Le playground link est le CTA principal — les gens peuvent tester sans rien installer
- Longueur : ~1200 chars FR, ~1100 chars EN — dans la limite LinkedIn sans "voir plus"
