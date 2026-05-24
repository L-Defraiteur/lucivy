# Design : Word-stripped postings au dernier chunk

## Problème

Les word-stripped entries (partition 0x02) ont des postings au **premier chunk** du mot.
Quand une chain cross-mot cherche l'adjacence entre deux mots, la position du premier
chunk est trop éloignée du mot suivant — les chunks intermédiaires du même mot sont des
chunks CONTENT, et `intermediates_are_pure_sep` les rejette.

```
pos=0: "interna"   (content, mot 1, chunk 1/3)  ← posting actuel ws mot 1
pos=1: "tionali"   (content, mot 1, chunk 2/3)
pos=2: "zation_"   (content, mot 1, chunk 3/3)  ← devrait être ici
pos=3: "________"  (pure-sep)
pos=4: "initial"   (content, mot 2, chunk 1/2)  ← posting ws mot 2
```

Chain pos=0 → pos=4 : intermédiaires 1,2 sont content → rejeté.
Chain pos=2 → pos=4 : intermédiaire 3 est pure-sep → accepté ✓

## Solution

Changer les postings word-stripped pour utiliser la position du **dernier chunk** du mot,
tout en gardant le `byte_from` du premier chunk (pour les highlights corrects).

### Changements

#### 1. `WordStrippedEntry` — ajouter `last_chunk_intern_ord`

```rust
pub struct WordStrippedEntry {
    // ... champs existants ...
    pub first_chunk_intern_ord: u32,   // pour byte_from
    pub last_chunk_intern_ord: u32,    // NOUVEAU — pour position + byte_to
    // ...
}
```

#### 2. `add_value()` — enregistrer le dernier chunk

Déjà disponible : `chunk_intern_ids[last_ci]` est calculé dans la boucle existante.

```rust
self.word_stripped_entries.push(WordStrippedEntry {
    // ...
    first_chunk_intern_ord: chunk_intern_ids[first_ci],
    last_chunk_intern_ord: chunk_intern_ids[last_ci],  // NOUVEAU
    // ...
});
```

#### 3. `into_data()` — construire les postings hybrides

Pour chaque word-stripped entry avec `is_word_stripped == true` :

```
Pour chaque doc_id commun aux postings du premier et du dernier chunk :
  posting = (
    doc_id,
    position = dernier_chunk.token_index,    // pour adjacency check
    byte_from = premier_chunk.byte_from,     // pour highlight début mot
    byte_to = dernier_chunk.byte_to,         // pour highlight fin mot
  )
```

Avec extended ordinals, le premier chunk et le dernier chunk peuvent avoir
**plusieurs variants d'overlap** (différents ordinals). La jointure se fait
par `doc_id` :

```rust
// Trouver tous les ordinals du premier chunk (même content key)
let first_variants = content_key_to_interns[&first_content_key];
// Trouver tous les ordinals du dernier chunk (même content key)  
let last_variants = content_key_to_interns[&last_content_key];

// Collecter les postings par doc_id
let mut first_by_doc: HashMap<u32, (u32, u32)> = HashMap::new(); // doc → (ti, byte_from)
for &vio in first_variants {
    for &(doc_id, ti, bf, bt) in &self.token_postings[vio] {
        first_by_doc.entry(doc_id).or_insert((ti, bf));
    }
}

let mut agg = Vec::new();
for &vio in last_variants {
    for &(doc_id, ti, bf, bt) in &self.token_postings[vio] {
        if let Some(&(first_ti, first_bf)) = first_by_doc.get(&doc_id) {
            agg.push((doc_id, ti, first_bf, bt));
            //                 ^^  ^^^^^^^^  ^^
            //          last_pos   first_bf  last_bt
        }
    }
}
```

#### 4. Cas mono-chunk (premier == dernier)

Si le mot n'a qu'un seul chunk, `first_chunk_intern_ord == last_chunk_intern_ord`.
Le comportement est identique à l'actuel — position et byte_from viennent du même chunk.

#### 5. Cas word-stripped partagé avec chunk (`is_word_stripped == false`)

Si le word-stripped text est identique à un chunk text (mot mono-chunk sans sep),
ils partagent le même `intern_id`. Le word-stripped n'a pas d'entrée propre dans
`ord_map`. Son ordinal est celui du chunk → les postings sont ceux du chunk → correct.

### Impact

- **Single-word match** : fst_candidates trouve le word-stripped, le posting a
  position=last_chunk, byte_from=first_chunk_start, byte_to=last_chunk_end.
  Le highlight couvre le mot entier. ✓

- **Cross-word chain** : position du dernier chunk → adjacent au sep ou au mot suivant.
  `intermediates_are_pure_sep` ne voit que les vrais seps entre les mots. ✓

- **content_len filter** : `byte_to - byte_from` = longueur du mot entier (pas juste
  le dernier chunk). Correct pour la vérification de taille. ✓

- **Dedup** : les matches sont dedupliqués par `(doc_id, position)`. Position = last_chunk
  est stable et unique par mot. ✓

### Pas de changements à

- Le FST : les clés word-stripped et leurs métadonnées (own_len, sti) ne changent pas.
- Le resolve : l'adjacency check (strict ou relaxed) ne change pas.
- Le falling walk : les split candidates ne changent pas.
- Le posmap/bytemap : construits depuis le sfxpost, ils reflèteront les nouvelles positions.

### Fichiers à modifier

| Fichier | Changement |
|---------|------------|
| `src/suffix_fst/collector_v3.rs` | `WordStrippedEntry` + `add_value()` + `into_data()` |
| `src/suffix_fst/collector_v3.rs` | `build_word_stripped()` (pour le merge) |

### Tests de validation

1. `x11b_stripped_traverse_pure_sep` : "nationalizationinit" → doit matcher
2. `x11d_stripped_skip_multiple_seps` : "zationinitial" → doit matcher
3. Ground truth relaxed : uint64_t, function, TableFunction
