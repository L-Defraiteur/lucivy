# Design — Merge SFX Optimization

Date : 17 mars 2026
Status : Design

## Problème

`merger.merge_sfx()` reconstruit le suffix FST from scratch à chaque merge de segments.
C'est le coût dominant du merge pour les champs texte avec suffix index.

### Bottlenecks identifiés

**1. Alive check par lecture de postings (O(T × P))**
```rust
// Pour chaque terme de chaque segment source :
let mut postings = inv_idx.read_postings_from_terminfo(&ti, Basic)?;
let has_alive_doc = loop {
    let doc = postings.doc();
    if doc == TERMINATED { break false; }
    if alive.is_alive(doc) && reverse_doc_map.contains_key(&doc) { break true; }
    postings.advance();
};
```
Lit les posting lists juste pour vérifier si au moins un doc est vivant.
Sur un segment de 5K docs avec 50K termes → 50K lectures de postings.

**2. Suffix FST rebuild (O(E log E))**
```rust
for (ordinal, token) in unique_tokens.iter().enumerate() {
    sfx_builder.add_token(token, ordinal as u64);  // génère tous les suffixes
}
let (fst_data, parent_list_data) = sfx_builder.build()?;  // sort + FST build
```
`add_token` regénère tous les suffixes (N par token, N = longueur moyenne).
`build()` trie E entrées où E = tokens × longueur moyenne.
Les source FSTs ont **déjà** ces suffixes triés — travail 100% redondant.

**3. Double pass sur les termes**
Pass 1 : collecte unique_tokens (BTreeSet) → pour le FST
Pass 2 : construit token_to_ordinal (HashMap) → pour le sfxpost merge
Les deux itèrent les mêmes term dictionaries.

**4. sfxpost merge avec allocations**
Pour chaque token : collect entries de tous segments → Vec → sort → encode.
Beaucoup d'allocations intermédiaires.

## Phases d'optimisation

### Phase A — Skip alive check via merged term dictionary

**Idée** : le merge principal (postings + term dictionary) a déjà filtré les termes
morts. La merged term dictionary ne contient QUE les termes avec des docs vivants.
On peut l'utiliser directement au lieu de re-scanner les source segments.

**Changement** :
```rust
// AVANT : scan chaque source segment, check alive pour chaque terme
let mut unique_tokens = BTreeSet::new();
for reader in &self.readers {
    let term_dict = reader.inverted_index(field).terms();
    for term in term_dict.stream() {
        if has_alive_doc_in_postings(term) {  // COÛTEUX
            unique_tokens.insert(term);
        }
    }
}

// APRÈS : lire directement la merged term dictionary
let merged_term_dict = merged_segment.inverted_index(field).terms();
let mut unique_tokens = Vec::new();  // déjà trié !
let mut stream = merged_term_dict.stream();
while stream.advance() {
    unique_tokens.push(std::str::from_utf8(stream.key()).unwrap().to_string());
}
```

**Gain** : élimine O(T × P) lectures de postings. Les termes sont déjà triés
(stream de term dict est toujours trié) → pas besoin de BTreeSet.

**Prérequis** : le merge principal doit être terminé avant merge_sfx.
Vérifier que c'est le cas dans le flow actuel.

**Benchmark avant Phase B** : mesurer le gain isolé de Phase A sur 5K docs.
Si le merge est déjà rapide (<10ms), les phases suivantes sont moins urgentes.

### Phase B — Merge-sort des FST streams (skip rebuild)

**Idée** : au lieu de reconstruire le suffix FST via `add_token` (qui regénère
tous les suffixes puis trie), merger directement les streams triés des source FSTs.

Les source .sfx FSTs sont déjà triés par clé. On peut faire un N-way merge sort
de ces streams pour produire le merged FST en O(E) au lieu de O(E log E).

**Changement** :
```rust
// AVANT : collect tokens → add_token (regénère suffixes) → sort → build FST
for token in unique_tokens {
    sfx_builder.add_token(token, ordinal);  // O(L) suffixes par token
}
sfx_builder.build()  // O(E log E) sort

// APRÈS : N-way merge des source FST streams → build FST directement
let mut streams: Vec<FstStream> = source_fsts.iter()
    .map(|fst| fst.stream())
    .collect();

let mut fst_builder = MapBuilder::memory();
// N-way merge (les streams sont déjà triés)
while let Some((key, merged_output)) = merge_next(&mut streams) {
    // Remap ordinals dans l'output (parent entries)
    let remapped = remap_parent_entries(merged_output, ordinal_maps);
    fst_builder.insert(key, remapped)?;
}
```

**Complexité** :
- Avant : O(E log E) pour le sort dans build()
- Après : O(E × log N) pour le N-way merge (N = nombre de segments, typiquement 2-5)

**Points importants** :
- Les ordinals dans les parent entries doivent être remappés (vieux ordinal → nouveau)
- Le prefix byte (\x00 / \x01) est déjà dans les clés FST → transparent
- La parent list (output table) doit être reconstruite avec les ordinals remappés
- Les tokens qui apparaissent dans plusieurs segments doivent être fusionnés
  (même clé suffixe → union des parent entries)

**Prérequis** : Phase A terminée et benchmarkée.

**Benchmark avant Phase C** : mesurer le gain de Phase B.
Comparer temps de merge avec/sans rebuild.

### Phase C — Single-pass merge (tokens + sfxpost)

**Idée** : combiner la collecte de tokens et le merge sfxpost en un seul pass.
Actuellement on itère les termes 2 fois (une pour unique_tokens, une pour
token_to_ordinal). Avec le merge-sort de Phase B, on peut construire le sfxpost
en même temps que le FST.

**Changement** :
```rust
// Le N-way merge produit les tokens dans l'ordre trié.
// Pour chaque token, on connaît le nouvel ordinal → on peut immédiatement
// merger les sfxpost entries avec doc_id remapping.

let mut posting_offsets = Vec::new();
let mut posting_bytes = Vec::new();
let mut ordinal = 0u64;

for (key, source_entries) in n_way_merge(&streams) {
    // Écrire dans le FST
    fst_builder.insert(key, encode_output(ordinal, ...))?;

    // En même temps, merger les sfxpost entries pour ce token
    posting_offsets.push(posting_bytes.len() as u32);
    for (seg_ord, old_ord) in source_entries {
        for entry in sfxpost_readers[seg_ord].entries(old_ord) {
            if let Some(new_doc) = reverse_doc_map[seg_ord].get(&entry.doc_id) {
                encode_vint(*new_doc, &mut posting_bytes);
                // ...
            }
        }
    }

    ordinal += 1;
}
```

**Gain** : élimine le 2ème pass sur les termes + les HashMap<String, u32>
(token_to_ordinal). Un seul pass produit FST + sfxpost.

**Prérequis** : Phase B terminée et benchmarkée.

### Phase D — GapMap streaming (optionnel)

**Idée** : le gapmap copy est déjà O(D) (un copy par doc dans merge order).
Mais on alloue un GapMapWriter intermédiaire. On pourrait streamer directement
les bytes dans le serializer sans buffer intermédiaire.

**Gain estimé** : minime (le gapmap est petit par rapport au FST).
À faire seulement si les phases A-C ne suffisent pas.

## Résumé

| Phase | Description | Complexité code | Gain estimé |
|-------|------------|-----------------|-------------|
| A | Skip alive check, use merged term dict | Simple (~30 lignes) | Élimine O(T×P) I/O |
| B | N-way merge FST streams | Moyen (~100 lignes) | O(E log E) → O(E log N) |
| C | Single-pass merge (FST + sfxpost) | Moyen (~80 lignes) | Élimine 2ème pass + HashMaps |
| D | GapMap streaming | Simple (~20 lignes) | Minime |

## Points de benchmark

Après chaque phase, mesurer sur le bench 5K docs :
1. **Temps d'indexation total** (inclut le merge)
2. **Temps de commit** (déclenche le merge)
3. **Nombre de segments avant/après merge**
4. Si possible, instrumenter le temps de `merge_sfx` isolément

Baseline à établir AVANT Phase A : temps de merge_sfx actuel sur 5K docs.
