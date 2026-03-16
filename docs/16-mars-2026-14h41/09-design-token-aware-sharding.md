# Design : Token-Aware Sharding — 16 mars 2026

## Idée

Sharding par documents avec un routage intelligent basé sur les fréquences per-token per-shard. Chaque shard a son propre .sfx + .sfxpost + inverted index complet. Le nombre de shards est **configuré à la création** (pas dérivé du volume).

## Pourquoi pas round-robin

Round-robin répartit les documents équitablement mais ne garantit pas l'équilibre **per-token**. Un token rare comme "rag3db" (160 docs) peut se retrouver 100/50/10 entre 3 shards. La query pour "rag3db" est limitée par le shard le plus chargé (100) au lieu de 160/3=53.

Le round-robin optimise l'**average-case** (les tokens fréquents sont naturellement répartis). Le token-aware routing optimise le **worst-case** (chaque token est équilibré → parallélisme maximal pour n'importe quelle query).

## Architecture

### Configuration

```json
{
  "fields": [
    {"name": "content", "type": "text"},
    {"name": "path", "type": "text", "stored": true}
  ],
  "shards": 6
}
```

Le nombre de shards est fixé à la création. Chaque shard est un index lucivy indépendant dans un sous-répertoire :

```
index_dir/
  _config.json
  _shard_stats.bin       ← compteurs per-token per-shard
  shard_0/               ← index lucivy complet
  shard_1/
  shard_2/
  ...
  shard_5/
```

### Routage à l'insertion

Chaque shard maintient un compteur per-token :

```rust
struct ShardRouter {
    num_shards: usize,
    /// Per-shard token counts: shard_id → (token_hash → posting_count)
    shard_token_counts: Vec<HashMap<u64, u32>>,
    /// Per-shard total posting entries
    shard_totals: Vec<u64>,
}
```

À l'insertion d'un document :

```rust
fn route(&mut self, doc_tokens: &[&str]) -> usize {
    let mut best_shard = 0;
    let mut best_score = u64::MAX;

    for shard_id in 0..self.num_shards {
        // Score = somme des counts per-token dans ce shard
        // Plus le score est bas, plus ce shard est sous-représenté pour ces tokens
        let score: u64 = doc_tokens.iter()
            .map(|tok| {
                let h = hash(tok);
                *self.shard_token_counts[shard_id].get(&h).unwrap_or(&0) as u64
            })
            .sum();
        if score < best_score {
            best_score = score;
            best_shard = shard_id;
        }
    }

    // Mettre à jour les compteurs
    for tok in doc_tokens {
        let h = hash(tok);
        *self.shard_token_counts[best_shard].entry(h).or_default() += 1;
    }
    self.shard_totals[best_shard] += doc_tokens.len() as u64;

    best_shard
}
```

**Coût** : O(doc_tokens × num_shards) par document. Pour 100 tokens × 6 shards = 600 lookups HashMap. Négligeable vs le coût de tokenization + indexation.

### Heuristique de score

Le score simple (somme des counts) favorise l'équilibre per-token. Variantes possibles :

**1. Score pondéré par IDF** — les tokens rares comptent plus que les tokens fréquents :
```rust
let idf_weight = 1.0 / (1.0 + global_count[tok] as f64).ln();
score += count_in_shard as f64 * idf_weight;
```
Rationale : "import" (fréquent) sera réparti naturellement. "rag3db" (rare) a besoin d'aide.

**2. Score min-max** — minimiser le max imbalance au lieu de la somme :
```rust
let score = doc_tokens.iter()
    .map(|tok| shard_token_counts[shard_id][tok])
    .max()
    .unwrap_or(0);
```
Rationale : le token le plus déséquilibré détermine la latence de query.

**3. Score hybride** — combiner total postings + per-token :
```rust
let token_score: u64 = /* somme des counts */;
let total_penalty = shard_totals[shard_id] / avg_total;
score = token_score + total_penalty * BALANCE_WEIGHT;
```
Rationale : éviter qu'un shard reçoive trop de docs au total même s'il est sous-représenté en tokens.

**Recommandation** : commencer avec le score simple (somme). Itérer sur les heuristiques après benchmarks.

### Query

```rust
fn search(query: &str, shards: &[ShardHandle]) -> Vec<SearchResult> {
    // 1. Query chaque shard en parallèle
    let shard_results: Vec<Vec<(Score, DocId)>> = shards.par_iter()
        .map(|shard| shard.search(query))
        .collect();

    // 2. Merge top-K
    let mut merged = shard_results.into_iter().flatten().collect::<Vec<_>>();
    merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    merged.truncate(top_k);

    merged
}
```

### BM25 cross-shard

Chaque shard a ses propres stats locales. Pour un scoring cohérent :

**Option A (simple)** : Stats globales pré-calculées au commit.
```rust
struct GlobalStats {
    total_docs: u64,
    avg_fieldnorm: f32,       // par champ
    // Pas de per-token doc_freq global — trop coûteux à maintenir
}
```
Le `doc_freq` est calculé localement par shard. L'IDF sera légèrement différent par shard mais avec le token-aware routing, les doc_freq sont proches → IDF quasi-identique.

**Option B (exact)** : Two-pass scoring. Pass 1 : collecter doc_freq de chaque shard. Pass 2 : scorer avec le doc_freq global. Plus lent mais scoring exact.

**Recommandation** : Option A. Le token-aware routing rend les stats locales très proches des globales.

## Avantages vs Elasticsearch

1. **Token-aware routing** — ES utilise hash(doc_id) mod N_shards. Pas de garantie per-token. Lucivy route par fréquence → meilleur worst-case.

2. **Suffix FST natif** — chaque shard a un .sfx plus petit. Contains/fuzzy/regex en parallèle sur N shards.

3. **Configurable sans volume minimum** — "je veux 6 shards" fonctionne même avec 100 docs. Utile pour préparer la montée en charge.

4. **Pas de repartitionnement** — les compteurs guident le routage dès le premier doc. Pas de rebalance après coup.

## Limites

1. **Pas de resharding** — changer le nombre de shards nécessite un reindex complet. Acceptable pour une alpha.

2. **Delete/update** — supprimer un doc décrémente les compteurs du shard. L'équilibre peut se dégrader avec beaucoup de deletes. Fix : rebalance périodique ou lazy.

3. **Mémoire des compteurs** — HashMap<u64, u32> par shard. Pour 1M tokens uniques × 6 shards = ~72 MB. Acceptable. Peut être compressé (bloom filter approximatif pour les tokens rares).

4. **Cross-token queries** — le cross-token (regex multi-token via GapMap) fonctionne car chaque doc est dans UN seul shard. Le GapMap est local au shard.

## Implémentation

### Phase 1 : ShardRouter + multi-index
- `ShardRouter` struct avec compteurs
- `ShardedIndex` wraps N `LucivyHandle`
- `create(dir, config)` crée N sous-index
- `add_document(doc)` route via ShardRouter
- `search(query)` query N shards en parallèle, merge top-K

### Phase 2 : Persistance des compteurs
- `_shard_stats.bin` sérialisé au commit
- Rechargé à l'open pour reprendre le routage

### Phase 3 : BM25 global
- Stats globales agrégées au commit
- Chaque shard lit les stats globales pour scorer

### Phase 4 : Heuristiques avancées
- Score IDF-weighted
- Score min-max
- Benchmarks comparatifs vs round-robin

## Estimation

- Phase 1 : ~200 lignes (ShardRouter, ShardedIndex, parallel search)
- Phase 2 : ~50 lignes (sérialisation compteurs)
- Phase 3 : ~30 lignes (stats globales)
- Phase 4 : itération sur benchmarks
