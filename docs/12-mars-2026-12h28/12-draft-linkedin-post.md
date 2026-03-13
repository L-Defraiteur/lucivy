# Draft post LinkedIn — lucivy update

---

**lucivy tourne maintenant à 100% en WASM — indexation comprise, multithreadé.**

Quand j'ai commencé à compiler lucivy en WebAssembly, la recherche marchait, mais l'indexation restait single-thread. Le commit bloquait le main thread. C'était un proof-of-concept, pas un vrai moteur.

Aujourd'hui c'est réglé. Tout tourne en parallèle dans le navigateur :
- Indexation multithreadée via Web Workers + SharedArrayBuffer
- Commit sur thread dédié (plus de freeze UI)
- Recherche concurrente, comme en natif

Et on en a profité pour ajouter un nouveau type de recherche : **startsWith**.

La plupart des moteurs full-text font du prefix search via des trigrams + vérification sur le texte stocké. lucivy exploite directement le FST (Finite State Transducer) — c'est un trie trié par préfixe. Descendre au noeud du préfixe est O(len(prefix)). Pas de trigrams, pas d'I/O sur du stored text. Juste une traversée de trie.

Résultat : **[BENCHMARKS À INSÉRER]**

Le tout avec support fuzzy optionnel (Levenshtein prefix DFA) et multi-token (phrase adjacente avec dernier token = préfixe).

Concrètement, `{ "type": "startsWith", "field": "body", "value": "async prog", "distance": 1 }` trouve tous les documents contenant "async programming", "async programs", etc. — même avec une typo.

Aussi fixé : un bug de normalisation UTF-8 dans le pipeline ngram qui causait des panics sur les caractères accentués/CJK.

**TL;DR** :
- WASM multithreadé à 100% (indexation + recherche + commit)
- startsWith query : prefix search O(len(prefix)) via FST
- Fix UTF-8 pour les caractères multi-octets
- Zéro serveur, tout côté client

Package npm à jour : **[LIEN NPM]**
Testez dans le playground : **[LIEN PLAYGROUND]**

#fulltext #wasm #webassembly #rust #search #opensource

---

## Notes

- Insérer les benchmarks une fois qu'on les a (chantier 1b)
- Insérer le lien npm une fois publié (chantier 2)
- Le playground doit être à jour avec la branche feature/startsWith
- Ton : technique mais pas jargonneux, montrer les résultats concrets
- Longueur : ~1500 caractères, dans la limite LinkedIn sans "voir plus"
