# 14 — Design : Prescan regex — architecture finale

Date : 28 mars 2026

## Constat

### Ce qui existe (contains exact)

Le contains exact a un prescan complet et correct :

```
1. Query.sfx_prescan_params() → Vec<SfxPrescanParam>   (léger, clonable)
2. DAG : PrescanShardNode × N shards en parallèle
   → Pour chaque segment : run_sfx_walk(params) → (doc_tf, highlights)
   → Accumule doc_freq par query_text
3. DAG : MergePrescanNode
   → Fusionne cache (SegmentId → CachedSfxResult) + somme doc_freq cross-shard
4. DAG : BuildWeightNode
   → query.inject_prescan_cache(merged_cache)
   → query.set_global_contains_doc_freqs(merged_freqs)
   → query.weight(EnableScoring::Enabled { stats: Arc<global_stats> })
5. Weight.scorer(reader)
   → Lookup prescan_cache[segment_id] → direct SuffixContainsScorer(doc_tf, bm25, fieldnorm)
   → ZERO .sfx ouvert, ZERO walk refait
```

**Propriété clé** : le travail lourd (SFX walk) est fait UNE SEULE FOIS par segment, pendant le prescan. Le scorer ne fait que lire le cache et construire le BM25.

### Ce qui manque (regex)

Le regex fait TOUT dans `scorer()` :
- Compile le DFA (4ms WASM × 6 segments = 24ms gaspillés)
- Ouvre .sfx, .sfxpost, .posmap par segment
- Appelle `regex_contains_via_literal`
- Construit un `ConstScorer(boost)` — score 1.0 uniforme, pas de BM25

Pas de prescan, pas de cache, pas de doc_freq global, pas de scoring BM25.

## Objectif

1. Le regex passe par le prescan DAG → parallélisme cross-shard
2. `regex_contains_via_literal` s'exécute UNE SEULE FOIS par segment (prescan)
3. Le scorer lit le cache → `SuffixContainsScorer` avec BM25 correct
4. Le doc_freq est global (cross-shard) pour un IDF correct
5. La DFA est compilée UNE SEULE FOIS par shard (pas par segment)
6. Fallback per-segment si prescan skippé (non-DAG path)
7. Zero changement à `regex_contains_via_literal` lui-même

## Architecture

### Nouveau struct : `RegexPrescanParam`

```rust
// src/query/query.rs

#[derive(Clone, Debug)]
pub struct RegexPrescanParam {
    pub field: Field,
    pub pattern: String,
    pub mode: ContinuationMode,
}
```

Léger, clonable, sérialisable. Le pattern est une String — la compilation DFA se fait dans le noeud prescan, pas dans le param.

### Nouveau trait method : `regex_prescan_params()`

```rust
// Sur le trait Query (src/query/query.rs)

fn regex_prescan_params(&self) -> Vec<RegexPrescanParam> { vec![] }
```

Default : vide. `RegexContinuationQuery` retourne ses params. `BooleanQuery` agrège via `flat_map`.

### Nouveau struct de cache : `CachedRegexResult`

On ne réutilise PAS `CachedSfxResult` directement. Raison : le regex retourne `(BitSet, Vec<(DocId, usize, usize)>)`. On convertit en `(doc_tf, highlights)` dans le prescan node, mais on a besoin d'un type dédié pour clarifier la provenance et permettre des extensions futures (ex: byte offsets des matches pour le highlight enrichi).

```rust
// src/query/phrase_query/regex_continuation_query.rs

#[derive(Clone, Debug)]
pub struct CachedRegexResult {
    pub(crate) doc_tf: Vec<(DocId, u32)>,
    pub(crate) highlights: Vec<(DocId, usize, usize)>,
}
```

La conversion BitSet → doc_tf se fait dans le prescan :

```rust
fn highlights_to_doc_tf(highlights: &[(DocId, usize, usize)]) -> Vec<(DocId, u32)> {
    let mut counts: HashMap<DocId, u32> = HashMap::new();
    for &(doc_id, _, _) in highlights {
        *counts.entry(doc_id).or_default() += 1;
    }
    let mut doc_tf: Vec<(DocId, u32)> = counts.into_iter().collect();
    doc_tf.sort_by_key(|&(d, _)| d);
    doc_tf
}
```

### Flow prescan complet

```
                         compile time                          runtime (DAG)
                         ─────────────                         ─────────────
  RegexContinuationQuery ──→ regex_prescan_params()          PrescanShardNode
  "rag3.*ver", field=0       → [RegexPrescanParam {              │
                                  field: 0,                      │ Pour CHAQUE regex param :
                                  pattern: "rag3.*ver",          │
                                  mode: ContainsSubstring,       │   1. Compiler DFA UNE FOIS
                               }]                                │      let regex = Regex::new(&param.pattern)
                                                                 │      let automaton = SfxAutomatonAdapter(&regex)
                                                                 │
                                                                 │   2. Pour CHAQUE segment :
                                                                 │      - Ouvrir .sfx, .sfxpost, .posmap
                                                                 │      - regex_contains_via_literal(...)
                                                                 │      - highlights_to_doc_tf(highlights)
                                                                 │      - cache[segment_id] = CachedRegexResult
                                                                 │      - doc_freq += doc_tf.len()
                                                                 │
                                                                 ▼
                                                           MergePrescanNode
                                                                 │ Fusionne regex_cache + somme doc_freq
                                                                 ▼
                                                           BuildWeightNode
                                                                 │ query.inject_regex_prescan_cache(merged)
                                                                 │ query.set_global_regex_doc_freq(freq)
                                                                 │ query.weight(EnableScoring)
                                                                 ▼
                                                           RegexContinuationWeight
                                                                 │ scorer(reader):
                                                                 │   cache[segment_id] → doc_tf, highlights
                                                                 │   build_scorer(doc_tf, bm25, fieldnorm)
                                                                 │   → SuffixContainsScorer
                                                                 │   ZERO .sfx ouvert
                                                                 │   ZERO DFA compilé
```

### DFA compilé UNE SEULE FOIS par shard

La DFA est compilée dans `PrescanShardNode::execute()` avant la boucle segments :

```rust
// Dans PrescanShardNode::execute() :

for param in &self.regex_prescan_params {
    // 1. Compiler DFA UNE SEULE FOIS pour ce shard
    let regex = Regex::new(&param.pattern)?;
    let automaton = SfxAutomatonAdapter(&regex);

    // 2. Itérer les segments avec la DFA partagée
    for seg_reader in searcher.segment_readers() {
        let (bitset, highlights) = regex_contains_via_literal(
            &automaton, &param.pattern, &sfx_dict, &resolver,
            &sfx_reader, param.mode, max_doc, &ord_to_term_fn,
            posmap_data.as_deref(),
        )?;
        let doc_tf = highlights_to_doc_tf(&highlights);
        doc_freq += doc_tf.len() as u64;
        if !doc_tf.is_empty() {
            cache.insert(seg_reader.segment_id(), CachedRegexResult { doc_tf, highlights });
        }
    }
}
```

Avec 4 shards × 3 segments : 4 compilations DFA (parallèles, ~4ms chacune) au lieu de 12 séquentielles.

### Séparation prescan SFX / prescan regex dans le output

Le prescan output actuel est :

```rust
type PrescanResult = (
    HashMap<SegmentId, CachedSfxResult>,  // SFX cache
    HashMap<String, u64>,                  // doc_freqs (clé = query_text)
);
```

On étend pour inclure le regex :

```rust
type PrescanResult = (
    HashMap<SegmentId, CachedSfxResult>,        // SFX cache
    HashMap<String, u64>,                        // SFX doc_freqs
    HashMap<SegmentId, CachedRegexResult>,       // Regex cache (NOUVEAU)
    HashMap<String, u64>,                        // Regex doc_freqs (NOUVEAU, clé = pattern)
);
```

Le `MergePrescanNode` fusionne les 4 maps. Le `BuildWeightNode` injecte les deux caches séparément :

```rust
self.query.inject_prescan_cache(sfx_cache);      // existant
self.query.inject_regex_prescan_cache(regex_cache); // nouveau
self.query.set_global_contains_doc_freqs(&sfx_freqs);     // existant
self.query.set_global_regex_doc_freqs(&regex_freqs);       // nouveau
```

### Trait Query — nouveaux methods

```rust
// src/query/query.rs — ajouts au trait Query

/// Return regex prescan parameters.
/// RegexContinuationQuery returns its params. BooleanQuery aggregates.
fn regex_prescan_params(&self) -> Vec<RegexPrescanParam> { vec![] }

/// Inject regex prescan cache (merged from multiple shards by the DAG).
fn inject_regex_prescan_cache(
    &mut self,
    _cache: HashMap<SegmentId, CachedRegexResult>,
) {}

/// Set global doc_freq for regex queries (from coordinator aggregation).
fn set_global_regex_doc_freqs(&mut self, _freqs: &HashMap<String, u64>) {}
```

`BooleanQuery` propage comme pour les methods SFX existants.

### RegexContinuationQuery — implémentation

```rust
pub struct RegexContinuationQuery {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    // NOUVEAU :
    regex_prescan_cache: Option<HashMap<SegmentId, CachedRegexResult>>,
    global_regex_doc_freq: Option<u64>,
}

impl Query for RegexContinuationQuery {
    fn regex_prescan_params(&self) -> Vec<RegexPrescanParam> {
        match &self.dfa_kind {
            DfaKind::Regex { pattern } => vec![RegexPrescanParam {
                field: self.field,
                pattern: pattern.clone(),
                mode: self.mode,
            }],
            DfaKind::Fuzzy { .. } => vec![], // fuzzy ne passe pas par regex prescan
        }
    }

    fn inject_regex_prescan_cache(&mut self, cache: HashMap<SegmentId, CachedRegexResult>) {
        self.regex_prescan_cache = Some(cache);
    }

    fn set_global_regex_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        if let DfaKind::Regex { pattern } = &self.dfa_kind {
            if let Some(&df) = freqs.get(pattern) {
                self.global_regex_doc_freq = Some(df);
            }
        }
    }

    fn weight(&self, enable_scoring: EnableScoring<'_>) -> Result<Box<dyn Weight>> {
        let (scoring_enabled, global_num_docs, global_num_tokens) = match &enable_scoring {
            EnableScoring::Enabled { stats, .. } => {
                (true, stats.total_num_docs().unwrap_or(0),
                 stats.total_num_tokens(self.field).unwrap_or(0))
            }
            _ => (false, 0, 0),
        };

        Ok(Box::new(RegexContinuationWeight {
            field: self.field,
            dfa_kind: self.dfa_kind.clone(),
            mode: self.mode,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
            // NOUVEAU :
            scoring_enabled,
            global_num_docs,
            global_num_tokens,
            regex_prescan_cache: self.regex_prescan_cache.clone().unwrap_or_default(),
            global_regex_doc_freq: self.global_regex_doc_freq.unwrap_or(0),
        }))
    }
}
```

### RegexContinuationWeight — scorer avec cache

```rust
struct RegexContinuationWeight {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    // NOUVEAU :
    scoring_enabled: bool,
    global_num_docs: u64,
    global_num_tokens: u64,
    regex_prescan_cache: HashMap<SegmentId, CachedRegexResult>,
    global_regex_doc_freq: u64,
}

impl Weight for RegexContinuationWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
        let segment_id = reader.segment_id();

        // === FAST PATH : prescan cache disponible ===
        if let Some(cached) = self.regex_prescan_cache.get(&segment_id) {
            if cached.doc_tf.is_empty() {
                return Ok(Box::new(EmptyScorer));
            }
            self.emit_highlights(segment_id, &cached.highlights);
            return self.build_scorer(reader, boost, cached.doc_tf.clone());
        }

        // === SLOW PATH : fallback (non-DAG ou prescan skippé) ===
        // Identique au code actuel : compile DFA, ouvre .sfx, run regex_contains_via_literal
        // Mais construit SuffixContainsScorer au lieu de ConstScorer
        let (doc_tf, highlights) = self.run_regex_walk(reader)?;
        if doc_tf.is_empty() {
            return Ok(Box::new(EmptyScorer));
        }
        self.emit_highlights(segment_id, &highlights);
        self.build_scorer(reader, boost, doc_tf)
    }
}

impl RegexContinuationWeight {
    /// BM25 scorer construction — identique à SuffixContainsWeight.build_scorer()
    fn build_scorer(
        &self, reader: &SegmentReader, boost: Score,
        doc_tf: Vec<(DocId, u32)>,
    ) -> Result<Box<dyn Scorer>> {
        let fieldnorm_reader = reader.fieldnorms_readers()
            .get_field(self.field)?
            .unwrap_or_else(|| FieldNormReader::constant(reader.max_doc(), 1));

        let bm25_weight = if self.scoring_enabled {
            let (total_num_docs, total_num_tokens) = if self.global_num_docs > 0 {
                (self.global_num_docs, self.global_num_tokens)
            } else {
                let inv_idx = reader.inverted_index(self.field)?;
                ((reader.max_doc() as u64).max(1), inv_idx.total_num_tokens())
            };
            let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
            let doc_freq = if self.global_regex_doc_freq > 0 {
                self.global_regex_doc_freq
            } else {
                doc_tf.len() as u64
            };
            Bm25Weight::for_one_term(doc_freq, total_num_docs, average_fieldnorm)
        } else {
            Bm25Weight::for_one_term(0, 1, 1.0)
        };

        Ok(Box::new(SuffixContainsScorer::new(
            doc_tf,
            bm25_weight.boost_by(boost),
            fieldnorm_reader,
        )))
    }

    /// Fallback : run regex walk without prescan cache.
    /// Compile DFA, ouvre .sfx/.posmap, appelle regex_contains_via_literal,
    /// convertit (BitSet, highlights) → (doc_tf, highlights).
    fn run_regex_walk(&self, reader: &SegmentReader)
        -> Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)>
    {
        // Code existant de scorer() : compile DFA, ouvre sfx, run walk
        // Mais au lieu de retourner (BitSet, highlights), retourne (doc_tf, highlights)
        // via highlights_to_doc_tf()
        todo!()
    }
}
```

### DAG — needs_prescan étendu

```rust
// lucivy_core/src/search_dag.rs — build_search_dag()

let sfx_params = query.sfx_prescan_params();
let regex_params = query.regex_prescan_params();
let needs_prescan = !sfx_params.is_empty() || !regex_params.is_empty();

// PrescanShardNode reçoit les deux :
PrescanShardNode::new(shard.clone(), sfx_params.clone(), regex_params.clone())
```

### PrescanShardNode — gère les deux types

```rust
pub(crate) struct PrescanShardNode {
    shard: Arc<LucivyHandle>,
    sfx_prescan_params: Vec<SfxPrescanParam>,
    regex_prescan_params: Vec<RegexPrescanParam>,   // NOUVEAU
}

impl Node for PrescanShardNode {
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let searcher = self.shard.reader.searcher();
        let mut sfx_cache = HashMap::new();
        let mut sfx_freqs: HashMap<String, u64> = HashMap::new();
        let mut regex_cache = HashMap::new();
        let mut regex_freqs: HashMap<String, u64> = HashMap::new();

        // --- SFX prescan (existant, inchangé) ---
        for seg_reader in searcher.segment_readers() {
            for param in &self.sfx_prescan_params {
                // ... run_sfx_walk ... (code existant)
            }
        }

        // --- Regex prescan (NOUVEAU) ---
        for param in &self.regex_prescan_params {
            // Compiler DFA UNE SEULE FOIS pour ce shard + ce pattern
            let regex = Regex::new(&param.pattern).map_err(|e| format!("{e}"))?;
            let automaton = SfxAutomatonAdapter(&regex);

            for seg_reader in searcher.segment_readers() {
                // Ouvrir .sfx, .sfxpost
                let sfx_data = match seg_reader.sfx_file(param.field) {
                    Some(d) => d,
                    None => continue,
                };
                let sfx_bytes = sfx_data.read_bytes().map_err(|e| format!("{e}"))?;
                let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
                    .map_err(|e| format!("{e}"))?;

                // Construire resolver + ord_to_term (même code que scorer actuel)
                let resolver = build_resolver(seg_reader, param.field)?;
                let inv_idx = seg_reader.inverted_index(param.field)?;
                let term_dict = inv_idx.terms();
                let sfx_dict = SfxTermDictionary::new(&sfx_reader, term_dict);
                let ord_to_term_fn = |ord: u64| -> Option<String> { /* ... */ };

                // Charger .posmap
                let posmap_bytes = seg_reader.posmap_file(param.field)
                    .and_then(|d| d.read_bytes().ok())
                    .map(|b| b.as_ref().to_vec());

                // Exécuter regex_contains_via_literal
                let (_, highlights) = regex_contains_via_literal(
                    &automaton, &param.pattern, &sfx_dict, &*resolver,
                    &sfx_reader, param.mode, seg_reader.max_doc(),
                    &ord_to_term_fn, posmap_bytes.as_deref(),
                )?;

                let doc_tf = highlights_to_doc_tf(&highlights);
                *regex_freqs.entry(param.pattern.clone()).or_insert(0)
                    += doc_tf.len() as u64;

                if !doc_tf.is_empty() {
                    regex_cache.insert(
                        seg_reader.segment_id(),
                        CachedRegexResult { doc_tf, highlights },
                    );
                }
            }
        }

        ctx.set_output("prescan", PortValue::new(
            (sfx_cache, sfx_freqs, regex_cache, regex_freqs)
        ));
        Ok(())
    }
}
```

### ExportableStats — support distribué

```rust
// lucivy_core/src/bm25_global.rs

pub struct ExportableStats {
    pub total_num_docs: u64,
    pub total_num_tokens: HashMap<u32, u64>,
    pub doc_freqs: HashMap<Vec<u8>, u64>,
    pub contains_doc_freqs: HashMap<String, u64>,
    pub regex_doc_freqs: HashMap<String, u64>,   // NOUVEAU — clé = pattern
}

impl ExportableStats {
    pub fn merge(stats: &[ExportableStats]) -> ExportableStats {
        // ... existant ...
        let mut regex_doc_freqs = HashMap::new();
        for s in stats {
            for (key, &freq) in &s.regex_doc_freqs {
                *regex_doc_freqs.entry(key.clone()).or_insert(0) += freq;
            }
        }
        // ...
    }
}
```

## Comptage des compilations et I/O

### Avant (code actuel)

| Opération | Par segment | Total (4 shards × 3 segments) |
|---|---|---|
| DFA compile | 1 | 12 (séquentiel par shard) |
| Ouvrir .sfx | 1 | 12 |
| Ouvrir .posmap | 1 | 12 |
| regex_contains_via_literal | 1 | 12 |
| **Total DFA compile time** | | 12 × 4ms = **48ms séquentiel** |

### Après (avec prescan)

| Opération | Par shard | Total (4 shards en parallèle) |
|---|---|---|
| DFA compile | 1 | 4 (parallèle) |
| Ouvrir .sfx | 3 (par segment) | 12 |
| Ouvrir .posmap | 3 (par segment) | 12 |
| regex_contains_via_literal | 3 (par segment) | 12 |
| scorer() cache lookup | 3 | 12 |
| **Total DFA compile time** | | max(4ms) = **4ms wall-clock** |

**Gain DFA** : 48ms → 4ms (12× mieux).
**Gain scoring** : ConstScorer(1.0) → BM25 correct cross-shard.
**Gain architecture** : scorer() O(1) lookup, zéro I/O.

## Séparation des responsabilités

### Prescan (PrescanShardNode)
- Compile DFA une fois par shard
- Ouvre .sfx + .posmap par segment
- Appelle regex_contains_via_literal (travail lourd)
- Convertit (BitSet, highlights) → (doc_tf, highlights)
- Accumule doc_freq
- **Produit** : CachedRegexResult + doc_freq par pattern

### MergePrescanNode
- Fusionne les caches (union sur SegmentId, pas de collision cross-shard)
- Somme les doc_freq par pattern
- **Produit** : cache global + doc_freq global

### BuildWeightNode
- Injecte le cache dans la query
- Injecte le doc_freq global dans la query
- Construit EnableScoring avec stats globales
- Appelle query.weight()
- **Produit** : Arc<dyn Weight>

### RegexContinuationWeight.scorer()
- Lookup cache[segment_id] → O(1)
- Emit highlights au sink
- Construit BM25 avec doc_freq global + global_num_docs + fieldnorm
- **Produit** : SuffixContainsScorer

### run_regex_walk() (fallback non-DAG)
- Même code que le scorer actuel
- Utilisé quand prescan skippé (path non-DAG, tests unitaires)
- Compile DFA, ouvre .sfx, run walk, convertit en doc_tf
- Scoring BM25 per-segment (df local, pas global)

## Fichiers à modifier

### ld-lucivy (core)

| Fichier | Changement |
|---|---|
| `src/query/query.rs` | `RegexPrescanParam` struct + 3 nouveaux trait methods |
| `src/query/phrase_query/regex_continuation_query.rs` | `CachedRegexResult`, `highlights_to_doc_tf`, cache fields, weight() rewrite, scorer() two-path, build_scorer() |
| `src/query/boolean_query/boolean_query.rs` | Propager les 3 nouveaux methods |

### lucivy_core (DAG)

| Fichier | Changement |
|---|---|
| `lucivy_core/src/search_dag.rs` | `PrescanShardNode` + `MergePrescanNode` + `BuildWeightNode` — regex params + cache |
| `lucivy_core/src/bm25_global.rs` | `ExportableStats.regex_doc_freqs` + merge |

### Aucun changement

| Fichier | Raison |
|---|---|
| `regex_contains_via_literal` | Inchangé — appelé par le prescan node au lieu du scorer |
| `literal_resolve.rs` | Inchangé — briques réutilisables |
| `posmap.rs` / `bytemap.rs` | Inchangé |
| `SuffixContainsScorer` | Réutilisé tel quel par le regex |
| `collector.rs` / `merger.rs` | Inchangé |

## Points d'attention

### 1. SuffixContainsScorer doit être pub(crate)

Actuellement `struct SuffixContainsScorer` est privé dans `suffix_contains_query.rs`.
Le regex weight doit pouvoir le construire → le rendre `pub(crate)` ou l'extraire
dans un module commun (`scoring_utils.rs` ou similaire).

Alternative : extraire `build_scorer()` en fonction libre dans un module partagé.

### 2. Clé de cache SegmentId — pas de collision regex/contains

Le regex cache et le SFX cache sont des `HashMap` séparés (types différents :
`CachedRegexResult` vs `CachedSfxResult`). Pas de collision possible.

### 3. BooleanQuery avec contains + regex

Si un BooleanQuery contient un SuffixContainsQuery ET un RegexContinuationQuery,
les deux prescans s'exécutent en parallèle dans le même PrescanShardNode.
Chacun stocke dans son cache dédié. L'injection propage aux bonnes sub-queries
via `inject_prescan_cache` et `inject_regex_prescan_cache`.

### 4. Fuzzy DFA dans RegexContinuationQuery

Le `DfaKind::Fuzzy` path ne passe PAS par regex prescan — il continue d'utiliser
`continuation_score_sibling`. `regex_prescan_params()` retourne `vec![]` pour Fuzzy.
Le scorer fallback gère le Fuzzy comme avant (ConstScorer ou futur scoring séparé).

### 5. Timer eprintlns à supprimer

Les `eprintln!("[regex-timer] ...")` dans le scorer actuel doivent être supprimés
ou convertis en DiagBus events.

### 6. Highlights multiples par doc = tf > 1

Un doc peut matcher un regex à plusieurs positions (ex: `rag3.*ver` matche dans
deux paragraphes différents). Chaque position produit un highlight entry.
`highlights_to_doc_tf` compte correctement : tf = nombre de match positions par doc.

### 7. Distribué (ExportableStats)

Pour le search distribué (coordinator + workers) : le coordinator agrège les
`regex_doc_freqs` via `ExportableStats::merge()`, les injecte dans les workers
via `set_global_regex_doc_freqs`. Même pattern que `contains_doc_freqs`.

## Ordre d'implémentation

1. `SuffixContainsScorer` → pub(crate) (ou extraction dans scoring_utils)
2. `RegexPrescanParam` + `CachedRegexResult` + `highlights_to_doc_tf`
3. Trait Query : 3 nouveaux methods + implémentation sur Box<dyn Query>
4. `RegexContinuationQuery` : cache fields + `regex_prescan_params()` + injection
5. `RegexContinuationWeight` : two-path scorer + `build_scorer()` BM25
6. `BooleanQuery` : propagation des 3 methods
7. `PrescanShardNode` : regex params + boucle prescan regex
8. `MergePrescanNode` : fusion regex cache + freqs
9. `BuildWeightNode` : injection regex cache + freqs
10. `build_search_dag` : extraire regex_prescan_params, passer à PrescanShardNode
11. `ExportableStats` : `regex_doc_freqs` field + merge
12. Cleanup : supprimer `eprintln!("[regex-timer]")`, supprimer ConstScorer import inutilisé
13. Tests : vérifier que le scoring regex n'est plus 1.0 uniforme
