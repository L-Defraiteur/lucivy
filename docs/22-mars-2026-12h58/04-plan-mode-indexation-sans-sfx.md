# Doc 04 — Plan : mode d'indexation sans SFX

Date : 22 mars 2026

## Motivation

Le suffix FST (.sfx + .sfxpost) est le composant le plus coûteux de l'indexation :
- **Temps** : génération de tous les suffixes de chaque terme → O(tokens × avg_token_len)
- **Espace** : le .sfx est souvent 3-5x plus gros que le term dict FST
- **Merge** : le merge SFX (sfx_merge.rs, sfx_dag.rs) est la partie la plus lente du segment merge

Si l'utilisateur n'a besoin que de term, phrase, fuzzy, regex, parse (queries standard
tantivy), le SFX n'apporte rien. Indexation plus rapide, index plus petit.

## Config proposée

```json
{
  "fields": [
    {"name": "content", "type": "text", "stored": true}
  ],
  "sfx": false
}
```

Default : `sfx: true` (backward compat).

## Points d'impact

### 1. SchemaConfig

```rust
pub struct SchemaConfig {
    // ...
    /// Enable suffix FST for contains/startsWith queries. Default: true.
    pub sfx: Option<bool>,
}
```

Le flag est propagé dans `SegmentWriter` et `Merger`.

### 2. SegmentWriter — skip SfxCollector

**Fichier** : `src/indexer/segment_writer.rs`

```rust
// Ligne 125 : construction des collectors
sfx_collectors: {
    if schema_config.sfx.unwrap_or(true) {
        // ... créer les SfxCollector par field ...
    } else {
        HashMap::new()  // ← pas de collectors = pas de SFX
    }
}
```

Si `sfx_collectors` est vide :
- Le `SfxTokenInterceptor` n'est pas wrappé autour du token stream
- `begin_value()` / `feed_token()` / `end_value()` ne sont jamais appelés
- `finalize()` ne génère aucun .sfx / .sfxpost
- `sfx_field_ids` reste vide → pas de manifest

**Impact** : la tokenisation est plus simple (pas de double path), chaque `add_document`
est plus rapide.

### 3. Merger — skip SFX merge

**Fichiers** : `src/indexer/merger.rs`, `src/indexer/sfx_merge.rs`, `src/indexer/sfx_dag.rs`

Le merger vérifie `segment_meta.sfx_field_ids()`. Si aucun segment source n'a de SFX
fields, le merge SFX est skip.

```rust
// Dans merger.rs
let sfx_field_ids = if schema_config.sfx.unwrap_or(true) {
    merge_sfx_fields(...)  // build + merge SFX pour le nouveau segment
} else {
    vec![]  // skip
};
```

**Impact** : le segment merge est significativement plus rapide (le SFX merge est
souvent 50-70% du temps total de merge).

### 4. Segment merge DAG — skip SFX nodes

**Fichier** : `src/indexer/merge_dag.rs`

Le DAG de merge a des noeuds `build_fst`, `copy_gapmap`, `merge_sfxpost`.
Si `sfx: false`, ces noeuds ne sont pas ajoutés au DAG.

### 5. Query validation — erreur claire

**Fichier** : `lucivy_core/src/query.rs`

```rust
fn build_contains_query(...) -> Result<Box<dyn Query>, String> {
    // Vérifier que le SFX existe pour ce field
    // (déjà fait implicitement : SuffixContainsQuery::scorer() retourne
    //  EmptyScorer si pas de .sfx file)
}
```

Actuellement, `SuffixContainsWeight::scorer()` retourne un `EmptyScorer` si
le .sfx file n'existe pas. C'est silencieux — 0 résultats sans erreur.

**Mieux** : retourner une erreur explicite :

```rust
if sfx_data.is_none() {
    return Err(LucivyError::SystemError(
        "contains/startsWith queries require SFX indexing (sfx: true in schema config)".into()
    ));
}
```

### 6. Propagation du flag

Le flag doit être accessible dans :
- `SegmentWriter::new()` — pour décider de créer les SfxCollectors
- `IndexWriter` → `SegmentWriter` — le writer crée les segment writers
- `IndexMerger` — pour décider de merger les SFX
- `LucivyHandle` — pour stocker le flag et le propager

Options :
- A. Stocker dans `IndexMeta` (persiste sur disque, visible à la réouverture)
- B. Stocker dans `IndexSettings` (persiste dans le meta.json)
- C. Stocker dans le `SchemaConfig` qui est déjà sérialisé dans `_config.json`

**Recommandation** : Option C — le `SchemaConfig` est déjà persisté par
`LucivyHandle::create()` et lu par `LucivyHandle::open()`. Le flag est
naturellement disponible partout.

## Ce qui reste fonctionnel sans SFX

| Query type | sfx:true | sfx:false |
|-----------|----------|-----------|
| term | ✓ | ✓ |
| phrase | ✓ | ✓ |
| fuzzy (top-level) | ✓ | ✓ |
| regex (top-level) | ✓ | ✓ |
| parse | ✓ | ✓ |
| phrase_prefix | ✓ | ✓ |
| disjunction_max | ✓ | ✓ |
| boolean | ✓ | ✓ |
| contains | ✓ | ✗ erreur |
| startsWith | ✓ | ✗ erreur |
| contains + regex | ✓ | ✗ erreur |
| contains + fuzzy | ✓ | ✗ erreur |

## Gains attendus

| Métrique | sfx:true (actuel) | sfx:false (estimé) |
|----------|------------------|-------------------|
| Indexation 90K | ~200s | ~30-50s |
| Taille index/shard | ~500MB | ~100-150MB |
| Merge time | ~15s/merge | ~3-5s/merge |
| term/phrase query | 0.2-5ms | 0.2-5ms (identique) |

## Étapes d'implémentation

1. Ajouter `sfx: Option<bool>` à `SchemaConfig`
2. Propager dans `SegmentWriter` : skip SfxCollector si `sfx=false`
3. Propager dans `Merger` / merge DAG : skip SFX nodes
4. Ajouter erreur explicite dans `SuffixContainsWeight::scorer()` si pas de .sfx
5. Tests : indexer sans SFX, vérifier term/phrase/fuzzy/regex OK, vérifier contains erreur
6. Bench comparatif : indexation time + index size avec/sans SFX

## Risques

- **Bindings** : les bindings (cxx, wasm, nodejs, python) devront passer le flag.
  Default `true` = backward compat, pas de casse.
- **Highlights** : les highlights term/phrase utilisent le standard inverted index
  (fix de cette session), pas d'impact.
- **startsWith via SFX** : actuellement startsWith utilise `SuffixContainsQuery::with_prefix_only()`.
  Sans SFX, on pourrait fallback sur `AutomatonPhraseQuery` (prefix walk sur le term dict),
  mais les résultats seraient légèrement différents (token-level vs byte-level).
  Pour l'instant : erreur claire.
