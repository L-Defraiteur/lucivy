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
    /// Per-shard token counts (only tokens with df < threshold)
    shard_token_counts: Vec<HashMap<u64, u32>>,
    /// Global token counts (for IDF weight + threshold check)
    global_token_counts: HashMap<u64, u32>,
    /// Per-shard total posting entries
    shard_totals: Vec<u64>,
    /// Only track tokens below this df
    df_threshold: u32,
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

**Score IDF-weighted avec sqrt** — les tokens rares dominent le routage :
```rust
fn route(&mut self, doc_tokens: &[&str]) -> usize {
    let mut best_shard = 0;
    let mut best_score = f64::MAX;

    for shard_id in 0..self.num_shards {
        let score: f64 = doc_tokens.iter()
            .filter_map(|tok| {
                let h = hash(tok);
                let global = *self.global_token_counts.get(&h)?;
                let local = *self.shard_token_counts[shard_id].get(&h).unwrap_or(&0) as f64;
                // sqrt donne un ratio ~14x entre rare (50 docs) et fréquent (10k docs)
                // vs ln qui ne donne que ~2x — trop faible pour différencier
                Some(local / (global as f64).sqrt())
            })
            .sum();
        if score < best_score {
            best_score = score;
            best_shard = shard_id;
        }
    }
    best_shard
}
```

Pourquoi `1/sqrt(global)` et pas `1/ln(global)` :
```
token rare (50 docs)    : 1/sqrt(50)    ≈ 0.14   vs  1/ln(51)    ≈ 0.25
token fréquent (10k)    : 1/sqrt(10000) = 0.01   vs  1/ln(10001) ≈ 0.11
ratio rare/fréquent     : 14x                     vs  2x
```
Le sqrt assure que les tokens mid-frequency (50-500 docs) dominent vraiment le routage. Les tokens fréquents se répartissent naturellement.

### Optimisation mémoire : threshold sur df

**Problème** : tracker TOUS les tokens coûte cher en mémoire.
```
50M tokens uniques × 6 shards × 24 bytes/entry = 7.2 GB  ← trop
```

**Solution** : ne tracker que les tokens avec `df < seuil`. Les tokens fréquents (df > 5000) se répartissent naturellement par la loi des grands nombres — pas besoin de les compter.

```rust
const DF_TRACKING_THRESHOLD: u32 = 5000;

fn should_track(global_count: u32) -> bool {
    global_count < DF_TRACKING_THRESHOLD
}
```

Résultat : ~500k tokens trackés au lieu de 50M → **72 MB** au lieu de 7.2 GB.

Les tokens au-dessus du seuil contribuent un score constant (même poids dans tous les shards) → n'influencent pas le choix du shard.

### Variante : power of two choices

Au lieu de tester TOUS les shards, tester 2 shards aléatoires et prendre le meilleur :

```rust
fn route_p2c(&mut self, doc_tokens: &[&str], doc_id: u64) -> usize {
    let a = hash(doc_id) as usize % self.num_shards;
    let b = hash(doc_id.wrapping_add(SEED)) as usize % self.num_shards;
    let score_a = self.score_shard(a, doc_tokens);
    let score_b = self.score_shard(b, doc_tokens);
    if score_a <= score_b { a } else { b }
}
```

Coût : O(tokens × 2) au lieu de O(tokens × N_shards). Prouvé quasi-optimal en théorie des systèmes distribués. Suffisant pour la plupart des cas, full scan des shards en fallback pour les corpus très déséquilibrés.

### Query

```rust
fn search(query: &str, shards: &[ShardHandle]) -> Vec<SearchResult> {
    // 1. Query chaque shard en parallèle
    let shard_results: Vec<Vec<(Score, DocId)>> = shards.par_iter()
        .map(|shard| shard.search(query))
        .collect();

    // 2. Merge top-K via priority queue (heap merge, not flatten+sort)
    use std::collections::BinaryHeap;
    let mut heap = BinaryHeap::new();
    for (shard_id, results) in shard_results.into_iter().enumerate() {
        for (score, doc_id) in results {
            heap.push((score, shard_id, doc_id));
            if heap.len() > top_k { heap.pop(); }
        }
    }
    heap.into_sorted_vec()
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

3. **Mémoire des compteurs** — Mitigé par le df threshold : seuls les tokens avec df < 5000 sont trackés. ~500k tokens × 6 shards × 24 bytes = ~72 MB. Sans threshold, 50M tokens × 6 shards = 7.2 GB (inacceptable).

4. **Cross-token queries** — le cross-token (regex multi-token via GapMap) fonctionne car chaque doc est dans UN seul shard. Le GapMap est local au shard.

## Implémentation

### Phase 1 : ShardRouter IDF-weighted + multi-index
- `ShardRouter` struct avec compteurs per-token per-shard
- Score IDF-weighted dès le départ (les tokens mid-frequency sont le vrai gain)
- `ShardedIndex` wraps N `LucivyHandle`
- `create(dir, config)` crée N sous-index
- `add_document(doc)` route via ShardRouter
- `search(query)` query N shards en parallèle, heap merge top-K

### Phase 2 : Persistance des compteurs + BM25 global
- `_shard_stats.bin` sérialisé au commit (compteurs + stats globales)
- Rechargé à l'open pour reprendre le routage
- Stats globales agrégées pour BM25 cross-shard

### Phase 3 : Heuristiques avancées + benchmarks
- Score min-max
- Score hybride (per-token + total balance)
- Benchmarks comparatifs vs round-robin sur 100K+ docs

## Estimation

- Phase 1 : ~200 lignes (ShardRouter, ShardedIndex, parallel search)
- Phase 2 : ~50 lignes (sérialisation compteurs)
- Phase 3 : ~30 lignes (stats globales)
- Phase 4 : itération sur benchmarks
