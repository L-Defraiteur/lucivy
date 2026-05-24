# Architecture mise en place — Sessions 4-5 (17-24 mai 2026)

## Vue d'ensemble

Refonte complète du pipeline cross-token v3 pour éliminer les faux positifs
et structurer proprement le matching strict et relaxed.

## Composants implémentés

### 1. Extended ordinals (index-time)

**Fichier** : `collector_v3.rs::into_data()`

Chaque texte étendu unique (content + sep + overlap) a son propre ordinal
et ses propres postings. Plus de groupement par `text[..own_len]`.

**Avant** : "func lo" et "func re" (même content, overlap différent) → même ordinal.
**Après** : ordinals séparés, postings séparés. Pas de mélange d'overlap.

**Limite** : augmente le nombre d'ordinals (et la taille du sfxpost). Chaque
variant d'overlap a ses propres postings au lieu de les partager.

### 2. WordSfxPost — postings séparés pour partition 0x02 (index-time)

**Fichier** : `word_sfxpost.rs` (nouveau), `collector_v3.rs`, `sfx_dag_v3.rs`

Format de postings dédié pour les word-stripped entries :
```
WordPostingEntry {
    doc_id: u32,
    first_position: u32,   // premier chunk du mot
    last_position: u32,    // dernier chunk du mot (pour adjacency)
    byte_from: u32,        // début du mot dans le texte
    byte_to: u32,          // fin du mot
}
```

Construit par jointure first/last chunk postings par doc_id. Les ordinals
word-stripped n'ont PAS de postings dans le sfxpost chunk (séparation complète).

Stocké comme registry file "word_sfxpost", enregistré dans `all_indexes()`.

**Limite** : les word-stripped ordinals qui partagent un intern_id avec un chunk
(texte identique, is_word_stripped=false) n'ont pas d'entrée dans le word_sfxpost.
Ils sont résolus via le chunk sfxpost (fallback dans resolve_word_chains_v3).

### 3. Séparation des chains par partition (query-time)

**Fichiers** : `fst_walk.rs`, `composite.rs`, `resolve.rs`

Deux pipelines parallèles, jamais mélangés :

```
Query strict_sep=false
  │
  ├── fst_candidates (toutes partitions) → single-token matches
  │
  ├── Chunk pipeline (0x00 + 0x01)
  │     falling_walk_chunks → cross_chunk_chain_v3 → resolve_chains_v3 (strict pos+1)
  │     PostingResolver (sfxpost chunk)
  │     best_consumed filter actif (empêche mélange de marker entries)
  │
  └── Word pipeline (0x02)
        falling_walk_words → cross_word_chain_v3 → resolve_word_chains_v3 (relaxed)
        WordSfxPostReader + fallback PostingResolver
        PosMap + ByteMap REQUIS (pas de fallback ByteOrdered)
        Pas de best_consumed filter
```

**Limite** : le word pipeline ne tourne que si posmap + bytemap + word_sfxpost
sont tous disponibles. Sans eux, seuls les single-token matches 0x02 fonctionnent.

### 4. resolve_word_chains_v3 (query-time)

**Fichier** : `resolve.rs`

Résout les word chains en utilisant le WordSfxPostReader. Adjacency check :
- `prev_last_position` (dernier chunk du mot précédent)
- `next_first_position` (premier chunk du mot suivant)
- Intermédiaires vérifiés comme pure-sep via posmap/bytemap

Fallback vers PostingResolver pour les ordinals chunk mixés dans les word chains
(quand fst_candidates retourne des ordinals 0x00/0x01 dans la continuation).

### 5. best_consumed filter (query-time, chunk pipeline seulement)

**Fichier** : `fst_walk.rs::build_chains_from_splits()`

Dans le chunk pipeline, les sub_splits de la falling walk viennent de nœuds FST
à différentes positions (markers courts → multi-parent). Le filtre garde
seulement les ordinals du même `query_consumed` que le best split.

**Actif pour** : chunk pipeline (`filter_best_consumed=true`)
**Inactif pour** : word pipeline (`filter_best_consumed=false`)

**Limite** : peut théoriquement perdre des vrais positifs intra-chunk si un
ordinal de consumed plus court est le seul chemin valide dans un doc. En pratique,
les overlaps divergent → le match n'est pas valide. 0 FN observés sur 500 docs.

### 6. fst_candidates incluant 0x02 pour anchor_start+relaxed (query-time)

**Fichier** : `fst_walk.rs::fst_candidates_v3()`

Quand `anchor_start=true && !strict_separators`, les partitions consultées sont
`[0x00, 0x02]` (au lieu de `[0x00]` seulement). Permet au chain builder word
de trouver les mots au SI=0 en partition 0x02.

La range query `fst.range().ge(query).lt(query+1)` retourne naturellement les
clés PLUS LONGUES qui partagent le préfixe de la query. Ça permet le matching
de préfixes de mots (e.g., "function" dans "functions") sans markers supplémentaires.

### 7. Suppression du fallback ByteOrdered (query-time)

**Fichier** : `resolve.rs`

`resolve_chains_v3_relaxed` exige `&PosMapReader` et `&ByteBitmapReader`
(non-Option). Plus de dégradation silencieuse vers ByteOrdered.

### 8. Propagation des maps dans tout le pipeline

posmap, bytemap, word_sfxpost propagés à travers :
- `contains_v3`, `fuzzy_v3` (orchestrator)
- `find_literal_v3`, `find_multi_token_v3` (composite)
- `contains_query_v3`, `fuzzy_query_v3` (query layer)
- `regex_v3` (regex pipeline)

### 9. Diag builder V3_DIAG_BUILD

**Fichier** : `builder_v3.rs`

Activé par `V3_DIAG_BUILD=1`. Log les clés FST multi-parent avec ordinals
distincts, stats de collision. Utile pour diagnostiquer les FP liés aux markers.

## Résultats ground truth

### Avant (début session 4)
- Strict : 6/10, Relaxed : 0/5
- Total : **6/15**

### Après (fin session 5)
- Strict : **10/10**, Relaxed : **3/5**
- Total : **13/15**
- 0 faux positifs

### Détail relaxed
| Query | Grep | V3 | Status |
|-------|------|----|--------|
| function relax | 62 | 62 | **OK** |
| uint64_t relax | 23 | 18 | 5 FN |
| std::unique_ptr relax | 8 | 8 | **OK** |
| ku_dynamic_cast relax | 0 | 0 | **OK** |
| TableFunction relax | 5 | 4 | 1 FN |

## Fichiers modifiés/créés

| Fichier | Type | Description |
|---------|------|-------------|
| `src/suffix_fst/word_sfxpost.rs` | NEW | Format WordSfxPost writer/reader + index entry |
| `src/suffix_fst/collector_v3.rs` | MOD | Extended ordinals, word postings séparés, last_chunk jointure |
| `src/suffix_fst/builder_v3.rs` | MOD | V3_DIAG_BUILD diagnostic |
| `src/suffix_fst/briques/fst_walk.rs` | MOD | Split falling_walk chunk/word, best_consumed per-partition |
| `src/suffix_fst/briques/composite.rs` | MOD | Deux resolve paths, propagation maps |
| `src/suffix_fst/briques/resolve.rs` | MOD | resolve_word_chains_v3, suppression ByteOrdered |
| `src/suffix_fst/briques/orchestrator.rs` | MOD | Propagation word_sfxpost |
| `src/suffix_fst/index_registry.rs` | MOD | Enregistrement WordSfxPostIndex |
| `src/suffix_fst/mod.rs` | MOD | pub mod word_sfxpost |
| `src/indexer/sfx_dag_v3.rs` | MOD | Extended ordinals, word_sfxpost registry file |
| `src/query/contains_query_v3.rs` | MOD | Chargement posmap/bytemap/word_sfxpost |
| `src/query/fuzzy_query_v3.rs` | MOD | Idem |

## Commits

| Hash | Description |
|------|-------------|
| `8f041e4` | Extended ordinals + chain builder consumed filter |
| `dcb6353` | Investigation report session 5 |
| `0667f76` | Remove ByteOrdered fallback, propagate posmap/bytemap |
| `2eb00e2` | Design docs: partition-separated chains + word sfxpost |
| `d1690aa` | WordSfxPost format + word-stripped postings separated |
| `fd31754` | Partition-separated chains + WordSfxPost resolve |
| `0924d67` | best_consumed filter only for chunk pipeline |
| `16a9912` | Register WordSfxPostIndex in all_indexes |
| `62be879` | Session 5 recap doc |
| `f45298d` | Include 0x02 in fst_candidates for anchor_start+relaxed |
