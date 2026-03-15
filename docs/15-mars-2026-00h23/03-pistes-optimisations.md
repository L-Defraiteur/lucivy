# Pistes d'optimisation — 15 mars 2026

## 1. Cancel flag pour interrompre une recherche en cours

Problème : en WASM, impossible d'interrompre un `search()` en cours. Quand
l'utilisateur tape vite, les recherches précédentes bloquent le worker.

Solution : passer un `AtomicBool` via SharedArrayBuffer. Le scorer Rust
vérifie `cancelled.load(Relaxed)` au début de chaque segment. Si levé,
retour vide immédiat. Côté JS, on set le flag avant chaque nouvelle recherche.

Même pattern que `COMMIT_STATUS` (déjà en place pour le commit async).

Coût : un `Atomics.load` par segment, négligeable.

## 2. Pivot sur le token le plus long (multi-token contains)

Problème : `suffix_contains_multi_token` fait le walk sur le premier token.
Si le premier token est court ("is", "a", "db"), le walk retourne des milliers
de candidats. En fuzzy c'est encore pire (DFA très large sur mot court).

Solution : pivoter sur le token le plus long de la query (le plus discriminant).
1. Walk le suffix FST sur le token le plus long → peu de candidats
2. Pour chaque candidat (doc, Ti), vérifier les tokens adjacents aux positions
   Ti-1, Ti+1 avec les bons séparateurs (GapMap)
3. Valider la chaîne complète dans les deux directions depuis le pivot

Implémentation : réordonner les tokens par longueur dans
`suffix_contains_multi_token_impl`, noter l'offset du pivot, valider
bidirectionnellement.

## 3. Sharding multi-index avec résolution BM25 unifiée

Problème : un seul index avec des millions de documents devient lent à merger
et lourd en mémoire. Cas big data (code search à l'échelle Google, corpus
massifs).

Solution : distribuer les documents sur N shards (N index indépendants),
chacun avec son propre suffix FST et ses posting lists. La recherche
interroge tous les shards en parallèle, puis fusionne les résultats avec
un BM25 global.

Points clés :
- Chaque shard a ses propres statistiques locales (df, total_num_tokens)
- Le BM25 global nécessite des statistiques agrégées : df global =
  somme des df locaux, N global = somme des num_docs
- Approche : pré-collecter les stats globales (2-pass : stats puis scoring).
  Pas de scoring local renormalisé — on veut un BM25 exact, pas une approximation.
- Highlights : une fois les top-K résultats globaux connus, résoudre les
  highlights uniquement sur ces K documents dans leur shard d'origine.
  Pas besoin d'un index temporaire — chaque shard sait produire les
  highlights pour ses propres docs (les offsets sont dans ses posting lists).
- Les shards peuvent être sur des threads, des processes, ou des machines
- Compatible avec l'architecture actor existante (un actor par shard)

Inspiration : Elasticsearch/Lucene (shard + coordinating node), Google
(sharding par document ID range).
