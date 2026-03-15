Petites updates en cours sur lucivy (notre moteur full-text search Rust).

La recherche substring vient de passer de ~3-6 secondes à ~25ms sur 5000+ documents. En WASM. Dans le navigateur.

L'ancien algo utilisait des trigrams (n-grams de 3 caractères) pour filtrer les candidats, puis vérifiait chaque candidat en décompressant le texte stocké. Ça marchait, mais c'était lent.

Le nouveau : un suffix FST custom. Chaque suffixe de chaque token est indexé dans un FST (finite state transducer). Une recherche substring = un simple prefix walk sur le suffix FST. Zéro décompression de texte, zéro faux positifs. Le fuzzy (Levenshtein) passe aussi par le FST via un DFA walk.

Le tout fonctionne en natif, Node.js, Python, et WASM (emscripten + pthreads).

#rust #search #fts #wasm #performance
