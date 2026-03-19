# Doc 17 — Plan : merge_sfx en sous-DAG

Date : 19 mars 2026

## Pourquoi

`merge_sfx` dans `merger.rs` est une fonction monolithique de ~200 lignes
qui fait 6 étapes séquentielles. C'est là que le bug gapmap se produit.
Aucune observabilité interne — on sait juste que "sfx a pris X ms".

En la découpant en nœuds DAG :
- Chaque étape est chronométrée individuellement
- On peut tapper les edges pour voir les données intermédiaires
- Le validate est un nœud qui émet des métriques (errors_found)
- Si le gapmap bug revient, on voit EXACTEMENT quelles bytes sont corrompues

## Architecture actuelle

```rust
fn merge_sfx(&self, serializer, doc_mapping) -> Result<()> {
    for field in sfx_fields {
        // 1. Collect tokens from all readers' FSTs
        // 2. Build new FST (SuffixFstBuilder)
        // 3. Copy gapmap per doc in merge order
        // 4. Merge sfxpost (posting entries + doc_id remapping)
        // 5. Validate gapmap (ajouté cette session)
        // 6. Write .sfx + .sfxpost files
    }
}
```

Problème Rust : `&self` (IndexMerger) et `&mut serializer` sont empruntés.
On ne peut pas les donner à des nœuds DAG séparés.

## Solution : fonctions standalone

Refactorer chaque étape en fonction standalone qui prend ses inputs
par value/ref et retourne ses outputs :

```rust
// Chaque fonction est indépendante, pas de &mut self
fn collect_sfx_tokens(readers, field) → Vec<(String, u64, ParentEntry)>
fn build_sfx_fst(tokens) → (Vec<u8>, Vec<u8>)  // fst_data, parent_list_data
fn copy_sfx_gapmap(readers, field, doc_mapping) → Vec<u8>
fn merge_sfx_postings(readers, field, doc_mapping, tokens) → Option<Vec<u8>>
fn validate_sfx_gapmap(gapmap_data) → Vec<GapMapError>
fn write_sfx_files(serializer, field, fst, parents, gapmap, sfxpost)
```

## DAG pour un champ

```
collect_tokens ──┬── build_fst ──────────────┐
                 └── copy_gapmap ── validate ─┼── write_sfx
                 └── merge_sfxpost ───────────┘
```

Note : `build_fst`, `copy_gapmap` et `merge_sfxpost` sont INDÉPENDANTS
(ils lisent les mêmes readers mais ne se modifient pas). Ils peuvent
tourner en parallèle !

C'est le vrai avantage du DAG ici : le FST build et le gapmap copy
peuvent tourner sur des threads séparés du pool.

## Types entre nœuds (PortValues)

| Edge | Type | Taille typique |
|------|------|----------------|
| collect → build_fst | `Vec<(String, u64, ParentEntry)>` | ~10K entries |
| collect → merge_sfxpost | same (partagé via fan-out) | Arc clone |
| build_fst → write | `(Vec<u8>, Vec<u8>)` | ~100KB |
| copy_gapmap → validate | `Vec<u8>` | ~1MB |
| validate → write | `Vec<u8>` (passthrough si OK) | ~1MB |
| merge_sfxpost → write | `Option<Vec<u8>>` | ~500KB |

Tous des types concrets, Send + Sync, parfaitement passables via PortValue.

## Fichiers modifiés

### merger.rs (~200 lignes changées)
- Extraire les 6 étapes de `merge_sfx` en fonctions `pub(crate)` standalone
- `merge_sfx` devient un thin wrapper qui appelle les 6 fonctions séquentiellement
  (backward compat pour les tests existants)

### Nouveau : sfx_dag.rs (~150 lignes)
- 6 nœuds : CollectTokensNode, BuildFstNode, CopyGapmapNode,
  MergeSfxpostNode, ValidateGapmapNode, WriteSfxNode
- `build_sfx_dag(readers, field, doc_mapping, serializer)` factory
- Appelé par MergeNode.step_sfx() au lieu de merger.merge_sfx()

### merge_state.rs (~10 lignes changées)
- step_sfx() construit et exécute le sfx DAG au lieu d'appeler merge_sfx

## Observabilité résultante

```
merge_0  120ms
  init_ms=1  postings_ms=15  store_ms=3  fast_fields_ms=2  close_ms=5
  sfx_ms=94
    sfx.collect_tokens  12ms  unique_tokens=8500
    sfx.build_fst       35ms  fst_bytes=102400
    sfx.copy_gapmap     18ms  docs=5000 bytes=1048576
    sfx.merge_sfxpost   22ms  terms=8500 postings=45000
    sfx.validate         1ms  errors=0
    sfx.write            6ms  sfx_bytes=204800 sfxpost_bytes=512000
```

Si le gapmap bug se reproduit :
```
    sfx.validate         0ms  errors=3 ← BUG ICI
      [ERR] gapmap doc_42: too few gaps: found 2, tokens=5, values=1
      [ERR] gapmap doc_99: doc_data too short: 2 bytes (min 3)
```

Et on peut tapper l'edge `copy_gapmap → validate` pour capturer
les bytes brutes et investiguer offline.

## Estimation

```
merger.rs refactor    ~200 lignes modifiées (extraire fonctions)
sfx_dag.rs            ~150 lignes (6 nœuds + factory)
merge_state.rs        ~10 lignes (step_sfx → sfx DAG)

Total : ~360 lignes changées
Net : probablement +50 lignes (plus structuré, moins monolithique)
```
