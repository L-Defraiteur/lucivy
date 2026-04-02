# 06 — Plan : Registry v2 — MergeStrategy + OrMergeNode générique

Date : 2 avril 2026

## Problème

Le sepmap en `DerivedWithDeps` walk le gapmap+posmap sérialisés → O(n_docs × n_tokens × lookup) → **32 secondes** sur 45K tokens / 200 docs en WASM. Inacceptable.

Le sepmap devrait être pré-construit par le collector (O(1) par token) au segment initial, et OR-mergé au merge (comme sibling). Mais l'abstraction actuelle ne supporte que `Primary` (node DAG externe), `Derived` (events), et `DerivedWithDeps` (post-traitement).

## Solution : MergeStrategy

### Nouveau trait

```rust
pub trait SfxIndexFile: Send {
    fn id(&self) -> &'static str;
    fn extension(&self) -> &'static str;

    // ── Segment initial ──────────────────────────────────────────
    
    /// Si true, le collector pré-construit cet index et le passe comme donnée.
    /// Si false, construit via on_token/on_posting pendant la single-pass.
    fn prebuilt_by_collector(&self) -> bool { false }

    // ── Merge ────────────────────────────────────────────────────
    
    fn merge_strategy(&self) -> MergeStrategy { MergeStrategy::EventDriven }

    // ── Events (pour EventDriven) ────────────────────────────────

    fn on_token(&mut self, _ord: u32, _text: &str) {}
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _pos: u32,
                  _byte_from: u32, _byte_to: u32) {}

    // ── OR-merge (pour OrMergeWithRemap) ─────────────────────────
    
    /// Merge bitmaps/data from source segments with ordinal remapping.
    /// `token_to_new_ord` maps token text → new ordinal in merged segment.
    /// Called by the generic OrMergeNode.
    fn merge_from_sources(
        &mut self,
        _sources: &[Option<&[u8]>],
        _source_termtexts: &[Option<&[u8]>],
        _token_to_new_ord: &dyn Fn(&str) -> Option<u32>,
    ) {}

    // ── Output ───────────────────────────────────────────────────

    fn serialize(&self) -> Vec<u8>;
}

pub enum MergeStrategy {
    /// Built from sfxpost + tokens via on_token/on_posting events.
    /// Used by: posmap, bytemap, termtexts, freqmap.
    EventDriven,
    
    /// OR-merge source data with ordinal remapping via token text.
    /// merge_from_sources() is called with source bytes + remapping.
    /// Used by: sibling, sepmap.
    OrMergeWithRemap,
    
    /// Managed by a dedicated DAG node (too complex for generic merge).
    /// Used by: sfxpost (N-way merge), gapmap (copy + doc remap).
    ExternalDagNode,
}
```

### Ce que chaque index déclare

| Index | prebuilt_by_collector | merge_strategy | Segment initial | Merge |
|-------|----------------------|----------------|-----------------|-------|
| sfxpost | false | ExternalDagNode | DAG BuildSfxPostNode | DAG MergeSfxpostNode |
| gapmap | true | ExternalDagNode | collector → passe data | DAG CopyGapmapNode |
| sibling | true | OrMergeWithRemap | collector → passe data | OrMergeNode générique |
| sepmap | true | OrMergeWithRemap | collector → passe data | OrMergeNode générique |
| posmap | false | EventDriven | on_posting | on_posting |
| bytemap | false | EventDriven | on_token | on_token |
| termtexts | false | EventDriven | on_token | on_token |
| freqmap | false | EventDriven | on_posting | on_posting |

### OrMergeNode générique

Un seul node DAG réutilisable qui :
1. Itère les index `OrMergeWithRemap`
2. Pour chaque index, charge les sources depuis les segment readers
3. Charge les termtexts sources (pour le reverse mapping)
4. Construit le `token_to_new_ord` closure depuis les merged tokens
5. Appelle `merge_from_sources()`
6. Sérialise et écrit

```rust
struct OrMergeNode {
    ctx: Arc<SfxContext>,
    field: Field,
}

impl Node for OrMergeNode {
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let tokens = nctx.input("tokens")...;
        
        // Build token → new ordinal map
        let token_to_ord: HashMap<&str, u32> = tokens.iter()
            .enumerate()
            .map(|(i, t)| (t.as_str(), i as u32))
            .collect();
        
        // Load source data per segment
        let source_termtexts = load_per_segment("termtexts");
        
        let mut results: Vec<(String, Vec<u8>)> = Vec::new();
        
        for mut index in all_indexes() {
            if !matches!(index.merge_strategy(), MergeStrategy::OrMergeWithRemap) {
                continue;
            }
            let sources = load_per_segment(index.id());
            index.merge_from_sources(&sources, &source_termtexts, 
                &|text| token_to_ord.get(text).copied());
            let data = index.serialize();
            if !data.is_empty() {
                results.push((index.extension().to_string(), data));
            }
        }
        
        nctx.set_output("or_merged", PortValue::new(results));
        Ok(())
    }
}
```

### Impact sur build_sfx_dag (merge)

```
collect_tokens ──┬── build_fst ─────────────────────────┐
                 ├── copy_gapmap ── validate_gapmap ─────┤
                 ├── merge_sfxpost ── validate_sfxpost ──┤
                 └── or_merge ───────────────────────────┼── write_sfx
                                                         │
                                              build_derived_indexes
```

`MergeSiblingLinksNode` est remplacé par `OrMergeNode` qui gère
sibling + sepmap + tout futur index OrMergeWithRemap.

### Impact sur segment initial

Le collector pré-construit sibling + sepmap (déjà le cas).
`into_data()` inclut les données sérialisées.
`AssembleSfxNode` les passe directement à l'écriture.

### Impact sur build_derived_indexes

Plus de DerivedWithDeps. La fonction ne gère que les EventDriven.
Le sepmap n'est plus dans la boucle events.

### Étapes

1. Ajouter `MergeStrategy` et les nouvelles méthodes au trait
2. Implémenter `merge_from_sources()` pour SiblingIndex et SepMapIndex
3. Créer `OrMergeNode` dans sfx_dag.rs
4. Remettre le sepmap pré-construit dans collector `into_data()`
5. Brancher OrMergeNode dans build_sfx_dag (merge)
6. Passer les données pré-construites dans AssembleSfxNode (segment initial)
7. Supprimer MergeSiblingLinksNode (remplacé par OrMergeNode)
8. Supprimer DerivedWithDeps du trait
9. Tests

### Performance attendue

- Segment initial : O(1) par token (collector pré-construit, comme avant)
- Merge : O(n_terms × n_source_segments) pour l'OR-merge (linéaire, pas quadratique)
- Plus de walk gapmap×posmap : **0ms au lieu de 32s**
