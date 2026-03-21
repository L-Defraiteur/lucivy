# Doc 18 — Design : recherche distribuée multi-machines avec BM25 unifié + highlights

Date : 21 mars 2026

## Vision

Shards répartis sur N machines (ou N bases de données), recherche unifiée
avec BM25 global cohérent et highlights, le tout en un seul appel côté client.

```
Client  ──→  Coordinateur  ──→  Machine A (shards 0-3)
                            ──→  Machine B (shards 4-7)
                            ──→  Machine C (shards 8-11)
```

## Protocole de recherche — 3 phases

### Phase 1 : Collect stats (scatter)

Le coordinateur envoie la query à toutes les machines.
Chaque machine retourne ses stats BM25 **sans résultats** :

```rust
struct ShardStats {
    shard_id: u32,
    num_docs: u64,
    /// Pour chaque terme de la query : combien de docs contiennent ce terme
    term_doc_freqs: HashMap<String, u64>,
}
```

C'est léger — quelques dizaines d'octets par shard. Un seul aller-retour réseau.

### Phase 2 : Build Weight (coordinateur)

Le coordinateur agrège les stats :
```rust
let global_num_docs: u64 = all_stats.iter().map(|s| s.num_docs).sum();
let global_term_freqs: HashMap<String, u64> = merge(all_stats);

// Même calcul que AggregatedBm25StatsOwned, mais depuis des stats sérialisées
let global_stats = GlobalBm25Stats { num_docs: global_num_docs, term_doc_freqs: global_term_freqs };
```

Puis envoie la query + stats globales à chaque machine.

### Phase 3 : Search + highlights (scatter-gather)

Chaque machine :
1. Build le Weight avec les stats globales (tous les shards voient le même IDF)
2. Exécute le Weight sur ses shards locaux
3. Retourne `top_k` résultats **avec highlights et stored docs**

```rust
struct ShardSearchResult {
    shard_id: u32,
    score: f32,
    /// Le document stocké (déjà résolu côté machine)
    doc: HashMap<String, serde_json::Value>,
    /// Highlights par champ : { "content": [[5, 10], [20, 30]], "title": [[0, 5]] }
    highlights: HashMap<String, Vec<[usize; 2]>>,
}
```

Le coordinateur fait un binary heap merge sur les résultats de toutes les machines.
**Les highlights sont déjà dans le résultat** — pas besoin d'un aller-retour supplémentaire.

## Pourquoi highlights côté machine (pas côté coordinateur)

Le coordinateur n'a pas accès aux stored docs. Les highlights sont calculés
pendant le search (dans le Weight/Scorer), pas après. Donc chaque machine
doit résoudre :
1. Le match (score + doc_id)
2. Les highlights (byte offsets dans le texte)
3. Le document stocké (pour que le client ait le texte)

Tout ça est retourné en un seul bloc. Le client reçoit des résultats complets,
prêts à afficher.

## Ce qui existe déjà

| Composant | Statut | Fichier |
|-----------|--------|---------|
| `AggregatedBm25StatsOwned` | Agrège stats de N searchers locaux | `lucivy_core/src/bm25_global.rs` |
| `HighlightSink` | Collecte highlights pendant le search | `src/query/phrase_query/scoring_utils.rs` |
| `SearchShardNode` | Search un shard local, retourne top-K | `lucivy_core/src/search_dag.rs` |
| `MergeResultsNode` | Binary heap merge multi-shard | `lucivy_core/src/search_dag.rs` |
| `ShardStorage` trait | Abstraction storage par shard | `lucivy_core/src/sharded_handle.rs` |
| `BlobShardStorage` | Shard dans un BlobStore (DB, S3...) | `lucivy_core/src/sharded_handle.rs` |

## Ce qu'il faut ajouter

### 1. Sérialisation des stats BM25

```rust
#[derive(Serialize, Deserialize)]
struct SerializableBm25Stats {
    num_docs: u64,
    term_doc_freqs: Vec<(Vec<u8>, u64)>,  // term bytes → doc freq
}

impl AggregatedBm25StatsOwned {
    /// Construct from serialized stats (distributed mode).
    fn from_serialized(stats: &[SerializableBm25Stats]) -> Self { ... }

    /// Export stats for this shard (to send to coordinator).
    fn export_stats(searcher: &Searcher, query_terms: &[Term]) -> SerializableBm25Stats { ... }
}
```

~30 lignes. Trivial avec serde.

### 2. `search_with_docs()` sur ShardedHandle

Retourne les résultats avec docs + highlights résolus :

```rust
struct SearchHit {
    pub score: f32,
    pub shard_id: usize,
    pub doc: HashMap<String, serde_json::Value>,
    pub highlights: HashMap<String, Vec<[usize; 2]>>,
}

impl ShardedHandle {
    pub fn search_with_docs(
        &self, config: &QueryConfig, top_k: usize,
    ) -> Result<Vec<SearchHit>, String> {
        let sink = Arc::new(HighlightSink::new());
        let results = self.search(config, top_k, Some(sink.clone()))?;

        results.iter().map(|r| {
            let shard = self.shard(r.shard_id).unwrap();
            let searcher = shard.reader.searcher();
            let seg_reader = searcher.segment_reader(r.doc_address.segment_ord);
            let doc = searcher.doc::<LucivyDocument>(r.doc_address)?;
            let highlights = sink.get(seg_reader.segment_id(), r.doc_address.doc_id)
                .unwrap_or_default();

            Ok(SearchHit {
                score: r.score,
                shard_id: r.shard_id,
                doc: doc_to_json(&doc, &self.schema),
                highlights,
            })
        }).collect()
    }
}
```

~40 lignes. Résout le boilerplate highlights pour local ET distribué.

### 3. Trait `SearchNode` pour abstraction local/remote

```rust
trait SearchNode: Send + Sync {
    /// Collect BM25 stats for query terms.
    fn collect_stats(&self, terms: &[Term]) -> Result<SerializableBm25Stats, String>;

    /// Execute search with pre-built global stats.
    fn search(
        &self, config: &QueryConfig, global_stats: &GlobalBm25Stats, top_k: usize,
    ) -> Result<Vec<SearchHit>, String>;
}

/// Local shard — appel direct
struct LocalSearchNode {
    handle: Arc<LucivyHandle>,
    shard_id: usize,
}

/// Remote shard — appel réseau
struct RemoteSearchNode {
    endpoint: String,  // "http://machine-b:8080/shard/4"
    shard_id: usize,
}
```

Le `ShardedHandle` (ou un `DistributedHandle`) itère sur `Vec<Box<dyn SearchNode>>`.
Local et remote se mélangent — une machine peut avoir des shards locaux ET requêter
des shards distants.

### 4. Transport (hors lucivy)

Le transport est hors scope de lucivy. L'utilisateur branche ce qu'il veut :
- gRPC (tonic)
- HTTP REST (axum/actix)
- Message queue (NATS, Redis streams)
- TCP brut

Lucivy fournit `SearchNode` trait + serde structs. Le transport implémente
`RemoteSearchNode`.

## Cas d'usage concrets

### Cas 1 : Multi-DB dans rag3weaver

```
Catalog "products"  →  BlobShardStorage(PostgresBlobStore("db1"))
                       3 shards, 1M docs

Catalog "articles"  →  BlobShardStorage(PostgresBlobStore("db2"))
                       2 shards, 500K docs
```

Chaque catalog a ses propres shards dans sa propre DB.
La recherche est locale (même process), BM25 unifié par catalog.

### Cas 2 : Cluster distribué

```
Machine A : shards 0-3 (local SSD, StdFsDirectory)
Machine B : shards 4-7 (local SSD, StdFsDirectory)
Coordinateur : DistributedHandle avec 8 RemoteSearchNode
```

Le coordinateur ne stocke rien. Il orchestre :
1. Scatter stats → A + B
2. Agrège → global BM25
3. Scatter search → A + B (avec global stats)
4. Merge → top-K final avec docs + highlights

### Cas 3 : Hybrid local + remote

```
Machine locale : shards 0-1 (données chaudes, LocalSearchNode)
Cloud :          shards 2-5 (données froides, RemoteSearchNode via S3)
```

Le `DistributedHandle` mélange local et remote transparently.

## BlobStore par shard — déjà possible

```rust
// Shard 0 dans Postgres
let store_0 = Arc::new(PostgresBlobStore::new("postgres://db1/..."));
// Shard 1 dans S3
let store_1 = Arc::new(S3BlobStore::new("s3://bucket/shard_1/"));
// Shard 2 en local
let store_2 = Arc::new(FsBlobStore::new("/data/shard_2/"));

struct MultiBlobStorage {
    stores: Vec<Arc<dyn BlobStore>>,
}
impl ShardStorage for MultiBlobStorage {
    fn create_shard_handle(&self, id: usize, config: &SchemaConfig) -> Result<LucivyHandle, _> {
        let dir = BlobDirectory::new(self.stores[id].clone(), &format!("shard_{id}"), cache_path);
        LucivyHandle::create(dir, config)
    }
}
```

Le `ShardStorage` trait supporte déjà ça. Il suffit d'implémenter les BlobStore
backends (Postgres, S3, etc.) qui sont hors du repo lucivy.

## Ordre d'implémentation

1. **`search_with_docs()`** — quick win, résout le boilerplate local (~40 lignes)
2. **`SerializableBm25Stats`** — serde structs pour les stats (~30 lignes)
3. **`SearchNode` trait** — abstraction local/remote (~50 lignes)
4. **`LocalSearchNode`** — implémentation locale (~30 lignes)
5. **Transport** — hors scope lucivy (l'utilisateur fait son `RemoteSearchNode`)

Total lucivy : ~150 lignes pour le mode distribué. Le gros du travail
est dans le transport et les BlobStore backends, qui sont hors repo.

## Highlights dans le mode distribué

**Clé : les highlights sont résolus côté machine, pas côté coordinateur.**

Chaque `SearchNode::search()` retourne des `SearchHit` avec highlights inclus.
Le coordinateur ne fait que merger les top-K — il n'a pas besoin d'accéder
aux stored docs ni aux segments.

Le `HighlightSink` actuel est in-process (HashMap avec segment_id + doc_id).
Pour le distribué, les highlights sont extraits dans `SearchHit` avant
sérialisation → le sink reste local à chaque machine.

Pas de changement dans le moteur de search. Juste un packaging différent
des résultats.
