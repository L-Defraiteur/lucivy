# Knowledge Dump — SFX v3 complet (24 mai 2026)

## Comment lancer et diagnostiquer

### Ground truth test

Le test indexe 500 fichiers C++ du clone rag3db et compare v3 vs grep naïf.

```bash
# Prérequis : cloner le repo de test
git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git /tmp/rag3db-bench

# Lancer le ground truth (debug, ~90s)
cargo test -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains

# En release (~5s, beaucoup plus rapide)
cargo test -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains --release

# Rapport détaillé écrit dans :
cat /tmp/v3_ground_truth_report.txt
```

### Debug ciblé sur une query

```bash
# Trace détaillée des chains/candidates/splits pour une query spécifique
V3_DEBUG_QUERY=struct cargo test -p lucivy-core --test test_sfx_v3_ground_truth debug_struct_fp
# → trace dans /tmp/v3_debug_trace.txt

# Debug struct FP avec tokenisation des zones de highlight
cargo test -p lucivy-core --test test_sfx_v3_ground_truth debug_struct_fp -- --nocapture
# → rapport dans /tmp/v3_debug_struct.txt
```

### Diagnostic de l'indexation (multi-parent FST)

```bash
# Log les clés FST multi-parent avec ordinals distincts
V3_DIAG_BUILD=1 cargo test -p lucivy-core --test test_sfx_v3_ground_truth debug_struct_fp
# → /tmp/v3_diag_build.txt (stats de collision markers)
```

### Tests unitaires lib

```bash
# Tous les tests (debug, ~2-3 min)
cargo test --lib

# Tests spécifiques
cargo test --lib x11b_stripped_traverse_pure_sep -- --nocapture
cargo test --lib test_find_literal_sep_skip -- --nocapture

# Tests par module
cargo test --lib suffix_fst::briques::integration_tests
cargo test --lib suffix_fst::briques::fst_walk
cargo test --lib suffix_fst::word_sfxpost
```

### État actuel des tests

```
Lib tests : 1421 pass, 3 fail (tests diag sans maps — à migrer)
Ground truth : 13/15 pass (10/10 strict, 3/5 relaxed)
  - 6 FN relaxed = différence sémantique grep vs v3 (by design)
```

## Comment ajouter un nouveau fichier d'index

Pour ajouter un nouveau fichier d'index par segment (comme posmap, bytemap, word_sfxpost) :

### 1. Créer le module writer/reader

```rust
// src/suffix_fst/mon_index.rs
pub struct MonIndexWriter { ... }
impl MonIndexWriter {
    pub fn new() -> Self { ... }
    pub fn add(&mut self, ...) { ... }
    pub fn finish(self) -> Vec<u8> { ... } // sérialise
}

pub struct MonIndexReader<'a> { data: &'a [u8] }
impl<'a> MonIndexReader<'a> {
    pub fn open(data: &'a [u8]) -> Option<Self> { ... }
    pub fn query(&self, ...) -> ... { ... }
}
```

### 2. Ajouter l'entry dans index_registry

```rust
// src/suffix_fst/mon_index.rs — ajouter à la fin
pub struct MonIndexEntry;
impl crate::suffix_fst::index_registry::SfxIndexFile for MonIndexEntry {
    fn id(&self) -> &'static str { "mon_index" }
    fn extension(&self) -> &'static str { "mon_index" }  // nom du fichier sur disque
    fn merge_strategy(&self) -> MergeStrategy {
        MergeStrategy::ExternalDagNode  // si construit par le DAG
        // ou MergeStrategy::EventDriven si construit via on_token/on_posting
    }
    fn on_token(&mut self, _ord: u32, _text: &str) {}
    fn on_posting(&mut self, _ord: u32, _doc: u32, _ti: u32, _bf: u32, _bt: u32) {}
    fn serialize(&self) -> Vec<u8> { Vec::new() }
}
```

### 3. Enregistrer dans all_indexes()

```rust
// src/suffix_fst/index_registry.rs
pub fn all_indexes() -> Vec<Box<dyn SfxIndexFile>> {
    vec![
        // ... existants ...
        Box::new(super::mon_index::MonIndexEntry),  // ← ajouter
    ]
}
```

**CRITIQUE** : sans cette ligne, le segment reader ne chargera JAMAIS le fichier.
`sfx_index_file("mon_index", field)` retournera `None`.

### 4. Déclarer le module

```rust
// src/suffix_fst/mod.rs
pub mod mon_index;
```

### 5. Construire les données dans le pipeline d'indexation

**Option A — Prebuilt (données construites dans collector/into_data)** :

```rust
// collector_v3.rs — ajouter au SfxCollectorDataV3
pub struct SfxCollectorDataV3 {
    // ...
    pub mon_index: Vec<u8>,  // données sérialisées
}

// sfx_dag_v3.rs — AssembleV3Node, ajouter aux registry_files
derived.push(("mon_index".to_string(), data.mon_index.clone()));
```

**Option B — EventDriven (construit via on_token/on_posting)** :

Implémenter `on_token` et `on_posting` dans le trait SfxIndexFile.
Le builder appelle automatiquement ces méthodes dans `build_derived_indexes_v3`.

### 6. Charger côté query

```rust
// src/query/contains_query_v3.rs (ou fuzzy_query_v3.rs)
let mon_index_bytes = seg_reader.sfx_index_file("mon_index", self.field)
    .and_then(|fs| fs.read_bytes().ok())
    .map(|b| b.as_ref().to_vec());
let mon_index_reader = mon_index_bytes.as_ref()
    .and_then(|b| MonIndexReader::open(b));
```

### 7. Passer au pipeline query

Ajouter le paramètre aux fonctions : `contains_v3`, `fuzzy_v3`,
`find_literal_v3`, etc. Propager jusqu'au resolve qui l'utilise.

### 8. Tests

Dans les tests d'intégration (`integration_tests.rs`) et les tests briques
(`composite.rs`, `orchestrator.rs`) :
- Ajouter le champ au `TestIndex` struct
- Le construire dans `build()` / `build_index()`
- Le passer aux helpers `query_contains`, `query_contains_hl`, `query_fuzzy`

### Checklist

- [ ] Module writer/reader avec tests roundtrip
- [ ] SfxIndexFile impl avec id() et extension()
- [ ] Enregistré dans all_indexes()
- [ ] Déclaré dans mod.rs
- [ ] Construit dans into_data() ou via EventDriven
- [ ] Ajouté aux registry_files dans AssembleV3Node
- [ ] Chargé dans contains_query_v3 / fuzzy_query_v3
- [ ] Paramètre propagé dans le pipeline query
- [ ] Tests mis à jour (TestIndex, build_index, helpers)

## Tokenizer : equal_chunk

**Fichier** : `src/tokenizer/equal_chunk.rs`

Le texte est découpé en **segments** (runs de content chars) séparés par des
**seps** (runs de non-content chars). `is_content_char(c)` = `c.is_ascii_alphanumeric() || !c.is_ascii()` (alphanum + Unicode).

Chaque segment est découpé en **chunks** de taille quasi-égale :
- `n = ceil(segment_len / max_token)` chunks
- `base = segment_len / n`, `leftover = segment_len % n`
- Les premiers `leftover` chunks ont `base+1` chars, les autres `base`
- `max_token = 8` par défaut (DEFAULT_MAX_TOKEN)

Chaque chunk a :
- `content_len` : nombre de bytes content
- `sep_len` : nombre de bytes sep APRÈS le content (trailing sep du segment)
- `own_len = content_len + sep_len`
- `word_id` : identifiant du mot (segment) dans la valeur
- `is_word_start` : true si c'est le premier chunk du mot

**Overlap** : chaque chunk est étendu avec les 2 premiers bytes du chunk suivant.
`DEFAULT_OVERLAP = 2`. Le texte étendu = `chunk_text + overlap_bytes`.
L'overlap garantit que les trigrammes cross-chunk sont dans le FST.

Exemple : "mutex_lock" →
- Segment "mutex" (5 chars), sep "_" (1 char)
- Segment "lock" (4 chars)
- Chunk 0 : content="mutex", sep="_", overlap="lo" → extended "mutex_lo"
- Chunk 1 : content="lock", sep="" → extended "lock"

## Collector v3

**Fichier** : `src/suffix_fst/collector_v3.rs` (1119 lignes)

### Interning

`intern_extended(text, meta) → intern_id` : HashMap text → u32.
Chaque texte étendu unique a un seul intern_id. Le meta (own_len, sep_len,
overlap_len, is_word_start, word_id, is_word_stripped) est stocké pour le
premier interning.

### Postings

`token_postings[intern_id]` = Vec<(doc_id, token_index, byte_from, byte_to)>.
Ajouté dans `add_value()` pour chaque chunk.

### Word-stripped entries

Pour chaque mot (group de chunks avec le même word_id), on construit :
- `word_content` : concaténation des content bytes de tous les chunks (pas de seps)
- `content_overlap` : premiers 2 bytes du contenu du mot SUIVANT
- `first_chunk_intern_ord` : intern_id du premier chunk
- `last_chunk_intern_ord` : intern_id du dernier chunk

Les word-stripped sont internés séparément (`is_word_stripped=true`).
Ils n'ont PAS de postings propres dans token_postings — leurs postings
sont dans le WordSfxPost (séparé du chunk sfxpost).

Tail entries : pour les mots très longs (> MAX_SUFFIX_INDEX + max_token = 264 bytes),
un tail entry couvre les derniers max_token bytes.

### into_data() — Extended ordinals

Chaque intern_id non-word-stripped → son propre final ordinal dans un BTreeMap
(ordre alphabétique = ordre ordinal). Pas de groupement par `text[..own_len]`.

Les word-stripped avec `is_word_stripped=true` → leur propre ordinal dans le
BTreeMap, avec postings VIDES dans content_postings (leurs postings sont dans
le WordSfxPost).

**WordSfxPost construction** (dans into_data) :
Pour chaque word-stripped entry :
- Mono-chunk : position et bytes du même chunk
- Multi-chunk : jointure par doc_id entre premier et dernier chunk
  - `first_position, byte_from` ← premier chunk
  - `last_position, byte_to` ← dernier chunk
  - Agrège les postings de toutes les variantes d'overlap via `content_key_to_interns`

## Builder v3

**Fichier** : `src/suffix_fst/builder_v3.rs` (847 lignes)

### add_token(extended_token, ordinal, own_len, sep_len, overlap_len, is_word_start)

Pour chaque token, génère des entrées FST pour tous les suffix indices (SI) :

**Partitions** :
- `0x00` (SI=0) : clé = `\x00` + lowered_text. Pour anchor_start.
- `0x01` (SI>0) : clé = `\x01` + lowered_suffix. Pour contains anywhere.

**Pour chaque SI** :
1. **Full key** : `partition + suffix_bytes` (content + sep + overlap). Le nœud
   final au bout de la clé a le parent metadata (ordinal, sti, own_len, etc.)
2. **Marker entry** : `partition + suffix_bytes[..split_at]` où `split_at = own_len - si`.
   Tronqué à la frontière own_len. Rend le nœud FST final à cette position
   pour que le falling walk détecte les splits même quand la query diverge
   dans l'overlap.

Le texte est **lowercased** avant insertion (`extended_token.to_lowercase()`).

### add_word_stripped(word_content, content_overlap, ordinal, ...)

Partition `0x02`. Pour chaque SI de 0 à content_len :
- Full key : `\x02` + content_suffix + overlap_bytes
- **Pas de marker entries** dans 0x02 (contrairement à 0x00/0x01)
- Skip si `first_sep_len == 0` (pas de stripping nécessaire)

### build() — FST construction

1. Trie toutes les entries par clé (alphabétique)
2. Dedup par (clé, ordinal, sti)
3. Groupe les clés identiques en **multi-parent** (même clé FST, parents différents)
4. Encode : single-parent dans la valeur FST directement, multi-parent via une
   output table (offset dans un blob séparé)
5. Construit le FST via `MapBuilder`

**Multi-parent** : une même clé FST lowercased peut venir de tokens différents
(casse différente, même suffixe lowered). Chaque parent a son propre ordinal,
sti, own_len. Le reader `decode_parents(value)` retourne TOUS les parents.

**Diag V3_DIAG_BUILD** : log les clés avec multi-parents distincts. Jusqu'à
11K parents pour une seule clé (markers courts comme "\x01s").

## Falling Walk

**Fichier** : `src/suffix_fst/briques/fst_walk.rs`

### Concept

Le falling walk parcourt le FST byte par byte en suivant les bytes de la
query. À chaque nœud final rencontré, il récupère les parents et vérifie
si un split point a été atteint.

**Split point** : position dans le token où `own_len - sti` bytes ont été
consommés. Au-delà, les bytes sont dans l'overlap zone (bytes du token suivant).

```
Token "mutex_lo" (own_len=6, sti=0, overlap="lo")
       m u t e x _ l o
       └───own_len──┘└ov┘
       split_byte = 6

Query "mutex_lock" : walk m,u,t,e,x,_,l,o
  → à position 6 : split_byte atteint, overlap_consumed=2
  → SplitCandidate { consumed=6, remainder="ck", overlap_validated=2 }
```

### walk_partition

Fonction interne. Walk byte par byte dans une partition du FST :
```rust
for (i, &byte) in query_bytes.iter().enumerate() {
    if !node.has_transition(byte) { break; }
    node = follow(byte);
    if node.is_final() {
        parents = decode_parents(value);
        for parent in parents {
            check_split(parent, prefix_len=i+1);
        }
    }
}
```

### falling_walk_chunks (partitions 0x00 + 0x01)

Pour les chunks. `check_split` : `prefix_len >= split_byte` → split trouvé.
`split_byte = own_len - sti`. `overlap_consumed = prefix_len - split_byte`.

Les marker entries rendent les nœuds finaux à split_byte, permettant de
détecter les splits même quand l'overlap diverge de la query.

### falling_walk_words (partition 0x02)

Pour les word-stripped. `check_split` : `prefix_len >= split_byte` où
`split_byte = content_len - sti`. Pas de markers additionnels dans 0x02.

### cross_chunk_chain_v3 / cross_word_chain_v3

Construit des chains à partir des splits. Pour chaque split initial :
1. Calcule le remainder (query bytes après le split)
2. Cherche le remainder via `fst_candidates(remainder, anchor_start=true)`
3. Si pas trouvé, fait un sous-falling walk sur le remainder
4. Répète jusqu'à consommer toute la query (max depth 8)

**best_consumed filter** (chunk pipeline seulement) : ne garde que les sub_splits
avec le même consumed que le best. Empêche le mélange d'ordinals de markers
courts avec des ordinals de full keys.

**Pas de best_consumed** pour le word pipeline : les word-stripped ont des
préfixes uniques, le mélange de consumed est rare et non-problématique.

## fst_candidates

**Fichier** : `src/suffix_fst/briques/fst_walk.rs`

Range query sur le FST : `fst.range().ge(prefix + query).lt(prefix + query+1)`.
Retourne TOUTES les clés qui commencent par la query dans les partitions sélectionnées.

**Partitions consultées** :
- `anchor_start=true, strict_sep=true` → `[0x00]`
- `anchor_start=true, strict_sep=false` → `[0x00, 0x02]` ← fix session 5
- `anchor_start=false, strict_sep=true` → `[0x00, 0x01]`
- `anchor_start=false, strict_sep=false` → `[0x00, 0x01, 0x02]`

**Matching de préfixes de mots** : la range query retourne naturellement les
clés plus longues. "function" trouve "functionsXY" en 0x02. Pas besoin de
markers spéciaux pour le prefix matching.

## Resolve

**Fichier** : `src/suffix_fst/briques/resolve.rs` (637 lignes)

### resolve_single_v3

Pour les fst_candidates (single-token matches). Chaque candidat a un ordinal.
Résout via PostingResolver. byte_to = byte_from + own_len - sep_len.

### resolve_chains_v3 (strict, chunk pipeline)

Adjacence `pos+1`. Active set propagé position par position.
Pour chaque position, union des postings de tous les ordinals alternatifs.

### resolve_word_chains_v3 (relaxed, word pipeline)

Utilise WordSfxPostReader pour les ordinals word-stripped.
Fallback vers PostingResolver pour les ordinals chunk dans les word chains.

Adjacence relaxed :
- `next_first_pos <= prev_last_pos` → rejeté
- `next_first_pos == prev_last_pos + 1` → OK (directement adjacent)
- sinon → `intermediates_are_pure_sep(posmap, bytemap, prev_last_pos+1, next_first_pos)`

**intermediates_are_pure_sep** : pour chaque position intermédiaire, vérifie
via posmap (position → ordinal) + bytemap (ordinal → bytes) que le token
ne contient aucun byte content (pas d'alphanum ni d'Unicode > 0x80).

### resolve_chains_v3_relaxed (legacy, pour tests)

Même chose que le strict mais avec adjacence relaxed via posmap/bytemap.
Exige des refs non-Option (pas de fallback ByteOrdered).

## Pipeline complet query v3

```
Query "mutexlock" strict_sep=false
  │
  ├─ Orchestrator: contains_v3()
  │   strip query → "mutexlock"
  │
  ├─ Composite: find_literal_v3()
  │   │
  │   ├─ fst_candidates(all partitions) → single-token matches
  │   │   "mutexlock" trouvé en 0x02 (word-stripped "mutexlock")
  │   │
  │   ├─ Chunk pipeline
  │   │   falling_walk_chunks("mutexlock") → splits en 0x00/0x01
  │   │   cross_chunk_chain_v3 → chains (best_consumed filter)
  │   │   resolve_chains_v3(strict pos+1) via PostingResolver
  │   │
  │   └─ Word pipeline (si posmap + bytemap + word_sfxpost disponibles)
  │       falling_walk_words("mutexlock") → splits en 0x02
  │       cross_word_chain_v3 → chains (pas de best_consumed)
  │       resolve_word_chains_v3(relaxed) via WordSfxPostReader
  │
  ├─ content_len filter (span=1 only)
  ├─ dedup par (doc_id, position)
  └─ exact_match filter (si demandé)
```

## Limitations potentielles introduites

### 1. best_consumed filter (chunk pipeline)

Le filtre garde seulement les ordinals du même consumed que le best split.
Si un ordinal de consumed plus court est le seul chemin valide dans un doc,
il est perdu. **En pratique, pas observé** (0 FN sur 500 docs) car les overlaps
divergent → le match n'est pas valide.

**Où surveiller** : queries courtes (3-4 bytes) avec beaucoup de markers,
corpus avec des tokens très variés partageant les mêmes suffixes courts.

### 2. Marker entries multi-parent (chunk pipeline)

Les markers courts ("\x01s", "\x01st") ont des centaines de parents.
Le best_consumed filter les gère mais ajoute un coût : tous les parents sont
décodés puis filtrés. Performance dégradée pour les queries avec beaucoup de
splits sur des markers courts.

**Optimisation future** : markers per-ordinal (clé FST incluant l'ordinal
pour éviter les collisions), ou validation termtexts des split candidates.

### 3. WordSfxPost non-disponible en merge

`merge_segments_v3()` crée un WordSfxPost vide. Les word maps et word
postings sont reconstruits dans `into_data()` du SfxCollectorDataV3 produit
par le merge. **Potentiel problème** : si le merge ne reconstruit pas
correctement les word-stripped entries, le word pipeline post-merge pourrait
être dégradé.

**Où vérifier** : indexation multi-segment avec merge, vérifier que les
word chains fonctionnent après un merge.

### 4. Word pipeline silencieusement sauté

Si posmap, bytemap, ou word_sfxpost est None, le word pipeline dans
`find_literal_v3` est silencieusement sauté (pas d'erreur). Seuls les
single-token matches 0x02 fonctionnent. Les cross-word chains relaxed
ne sont pas résolues.

**Quand ça arrive** : segments sans registry files (vieux index pré-v3),
ou tests qui ne construisent pas les maps.

### 5. Sémantique relaxed ≠ grep

Le v3 matche par mots ADJACENTS avec seps skippés. Le grep matche par
concaténation linéaire globale du texte strippé. Différences :
- v3 ne matche pas des fragments de mots éloignés
- v3 respecte les frontières de mots
- Le grep est plus permissif (et produit des résultats moins précis)

6 FN observés sur 500 docs × 15 queries. Considérés "by design".

### 6. fst_candidates retourne des ordinals cross-partition

Dans le word pipeline, fst_candidates retourne des ordinals de 0x00 ET 0x02.
Le resolve tente le word_sfxpost d'abord, puis fallback au chunk sfxpost.
Ça fonctionne mais c'est un peu impur — les ordinals chunk dans un word chain
ont first_position == last_position (pas de span mot).

### 7. Extended ordinals augmentent la taille de l'index

Chaque variant d'overlap a son propre ordinal et ses propres postings.
Avant : N content keys avec postings agrégés. Après : M extended texts
(M > N) avec postings séparés. Le sfxpost est plus gros.

**Impact estimé** : +20-30% d'ordinals, mais chaque ordinal a moins de
postings. La taille totale des postings est identique (redistribués).

## Fichiers clés (taille)

| Fichier | Lignes | Rôle |
|---------|--------|------|
| collector_v3.rs | 1119 | Tokenisation → interning → postings → into_data() |
| builder_v3.rs | 847 | Suffixes → FST keys → markers → build() |
| fst_walk.rs | 640 | Falling walk, fst_candidates, chain builders |
| resolve.rs | 637 | Resolve single/chains/word_chains, adjacency |
| composite.rs | 591 | find_literal, find_multi_token, resolve_trigrams |
| orchestrator.rs | 412 | contains_v3, fuzzy_v3 (entry points) |
| word_sfxpost.rs | 216 | WordSfxPost writer/reader/index |
