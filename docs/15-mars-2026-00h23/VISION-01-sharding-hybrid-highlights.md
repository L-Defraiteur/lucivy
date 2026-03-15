# VISION-01 — Sharding hybrid search avec highlight bag

Date : 15 mars 2026

## Contexte

Architecture distribuée pour du hybrid search (BM25 + dense + sparse)
à grande échelle, avec highlights résolus en un seul aller-retour.

## Architecture

### Par shard

Chaque shard est un Catalog indépendant :
- **Lucivy** : suffix FST + posting lists (BM25)
- **Dense** : index vectoriel (flat/HNSW)
- **Sparse (BGE-M3)** : index inversé sparse, WAND pruning

### Coordinateur

1. Broadcast query à tous les shards
2. Chaque shard retourne ses top-K **par modalité**
3. BM25 global via 2-pass (stats globales : df = somme df locaux, N = somme num_docs). Pas de scoring local renormalisé — BM25 exact.
4. Dense/sparse : scores absolus (cosine, dot product), directement comparables entre shards
5. RRF (reciprocal rank fusion) sur les résultats globaux → top-K final
6. Highlights résolus depuis le highlight bag (voir ci-dessous)

### Highlight bag — zéro round-trip supplémentaire

Chaque shard lucivy ne retourne pas juste le top-K BM25.
Il retourne aussi un **highlight bag** : les byte offsets de tous les
docs qui matchent au moins un terme de la query, même les low-score BM25.

```
Réponse shard lucivy = {
  top_k_bm25: [(doc_id, score), ...],
  stats: { df, total_tokens, num_docs },
  highlight_bag: { doc_id → [(byte_from, byte_to), ...] }  // compressé
}
```

Le highlight bag est léger (paires d'entiers, quelques KB même pour
des milliers de matches). Il voyage avec la réponse du shard.

Après RRF, pour chaque doc du top-K final :
- Lookup dans le highlight bag du shard lucivy correspondant
- Si le doc a été trouvé uniquement par dense/sparse et qu'aucun terme
  de la query n'apparaît textuellement → pas de highlight, et c'est honnête

Avantage : un seul aller-retour. Pas besoin de re-interroger les shards
après la fusion.

### Distribution des documents

- Par hash de doc_id (round-robin) ou par partition logique (par projet, par date, etc.)
- Chaque shard peut être sur un thread, un process, ou une machine
- Compatible avec l'architecture actor existante (un actor par shard)

## Prérequis

- PHASE-8 merge .sfx : FAIT
- Pivot token le plus long : à faire (doc 03)
- Cancel flag recherche : à faire (doc 03)
- Cloud / infra distribuée : pas encore
