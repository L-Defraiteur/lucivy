# Architecture sharding deux couches — 16 mars 2026

## Vue d'ensemble

```
┌─────────────────────────────────────────────────────┐
│  rag3weaver (couche haute)                          │
│                                                     │
│  Routing applicatif : 1 index par entity/repo       │
│  Shard pruning par filtre (entity_id, repo, ...)    │
│                                                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ entity A │  │ entity B │  │ entity C │          │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘          │
└───────┼──────────────┼──────────────┼───────────────┘
        │              │              │
┌───────┼──────────────┼──────────────┼───────────────┐
│  lucivy (couche basse)                              │
│                                                     │
│  Token-aware sharding intra-index                   │
│  Transparent pour l'utilisateur                     │
│                                                     │
│  ┌──┬──┬──┐  ┌──┬──┬──┐  ┌──────────┐             │
│  │s0│s1│s2│  │s0│s1│s2│  │ 1 shard  │  (petit)    │
│  └──┴──┴──┘  └──┴──┴──┘  └──────────┘             │
│  (gros repo)  (gros repo)  (petite entity)         │
└─────────────────────────────────────────────────────┘
```

Les deux couches se composent :
- rag3weaver crée N index (un par entity/repo)
- Chaque index peut être token-aware shardé si le volume le justifie
- Un petit index (< 10K docs) reste en 1 shard, un gros (100K+) peut en avoir 6

## Couche basse : lucivy token-aware sharding

Voir `09-design-token-aware-sharding.md` pour le design détaillé.

**API :**
```rust
// Création
let config = SchemaConfig {
    fields: vec![...],
    shards: Some(6),  // None = 1 shard (pas de sharding)
    ..
};
let handle = LucivyHandle::create(dir, &config)?;

// Insertion — routage transparent
handle.add_document(doc);  // ShardRouter choisit le shard

// Query — parallélisme transparent
handle.search(query);  // dispatch sur 6 shards, heap merge top-K
```

**Ce qui existe déjà et qu'on réutilise :**
- `LucivyHandle` : wrapper d'un index → `ShardedHandle` wrappe N `LucivyHandle`
- `build_query()` : construit la query → inchangé, exécuté N fois
- Scheduler actor model : dispatch du travail sur threads → dispatch queries sur shards
- `.sfx` + `.sfxpost` : complets par shard, aucune modification du format

**Ce qu'il faut ajouter :**
- `ShardRouter` : compteurs per-token per-shard, score IDF-weighted
- `ShardedHandle` : create/open/add/search/commit/close pour N sous-index
- `_shard_stats.bin` : persistance des compteurs
- Heap merge top-K cross-shard
- Stats globales BM25 agrégées

## Couche haute : rag3weaver routing applicatif

**Ce qui existe déjà :**
- `Catalog` avec `register_entity()` : chaque entity a son propre index lucivy
- Recherche par entity_id : filtre au niveau applicatif
- `shutdown()` : flush tous les index

**Ce que le sharding lucivy apporte :**
- Un gros entity (monorepo 200K fichiers) peut être shardé automatiquement
- Le Catalog n'a pas besoin de changer — il crée un `LucivyHandle` avec `shards: 6`
- Le shard pruning applicatif (skip entities entières) se compose avec le parallélisme intra-index

## Fondations à prévoir maintenant

Pour que le sharding s'intègre proprement plus tard, ces points doivent rester vrais :

### 1. LucivyHandle comme unité d'index
Le `LucivyHandle` est l'interface unique pour un index. Le `ShardedHandle` implémentera la même interface. Le code appelant (bindings, rag3weaver) n'a pas besoin de savoir si l'index est shardé ou non.

**Rien à changer** — c'est déjà le cas.

### 2. Queries stateless
`build_query()` prend un schema + config et retourne un `Box<dyn Query>`. La query ne dépend pas de l'état de l'index. On peut l'exécuter sur N shards sans modification.

**Rien à changer** — c'est déjà le cas.

### 3. BM25 stats séparables
Les stats BM25 (total_docs, avg_fieldnorm, doc_freq) doivent pouvoir être injectées plutôt que lues depuis l'index. Actuellement, `EnableScoring::Enabled { statistics_provider }` fournit les stats. Le `ShardedHandle` pourra fournir un stats provider agrégé.

**À vérifier** : que `statistics_provider` peut être construit depuis des stats agrégées. Sinon, adapter l'interface.

### 4. Commit indépendant par shard
Chaque shard peut commit indépendamment. Le `ShardedHandle` coordonne les commits (séquentiel ou parallèle). Le scheduler actor model peut dispatch les commits.

**Rien à changer** — chaque `LucivyHandle` commit indépendamment.

### 5. Format de fichier stable
Le .sfx, .sfxpost, manifest, GapMap ne contiennent aucune référence cross-shard. Chaque shard est un index lucivy complet et autonome.

**Rien à changer** — c'est déjà le cas.

### 6. Merge indépendant par shard
Le merger traite un segment à la fois au sein d'un index. Avec le sharding, chaque shard merge ses propres segments indépendamment.

**Rien à changer** — le merger est déjà per-index.

## Résumé : ce qu'on fait maintenant vs plus tard

### Maintenant (Phase 7c en cours)
- ._raw supprimé du schema ✅
- prefer_sfxpost flag ✅
- Token interning SfxCollector ✅
- Single tokenization sans stemmer ✅
- Merger tous champs avec .sfx ✅

### Prochaine étape (optimisation ingestion)
- Batch suffix generation dans SuffixFstBuilder
- Benchmarks sur 213K docs en release build

### Suite logique (sharding)
- Phase 1 : ShardRouter IDF-weighted + ShardedHandle
- Phase 2 : Persistance compteurs + BM25 global
- Phase 3 : Heuristiques avancées + benchmarks

### Au-dessus (rag3weaver)
- Catalog utilise ShardedHandle pour les grosses entities
- Shard pruning par entity_id au niveau applicatif
- Multi-codebase = multi-index + sharding intra-index

## Use case cible : mémoire long terme agent

Au-delà du code search, lucivy vise à servir de **mémoire persistante pour agents IA** :

- Un agent accumule sessions, code, docs, conversations → **50M+ tokens** sur des mois
- Recherche en temps réel pendant le raisonnement : "ce bug qu'on avait fixé en février"
- Contains/fuzzy/regex sur du texte mixte (code + langage naturel + logs)
- Latence critique : l'agent ne peut pas attendre 30s pour chercher dans sa mémoire

**Implications sur le design :**

- Le df_threshold doit être **configurable** : les tokens de langage naturel ont une distribution différente du code. "the" est ultra-fréquent en anglais, "fonction" est mid-frequency en français.
- Le sharding token-aware est essentiel : la distribution est naturellement déséquilibrée (sessions anciennes vs récentes, code vs prose, langues différentes).
- Le full scan des shards (pas juste power of two choices) doit rester une option pour les corpus à distribution imprévisible.
- La mémoire des compteurs (72 MB pour 500k tokens trackés) est acceptable pour un agent qui tourne en continu.

**Positionnement :** Aucun moteur de recherche existant n'est optimisé pour ce use case. Elasticsearch est trop lourd. Tantivy n'a pas de contains natif. Meilisearch est pour le typo-tolerant UI, pas pour la mémoire agent. Lucivy avec token-aware sharding + suffix FST + .sfxpost est la seule solution qui combine substring search, fuzzy cross-token, regex, BM25, et sharding intelligent dans une lib embeddable.
