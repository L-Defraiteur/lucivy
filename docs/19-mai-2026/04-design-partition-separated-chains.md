# Design : Séparation des chains par partition

## Motivation

Actuellement le falling walk et le chain builder mélangent les résultats des
trois partitions (0x00, 0x01, 0x02) dans un seul Vec de splits/chains. Ça cause :

1. **Mélange de consumed** — des splits 0x00/0x01 (chunk-level) avec consumed=2
   sont mélangés avec des splits 0x02 (word-level) avec consumed=10. Le filtre
   `best_consumed` doit choisir — risque de perdre des vrais positifs.

2. **Sémantiques d'adjacence incompatibles** — 0x00/0x01 utilisent strict (pos+1),
   0x02 utilise relaxed (seps skippés). Mixer les deux dans un même chain force
   un compromis (ByteOrdered fallback, posmap/bytemap requis partout).

3. **Performance** — les marker entries de 0x00/0x01 génèrent des centaines de
   split candidates inutiles pour le relaxed. Séparer évite ce bruit.

## Architecture proposée

### Deux pipelines parallèles

```
Query "mutexlock" strict_sep=false
  │
  ├── Pipeline CHUNK (partitions 0x00 + 0x01)
  │     falling_walk → cross_token_chain → resolve_strict (pos+1)
  │     Trouve: matches cross-chunk avec seps identiques à la query
  │
  └── Pipeline WORD (partition 0x02)
        falling_walk → cross_word_chain → resolve_relaxed (posmap/bytemap)
        Trouve: matches cross-mot avec seps ignorés
  │
  └── Union + dedup
```

En strict mode (`strict_sep=true`), seul le pipeline CHUNK tourne.
En relaxed mode (`strict_sep=false`), les DEUX tournent, résultats unionés.

### Changements

#### 1. `falling_walk_v3` — split par partition

Actuellement :
```rust
pub fn falling_walk_v3(...) -> Vec<SplitCandidateV3> {
    // walk 0x00 + 0x01
    for &partition in &[SI0_PREFIX, SI_REST_PREFIX] { ... }
    // walk 0x02
    if !strict_separators { ... }
    // Tous dans le même Vec
}
```

Proposé — deux fonctions séparées :
```rust
/// Splits from chunk partitions (0x00 + 0x01). For strict chains.
pub fn falling_walk_chunks(...) -> Vec<SplitCandidateV3> { ... }

/// Splits from word-stripped partition (0x02). For relaxed chains.
pub fn falling_walk_words(...) -> Vec<SplitCandidateV3> { ... }
```

Ou une seule fonction qui retourne deux Vecs :
```rust
pub fn falling_walk_v3(...) -> (Vec<SplitCandidateV3>, Vec<SplitCandidateV3>) {
    // (chunk_splits, word_splits)
}
```

#### 2. `cross_token_chain_v3` — deux builders

```rust
/// Chains from chunk partitions. Each position has ordinals from 0x00/0x01 only.
/// No mixing with word-level ordinals.
pub fn cross_chunk_chain_v3(...) -> Vec<TokenChainV3> { ... }

/// Chains from word-stripped partition. Each position has ordinals from 0x02 only.
/// Longer consumed values (word-level), different adjacency semantics.
pub fn cross_word_chain_v3(...) -> Vec<TokenChainV3> { ... }
```

Le chain builder pour chunks utilise `falling_walk_chunks` en interne.
Le chain builder pour words utilise `falling_walk_words` en interne.

**Pas de filtre `best_consumed` nécessaire** — au sein d'une partition, tous les
splits ont des sémantiques cohérentes. Les consumed values différentes viennent
de tokens différents dans la même partition, pas du mélange inter-partition.

#### 3. `find_literal_v3` — deux resolve paths

```rust
// Single-token matches (unchanged — searches all partitions)
let candidates = fst_candidates_v3(reader, query, anchor_start, strict_separators);
let single = resolve_single_v3(&candidates, resolver, filter_docs);

// Chunk chains — strict adjacency (pos+1)
let chunk_chains = cross_chunk_chain_v3(reader, query);
let chunk_cross = resolve_chains_v3(&chunk_chains, resolver, filter_docs);

// Word chains — relaxed adjacency (posmap/bytemap required)
if !strict_separators {
    let word_chains = cross_word_chain_v3(reader, query);
    if let (Some(pm), Some(bm)) = (posmap, bytemap) {
        let word_cross = resolve_chains_v3_relaxed(&word_chains, resolver, filter_docs, pm, bm);
        results.extend(word_cross);
    }
}

results = union(single, chunk_cross, word_cross);
```

#### 4. `fst_candidates_v3` — inchangé

`fst_candidates` cherche dans toutes les partitions et retourne des candidats
single-token. Pas de chain, pas de mixing. Aucun changement.

### Élimination du filtre best_consumed

Avec la séparation par partition, le filtre `best_consumed` n'est plus nécessaire :

- **Pipeline CHUNK** : tous les splits viennent de 0x00/0x01. Les consumed
  différentes correspondent à des tokens de tailles différentes dans la même
  partition. Ils ont TOUS la même sémantique (chunk-level, overlap vérifié).
  On peut garder le Vec<Vec<u64>> avec tous les ordinals — pas de mélange
  inter-partition.

- **Pipeline WORD** : tous les splits viennent de 0x02. Les consumed différentes
  correspondent à des mots de tailles différentes. Même sémantique word-level.

Le problème original (FP strict) venait de mixer des ordinals 0x01 (consumed=2,
marker "\\x01tr") avec des ordinals 0x01 (consumed=5, long suffix). **C'est
intra-partition** — le fix reste nécessaire.

Attendons — le fix best_consumed reste-t-il nécessaire en intra-partition ?

Oui, si des marker entries 0x01 avec split_at=2 et des full keys 0x01 avec
split_at=5 sont dans les mêmes sub_splits. Mais maintenant que tout est dans la
même partition, le consumed reflète la même sémantique. Le problème était que
0x02 ordinals (word-level) étaient à une mauvaise chain position pour un
remainder 0x01 (chunk-level). En intra-partition, le consumed et le remainder
sont cohérents — on peut garder tous les ordinals.

**MAIS** : le test de la session précédente montrait que les FP strict venaient
de markers 0x01 avec consumed=1 ou 2 mélangés avec des full keys 0x01 consumed=5.
C'est intra-0x01. Le filtre best_consumed éliminait les markers courts.

Sans le filtre, ces FP reviendraient. Il faut donc une solution index-level
pour les markers courts — soit :
- Valider les split candidates via termtexts (vérifier que le texte de l'ordinal
  matche les bytes de la query au sti)
- Ou exiger overlap_consumed > 0 (full key only, pas markers)

L'option `overlap_consumed > 0` fonctionnait pour tous les strict SAUF les
single-token matches (uint64_t, query = own_len exact). Pour ceux-là,
`fst_candidates` les trouve via les markers. Donc si le falling walk exige
overlap_consumed > 0, les single-token sont trouvés par fst_candidates, et
les chains n'utilisent que les full keys (pas les markers). Pas de FP.

### Résumé du plan complet

| Composant | Changement | Objectif |
|-----------|------------|----------|
| `falling_walk_v3` | Split en chunk/word | Plus de mixing inter-partition |
| `cross_chunk_chain_v3` | Builder chunk-only | Strict adjacency, pas de relaxed |
| `cross_word_chain_v3` | Builder word-only | Relaxed adjacency via posmap/bytemap |
| `falling_walk` check_split | `overlap_consumed > 0` pour 0x00/0x01 | Élimine markers courts → plus de FP strict |
| `falling_walk` check_split | `>= 0` (inchangé) pour 0x02 | Les markers 0x02 sont word-level, moins de collisions |
| `find_literal_v3` | Deux resolve paths | Strict pour chunks, relaxed pour words |
| Word-stripped postings | Position = last chunk | Cross-word adjacency correcte |
| `best_consumed` filter | **SUPPRIMÉ** | Plus nécessaire avec la séparation |
| `ByteOrdered` fallback | **DÉJÀ SUPPRIMÉ** | posmap/bytemap requis pour word chains |

### Impact performance

- **Moins de chains** : chaque partition produit ses propres chains, mais pas de
  multiplication croisée. Le nombre total est comparable ou inférieur.
- **Moins de resolve** : pas de chains FP à résoudre puis filtrer.
- **Falling walk plus rapide** : skip markers dans 0x00/0x01 (overlap_consumed > 0)
  = moins de split candidates.

### Fichiers à modifier

| Fichier | Changement |
|---------|------------|
| `briques/fst_walk.rs` | Split falling_walk + cross_chain par partition |
| `briques/fst_walk.rs` | overlap_consumed > 0 pour 0x00/0x01 |
| `briques/composite.rs` | Deux resolve paths dans find_literal_v3 |
| `briques/resolve.rs` | Déjà fait (ByteOrdered supprimé) |
| `collector_v3.rs` | Word-stripped postings last-chunk (doc 03) |

### Tests de validation

1. Strict ground truth : 10/10 maintenu (overlap_consumed > 0 + no mixing)
2. Relaxed ground truth : FP éliminés, FN récupérés (last-chunk + relaxed resolve)
3. Tests unitaires : `x11b`, `x11d`, `f8` doivent passer avec word pipeline
4. `diag_include_vs_inclusive` : à investiguer séparément

## Sfxpost séparé pour partition 0x02

### Pourquoi

Les postings word-stripped ont des sémantiques différentes des postings chunk :

| | Chunk sfxpost (0x00/0x01) | Word sfxpost (0x02) |
|---|---|---|
| position | chunk token_index | **last chunk** token_index |
| byte_from | chunk start | **first chunk** start (mot entier) |
| byte_to | chunk end (own_len) | **last chunk** end |
| span | toujours 1 chunk | N chunks (mot entier) |

Hacker `(doc_id, last_ti, first_bf, last_bt)` dans le même format à 4 champs
que les chunks fonctionne mais c'est fragile — le sens des champs change selon
la partition d'origine.

### Format proposé : WordSfxPost

Fichier séparé `.wordsfxpost` stocké comme registry file (à côté de posmap, bytemap).

```rust
struct WordPostingEntry {
    doc_id: u32,
    first_position: u32,   // token_index du premier chunk (pour dedup/ordering)
    last_position: u32,     // token_index du dernier chunk (pour adjacency check)
    byte_from: u32,         // début du mot dans le texte (premier chunk)
    byte_to: u32,           // fin du mot (dernier chunk, own_len)
    word_span: u16,         // nombre de chunks couverts
}
```

Encodage binaire simple : header (num_ordinals, offset table) + entries packed.
Même pattern que SfxPostV2 mais avec des entries plus larges.

### Construction

Dans `into_data()` :
- Les ordinals word-stripped (`is_word_stripped == true`) ne vont PAS dans
  `content_postings` (le sfxpost chunk)
- Ils vont dans un nouveau `word_postings: Vec<Vec<WordPostingEntry>>`
- La jointure first/last chunk par doc_id se fait ici (comme dans le doc 03)

Dans le DAG build :
- `BuildSfxPostV3Node` : ne traite que les ordinals chunk
- Nouveau `BuildWordSfxPostNode` : traite les ordinals word-stripped
- `AssembleV3Node` : stocke le word sfxpost comme registry file

### Resolve

Le `PostingResolver` trait a actuellement :
```rust
fn resolve(&self, ordinal: u64) -> Vec<PostingEntry>;
```

On ajoute un resolver séparé pour les word postings :
```rust
fn resolve_word(&self, ordinal: u64) -> Vec<WordPostingEntry>;
```

Ou mieux : le resolve word pipeline utilise son propre reader directement,
pas le trait générique. Le `resolve_chains_v3_relaxed` reçoit le word sfxpost
reader en paramètre.

### Impact sur les autres composants

- **PosMap** : inchangé — construit depuis le sfxpost chunk. Les word ordinals
  n'apparaissent pas dans le posmap (correct : le posmap indexe les chunks).
- **ByteMap** : idem — les word ordinals ont leur bytemap propre (dans le
  word sfxpost) ou partagent le bytemap chunk via le first/last chunk ordinal.
- **TermTexts** : les word-stripped sont déjà dans le termtexts v3 (ajouté
  dans la session précédente). Pas de changement.

## Vision long terme : fichiers par partition

À terme, chaque partition pourrait avoir ses propres fichiers :

```
segment/
  content.sfx           # FST commun (3 partitions dans un seul trie)
  content.sfxpost       # postings chunk (0x00 + 0x01)
  content.wordsfxpost   # postings word (0x02) ← nouveau
  content.termtexts     # textes + méta (toutes partitions)
  content.posmap        # position → ordinal chunk
  content.bytemap       # ordinal chunk → byte bitmap
  content.wordposmap    # (doc, position) → word_id
```

Future évolution possible :
```
segment/
  content.chunk.sfx       # FST chunk (0x00 + 0x01)
  content.chunk.sfxpost   # postings chunk
  content.word.sfx        # FST word (0x02)
  content.word.sfxpost    # postings word
  ...
```

Mais cette séparation FST n'est PAS nécessaire maintenant — le FST commun avec
préfixes de partition (0x00, 0x01, 0x02) fonctionne bien. La séparation sfxpost
suffit pour cette itération.

## Ordre d'implémentation

1. **WordSfxPost format** : writer + reader (nouveau module `word_sfxpost.rs`)
2. **collector_v3 into_data()** : séparer word postings du content_postings
3. **sfx_dag_v3** : nouveau node `BuildWordSfxPostNode`, registry file
4. **falling_walk** : split chunk/word, overlap_consumed > 0 pour chunks
5. **cross_chain builders** : chunk-only et word-only
6. **find_literal_v3** : deux resolve paths
7. **Supprimer best_consumed filter**
8. **Tests** : fixtures avec word sfxpost, ground truth

| Fichier | Changement |
|---------|------------|
| `src/suffix_fst/word_sfxpost.rs` | NOUVEAU — WordSfxPost writer/reader |
| `src/suffix_fst/mod.rs` | `pub mod word_sfxpost;` |
| `src/suffix_fst/collector_v3.rs` | `word_postings` séparé, last-chunk jointure |
| `src/indexer/sfx_dag_v3.rs` | BuildWordSfxPostNode, registry file |
| `src/suffix_fst/briques/fst_walk.rs` | Split falling_walk, overlap_consumed > 0 |
| `src/suffix_fst/briques/composite.rs` | Deux resolve paths |
| `src/suffix_fst/briques/resolve.rs` | Word resolve avec WordSfxPost reader |
| `src/suffix_fst/briques/orchestrator.rs` | Passer word resolver |
| `src/suffix_fst/briques/integration_tests.rs` | Fixtures word sfxpost |
| `src/query/contains_query_v3.rs` | Charger word sfxpost |
| `src/query/fuzzy_query_v3.rs` | Charger word sfxpost |
