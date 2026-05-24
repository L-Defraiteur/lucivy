# Knowledge Dump — SFX v3 Session 6 (24 mai 2026)

Mise à jour du knowledge dump 03. Couvre les ajouts session 6.

## Commandes

### Ground truth

```bash
# Prérequis
git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git /tmp/rag3db-bench

# Ground truth 5K docs (release, ~25s index + ~30s queries)
cargo test -p lucivy-core --test test_sfx_v3_ground_truth --release \
  -- v3_ground_truth_contains --nocapture

# Avec diagnostics complets (re-test en isolation + traces JSON)
V3_DIAG=1 cargo test -p lucivy-core --test test_sfx_v3_ground_truth \
  --release -- v3_ground_truth_contains --nocapture

# Rapport texte
cat /tmp/v3_ground_truth_report.txt

# Fichier JSON des fails (quand V3_DIAG=1)
cat /tmp/v3_ground_truth_fails.json

# Traces JSON par query (quand V3_DIAG=1)
ls /tmp/v3_trace_*.json
```

### Tests lib

```bash
cargo test --lib                    # tous (~150s debug)
cargo test --lib -- --release       # tous (~30s release)
cargo test --lib test_tablefunction_relaxed_real_docs  # test ciblé
```

### Analyser un trace JSON

```python
import json
traces = json.load(open('/tmp/v3_trace_std_unique_ptr_relax.json'))

# Compter les types d'events
from collections import Counter
labels = Counter()
for t in traces:
    for ev in t['events']:
        labels[ev['label']] += 1
for l, c in labels.most_common(15):
    print(f'  {l}: {c}')

# Chercher les events spécifiques
for i, t in enumerate(traces):
    for ev in t['events']:
        if 'PARTIAL' in ev['label']:
            print(f'seg {i}: {ev["label"]}')

# Analyser les splits d'un segment
t = traces[0]
for ev in t['events']:
    if ev['label'] == 'split':
        print(f"split: ord={ev['data'].get('ord')} text={ev['data'].get('text')}")
```

## Architecture — ce qui a changé en session 6

### BriquesContext (context.rs)

Remplace 10+ params `Option<&Reader>` par un struct unique :

```rust
pub struct BriquesContext<'a> {
    pub reader: &'a SfxFileReaderV3,
    pub resolver: &'a dyn PostingResolver,
    pub filter_docs: Option<&'a HashSet<DocId>>,
    pub debug: bool,
    pub trace_id: Option<u64>,
    // Index files — None = not available
    pub posmap: Option<PosMapReader<'a>>,
    pub bytemap: Option<ByteBitmapReader<'a>>,
    pub word_sfxpost: Option<WordSfxPostReader<'a>>,
    pub sibling_v3: Option<SiblingTableReader<'a>>,
    pub termtexts: Option<TermTextsReaderV3<'a>>,
}
```

- `require_*()` → panic si manquant
- `has_word_pipeline()` → posmap + bytemap + word_sfxpost tous Some
- `has_sibling_chains()` → sibling_v3 + termtexts tous Some
- `trace()` / `trace_msg()` / `trace_enter()` / `trace_exit()` → no-op si trace_id=None
- Ajouter un index file = 1 champ ici + loading dans contains_query_v3.rs

### Sibling Table v3 (sibling_table.rs, collector_v3.rs)

Format identique à la v2 : `ordinal → [(next_ordinal, gap_len)]`.

**IMPORTANT** : dans la v3, `gap_len` stocke le **content_len du DESTINATION**
(pas du source). Le DFS utilise ce content_len pour tronquer le texte du
sibling à sa portion content (excluant l'overlap) avant comparaison.

Construction dans `add_value()` :
- Chunk siblings : chunks consécutifs dans la même value
- Word siblings : word-stripped entries consécutives

```rust
// chunk siblings : content_len du chunk destination
for w in chunk_intern_ids.windows(2) {
    let meta = &self.token_meta[w[1] as usize]; // DESTINATION
    let content_len = meta.own_len - meta.sep_len;
    self.sibling_pairs.push((w[0], w[1], content_len));
}

// word siblings : content_len du mot destination
for w in ws_intern_sequence.windows(2) {
    self.sibling_pairs.push((w[0].0, w[1].0, w[1].1)); // w[1].1 = dest content_len
}
```

Remappage intern→final dans `into_data()`.
Enregistré comme "sibling_v3" dans le registry.

### sibling_chain_dfs (fst_walk.rs)

Même algorithme que v2 (`suffix_contains.rs:880-918`) :
1. Pour chaque split initial : remainder = query[split_byte..]
2. DFS via stack : suivre sibling links, comparer remainder avec content du sibling
3. TERMINAL si content couvre le remainder (prefix match)
4. PARTIAL si remainder start_with content → consommer et continuer

**Clé** : le content est tronqué via `gap_len` (content_len du destination) :
```rust
let content_len = sib.gap_len as usize;
let next_content = if content_len > 0 && content_len < next_lower.len() {
    let cl = snap_to_char_boundary(&next_lower, content_len);
    &next_lower[..cl]
} else {
    &next_lower
};
```

### splits_from_fst_candidates (fst_walk.rs)

Rattrape les premiers splits que le falling walk rate (query épuisé dans
l'overlap zone, noeud FST non-final) :

```rust
for cand in candidates {
    let content_len = cand.content_len() as usize;
    let split_byte = content_len.saturating_sub(cand.sti as usize);
    if split_byte > 0 && split_byte < query_len {
        // query extends past content boundary → split
        splits.push(SplitCandidateV3 { ... });
    }
}
```

### QueryTrace (trace.rs)

Store global thread-safe : `LazyLock<Mutex<HashMap<u64, QueryTrace>>>`.

```rust
// Créer un trace
let tid = trace::trace_begin();

// Pusher des events (depuis n'importe quel thread)
trace::trace_event(tid, "label", &[("key", &value)]);
trace::trace_enter(tid, "section");  // augmente depth
trace::trace_exit(tid);              // diminue depth

// Récupérer et supprimer
let trace = trace::trace_finish(tid);
println!("{}", trace.dump());  // arbre indenté

// Ou drain tout
let all = trace::trace_drain_all();  // Vec<(tid, QueryTrace)>
```

Via le BriquesContext :
```rust
ctx.trace_msg(&format!("splits found={}", n));  // no-op si trace_id=None
ctx.trace_enter("find_literal_v3");
ctx.trace_exit();
```

### V3_DIAG mode (test_sfx_v3_ground_truth.rs)

Env var `V3_DIAG=1` active :
1. **Pass 1** : ground truth normal → `fails.json` exporté
2. **Pass 2** : pour chaque fail :
   - Re-indexe les FN docs en isolation → verdict SCALE-DEPENDENT ou PER-DOC BUG
   - Re-run la query sur le gros index avec `V3_DEBUG_QUERY=<query>` → traces JSON

Env var `V3_DEBUG_QUERY=<query>` : dans `contains_query_v3.rs`, active
`debug=true` et `trace_id=Some(trace_begin())` pour la query spécifiée.

### WordSfxPost cross-join fix (collector_v3.rs)

Avant : `content_key_to_interns` pour trouver les postings → cross-join entre
chunks de mots différents partageant le même content key.

Après : `token_postings[dws.first_chunk_intern]` directement. Le tokenizer
est déterministe, même mot → même intern_ord. Plus de cross-join.

Ajout de `num_chunks` au `WordStrippedEntry` pour vérifier la distance
exacte (`last_pos - first_pos == num_chunks - 1`).

### Ground truth grep word-adjacency

Le grep relaxed du test matche par mots adjacents (pas concaténation globale).
Algorithme : split en mots (runs content chars), sliding window de mots
consécutifs, cherche la query strippée dans la concaténation.

Fix : le `break` dans la boucle inner ne sort plus quand `concat >= query`
si le match pourrait chevaucher deux mots. Continue tant que
`concat.len() < query.len() * 2`.

## Pipeline query v3 — flow complet mis à jour

```
Query "stduniqueptr" strict_sep=false
  │
  ├─ Orchestrator: contains_v3(ctx, query, ...)
  │   strip query → "stduniqueptr"
  │
  ├─ Composite: find_literal_v3(ctx, "stduniqueptr", ...)
  │   │
  │   ├─ fst_candidates(all partitions) → candidates[]
  │   │
  │   ├─ resolve_single(candidates) → single-token matches
  │   │
  │   ├─ Chunk pipeline (0x00 + 0x01)
  │   │   ├─ cross_chunk_chain_v3 (falling walk chains)
  │   │   ├─ [si sibling_v3 dispo] sibling_chain_dfs (chunk siblings)
  │   │   └─ resolve_chains_v3 (strict pos+1)
  │   │
  │   └─ Word pipeline (0x02, si posmap + bytemap + word_sfxpost)
  │       ├─ cross_word_chain_v3 (falling walk chains)
  │       ├─ [si sibling_v3 dispo]
  │       │   ├─ splits_from_fst_candidates (rattrape splits ratés)
  │       │   └─ sibling_chain_dfs (word siblings, DFS)
  │       └─ resolve_word_chains_v3 (relaxed, WordSfxPost)
  │
  ├─ content_len filter (span=1, byte_span >= query_content_len)
  ├─ dedup par (doc_id, position)
  └─ exact_match filter (si demandé)
```

## Ajouter un index file — procédure simplifiée (session 6)

Grâce au BriquesContext, ajouter un index file ne nécessite plus de modifier
les signatures de 10 fonctions :

1. **Module** : `src/suffix_fst/mon_index.rs` (writer/reader)
2. **Registry** : `all_indexes()` + SfxIndexFile impl
3. **mod.rs** : `pub mod mon_index;`
4. **Collector** : construire dans `into_data()`, ajouter au struct SfxCollectorDataV3
5. **DAG** : `derived.push(("mon_index", data.mon_index.clone()))`
6. **BriquesContext** : 1 champ `pub mon_index: Option<MonIndexReader<'a>>`
7. **contains_query_v3.rs** : `let mi = load("mon_index"); ... mon_index: mi,`
8. **fuzzy_query_v3.rs** : idem
9. **Briques** : `ctx.require_mon_index()` ou `ctx.mon_index.as_ref()`

PAS besoin de modifier : orchestrator, composite, resolve, regex, tests (sauf
si l'index est utilisé dans ces couches).

## Fichiers clés — tailles mises à jour

| Fichier | Rôle |
|---------|------|
| `briques/context.rs` | BriquesContext (struct + helpers + trace) |
| `briques/trace.rs` | QueryTrace (store global Mutex, events, dump) |
| `briques/fst_walk.rs` | Falling walk, fst_candidates, chains, **sibling_chain_dfs**, splits_from_fst_candidates |
| `briques/composite.rs` | find_literal_v3 (chunk + word + sibling chains) |
| `briques/orchestrator.rs` | contains_v3, fuzzy_v3 (ctx-based) |
| `briques/resolve.rs` | resolve single/chains/word_chains |
| `collector_v3.rs` | Tokenisation, interning, WordSfxPost, **sibling pairs** |
| `sibling_table.rs` | SiblingTableWriter/Reader (format v2, gap_len=dest content_len) |
| `word_sfxpost.rs` | WordSfxPost writer/reader |
| `query/contains_query_v3.rs` | Chargement ctx + V3_DEBUG_QUERY |
| `query/fuzzy_query_v3.rs` | Chargement ctx |

## État des tests

```
Lib tests : 1422 pass, 3 fail (tests diag legacy sans maps)
Ground truth 500 docs  : 15/15
Ground truth 5000 docs : 11/15
  - 3 FN strict (function, rag3db) — 1 doc chaque, scale-dependent
  - 1 FP relax (function) — cross-token chunk FP
  - 1 FN relax (TableFunction) — 1 doc, scale-dependent
```

## Bugs connus non résolus

### Single-token FN à grande échelle

Les queries "function" et "rag3db" strict perdent 1 doc chacune quand
il y a 5K docs. Les docs contiennent le query en substring d'un mot plus
long ("functionality", "rag3dbjs"). Le chunk correspondant DEVRAIT être
trouvé par fst_candidates (range query), et le resolve DEVRAIT produire
le match. Mais quelque chose le perd.

**Hypothèses restantes** :
- Posting manquant pour l'ordinal dans le segment concerné
- byte_span calculé incorrectement pour un cas edge
- Collision d'ordinal avec un autre chunk de même texte étendu

**Test** : bisection avec N croissant pour trouver le N minimal qui
reproduit le FN. Puis trace le segment spécifique.

### Cross-token FP chunk pipeline

Le query "function" relaxed matche "finition" dans `wal_record.cpp`.
C'est un FP du chunk pipeline : le falling walk trouve un cross-token
match qui traverse un mauvais boundary. Devrait être filtré par le
word_pos_map post-filter.
