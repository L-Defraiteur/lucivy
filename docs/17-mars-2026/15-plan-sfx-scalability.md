# Plan — SFX Scalabilité (prochaine session)

Date : 18 mars 2026

## Constat

Le SuffixFstBuilder.build() explose en mémoire sur les gros index :
- 212K docs, 4 shards → 10GB+ RAM, indexation exponentielle
- Cause : accumulation de TOUTES les suffix entries en mémoire pour le sort O(E log E)
- Le DEFAULT_MAX_DOCS_BEFORE_MERGE = 10M (hérité de Tantivy) est beaucoup trop permissif
  car Tantivy n'a pas de suffix FST

## 3 changements combinés

### 1. max_docs_before_merge raisonnable

Baisser le seuil pour que les segments ne dépassent jamais une taille qui explose en mémoire.

- Exposer dans `SchemaConfig` : `max_docs_per_segment: Option<usize>` (défaut ~50K)
- Appliquer dans `create_writer()` via `LogMergePolicy::set_max_docs_before_merge()`
- Résultat : avec 212K docs et 4 shards, chaque shard a ~4 segments de ~13K docs
- Mémoire bornée : ~1.5GB max pour le sfx rebuild d'un segment de 50K docs

### 2. Skip merge_sfx pendant les merges, un seul rebuild au commit

Ne PAS reconstruire le .sfx à chaque merge intermédiaire — c'est du travail jeté
car le segment sera re-mergé plus tard.

- Dans `merger.write()` : skip `merge_sfx()` entièrement
- Après le commit (tous les merges terminés) : rebuild .sfx pour les segments
  qui n'en ont pas
- Résultat : au lieu de N rebuilds en cascade, 1 seul rebuild par segment final

Implémentation :
- Le merger ne produit plus de .sfx (le segment mergé existe sans .sfx)
- Après `IndexWriter::commit()`, scanner les segments, identifier ceux sans .sfx
- Reconstruire le .sfx pour chacun (peut être parallélisé via shard actors)
- Le search vérifie que les .sfx existent avant de query

### 3. Multi-segment sfx search (plus tard, optionnel)

Si un segment mergé n'a pas de .sfx, au lieu de le reconstruire, on query les
.sfx des segments sources. C'est le même pattern que les segments eux-mêmes :
on query N segments en parallèle et on merge les résultats.

Avantages :
- Merge instantané côté sfx (zéro coût, zéro mémoire)
- Scalabilité totale — chaque segment a son .sfx de taille bornée
- Le search est un peu plus lent (query N .sfx au lieu de 1) mais le FST lookup
  est ~microseconde, l'impact est négligeable

Prérequis :
- Garder les .sfx des segments sources (ne pas les supprimer au merge)
- Remap doc_ids : le merger renumérote les docs, il faut un mapping old→new
- Ou bien : utiliser les alive_bitsets des segments sources pour filtrer

C'est la solution la plus propre à long terme, mais plus complexe.
Le point 2 (rebuild au commit) est un bon intermédiaire.

## Ordre d'implémentation

1. **max_docs_before_merge** — simple, immédiat, borne la mémoire
2. **Skip merge_sfx + rebuild au commit** — élimine les rebuilds en cascade
3. **Multi-segment sfx search** — scalabilité totale (optionnel, plus tard)

## Bench à faire

Après chaque étape, bench sur 212K docs :
- `BENCH_MODE=TA` (skip 1-shard pour aller vite)
- Mesurer : temps total, mémoire peak, nombre de segments finaux
- Comparer avec la baseline 1-shard (167s pour 212K)
