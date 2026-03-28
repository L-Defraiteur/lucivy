# 01 — Design : Merge incrémental SFX (sfxpost, posmap, bytemap)

Date : 28 mars 2026

## Problème

Le merge SFX actuel (`merge_sfx_deferred`) reconstruit TOUT depuis zéro :

1. **Collecte de tous les tokens uniques** — stream tous les term dicts de tous les segments, insère chaque token dans un BTreeSet (allocation String par token)
2. **Reconstruction token_to_ordinal** — re-stream tous les term dicts pour construire des HashMap<String, u32> par segment (encore une allocation String par token)
3. **Reconstruction complète de sfxpost** — pour chaque token, pour chaque segment, lookup ordinal, lire les entries, remapper doc_ids, push dans un Vec temporaire, puis add_entry dans un nouveau SfxPostWriterV2
4. **Reconstruction complète de posmap** — dans la même boucle, `posmap_writer.add()` pour chaque posting entry remappé
5. **Reconstruction complète de bytemap** — dans la même boucle, `bytemap_writer.record_token()` par token

Pour N segments avec T tokens et E entries total :
- `O(N × T)` streams term dict (étape 1 + 2)
- `O(T × N)` lookups HashMap (étape 3)
- `O(E)` remappings + copies (étape 3-4)
- `O(T)` String allocations × 2 (étape 1 + 2)

Sur 90K docs avec ~500K tokens uniques, ça chiffre.

## Ce qu'on peut éviter

### Observation 1 : sfxpost est déjà indexé par ordinal

Le `SfxPostReaderV2` lit directement les entries d'un ordinal via `entries(ordinal)`. On n'a pas besoin de reconstruire un HashMap<String, u32> pour trouver l'ordinal — on peut fusionner les entries directement par ordinal si on a une correspondance ordinal-source → ordinal-cible.

### Observation 2 : les ordinals sont triés par token texte

Le term dict est trié alphabétiquement. Les ordinals 0, 1, 2... correspondent aux tokens dans l'ordre lexicographique. Deux segments qui ont le même token "mutex" auront des ordinals différents, mais on peut les aligner par merge-sort sur les clés du term dict.

### Observation 3 : PosMap et ByteMap sont dérivés de sfxpost

- `posmap[doc_id][position] = ordinal` — c'est juste un sous-ensemble de sfxpost (doc_id, position → ordinal)
- `bytemap[ordinal] = byte_presence` — c'est juste les bytes du token text à cet ordinal

Si on fusionne sfxpost correctement, posmap et bytemap suivent gratuitement.

### Observation 4 : le GapMap est déjà fusionné par copie brute

Le GapMap est copié doc par doc dans l'ordre du mapping — pas de reconstruction. C'est le bon pattern.

## Architecture proposée : merge par fusion ordonnée

### Principe

Au lieu de reconstruire depuis zéro, **fusionner les sfxpost comme un merge-sort sur les term dicts triés**.

```
Segment A (term dict trié) :     Segment B (term dict trié) :
  ord 0: "array"                   ord 0: "buffer"
  ord 1: "buffer"                  ord 1: "mutex"
  ord 2: "mutex"                   ord 2: "queue"

Merge-sort → new ordinals :
  new 0: "array"   ← A:0
  new 1: "buffer"  ← A:1, B:0  (entries fusionnées)
  new 2: "mutex"   ← A:2, B:1  (entries fusionnées)
  new 3: "queue"   ← B:2
```

Pour chaque token dans l'ordre fusionné :
1. Lire les entries depuis chaque segment source (par ordinal, pas par lookup HashMap)
2. Remapper les doc_ids via reverse_doc_map
3. Écrire directement dans sfxpost_writer + posmap_writer
4. bytemap : OR les bitmaps sources (pas besoin de relire le texte)

### Détail : fusion des bytemaps

Si le même token existe dans 2 segments, le bytemap est identique (mêmes bytes dans le texte). On peut simplement copier le bitmap du premier segment qui l'a. Pas besoin de `record_token()` qui re-scanne le texte.

Si le token est nouveau (un seul segment), copier son bitmap directement.

### Détail : fusion des sfxpost entries

Pour un token présent dans N segments :
```rust
for (seg_ord, old_ordinal) in sources {
    for entry in sfxpost_readers[seg_ord].entries(old_ordinal) {
        if let Some(&new_doc) = reverse_doc_map[seg_ord].get(&entry.doc_id) {
            sfxpost_writer.add_entry(new_ord, new_doc, entry.ti, entry.bf, entry.bt);
            posmap_writer.add(new_doc, entry.ti, new_ord);
        }
    }
}
```

Pas de Vec temporaire `merged`, pas de sort intermédiaire.

### Détail : stream merge des term dicts

```rust
// Ouvrir N streams triés (un par segment)
let mut streams: Vec<TermStream> = readers.iter()
    .map(|r| r.inverted_index(field).terms().stream())
    .collect();

// Merge N-way sur les clés
loop {
    // Trouver le plus petit token parmi les heads de tous les streams
    let min_token = streams.iter()
        .filter(|s| !s.is_done())
        .map(|s| s.key())
        .min();

    let Some(token) = min_token else { break };

    // Collecter les ordinals sources qui ont ce token
    let mut sources: Vec<(usize, u32)> = Vec::new(); // (seg_ord, old_ordinal)
    for (seg_ord, stream) in streams.iter_mut().enumerate() {
        if !stream.is_done() && stream.key() == token {
            sources.push((seg_ord, stream.ordinal()));
            stream.advance();
        }
    }

    // Fusionner entries + écrire sfxpost + posmap + bytemap
    // ...
    new_ord += 1;
}
```

### Complexité comparée

| Opération | Actuel | Proposé |
|---|---|---|
| Stream term dicts | 2 × O(N × T) | 1 × O(T_total) (merge-sort) |
| String allocations | O(T_unique) × 2 | 0 (compare bytes directement) |
| HashMap lookups | O(T_unique × N) | 0 (ordinals par stream position) |
| Vec temporaire merged | O(E) allocs | 0 (écriture directe) |
| sfxpost entries | O(E) copies | O(E) copies (identique) |
| Bytemap | O(T) × record_token | O(T) × copie bitmap 32 bytes |

**Gain principal** : élimination de toutes les allocations String et HashMap. Le coût dominant O(E) pour les entries reste identique — c'est incompressible.

## Prérequis

### 1. Accès aux bytemaps sources pendant le merge

Le merger doit charger les `.bytemap` des segments sources pour copier les bitmaps. Actuellement `bytemap_file()` est disponible sur `SegmentReader`, donc c'est déjà possible.

### 2. Stream avec ordinal

Le term dict stream doit exposer l'ordinal courant. Vérifier que `TermDictionary::stream()` fournit cette info (via `stream.value()` qui retourne le TermInfo, ou via un compteur).

### 3. Alive check intégré au merge-sort

Pour les segments avec deletes, un token ne doit être inclus que s'il a au moins un doc alive. L'actuel fait un check séparé (étape 1). Le proposé peut intégrer ce check dans la boucle de fusion : si aucune entry survivante pour ce token dans ce segment, skipper.

## Risques

### Token texte pas disponible dans le stream

Le stream fournit la clé (bytes du token) mais le sfxpost ne stocke pas le texte. Pour le bytemap, on a besoin soit du texte soit du bitmap source. Si le bitmap source est disponible (`.bytemap` chargé), pas de problème. Sinon, on tombe sur le texte via la clé du stream.

### Ordinal mismatch term dict vs sfxpost

Le term dict standard et le sfxpost peuvent avoir des ordinals différents si le sfxpost est construit par le SfxCollector (qui trie différemment). Vérifier que l'ordinal dans le term dict correspond bien à l'ordinal dans le sfxpost.

**Réponse** : non, ils correspondent. Le SfxCollector trie par token texte (même ordre que le term dict). Le `token_to_ordinal` HashMap dans le code actuel le confirme — c'est un mapping 1:1 par construction.

Mais ATTENTION : le term dict contient TOUS les tokens (y compris ceux sans SFX entries si le schéma a des champs non-SFX). Le sfxpost ne contient que les tokens SFX. Il faudra vérifier que le stream ne produit pas des tokens qui n'existent pas dans le sfxpost.

**Mitigation** : utiliser `sfxpost_reader.num_ordinals()` pour borner les ordinals valides. Si un ordinal dépasse, c'est un token non-SFX — skipper.

## Implémentation

### Fichiers à modifier

| Fichier | Changement |
|---|---|
| `src/indexer/merger.rs` | Réécrire la boucle de fusion dans `merge_sfx_deferred` |
| `src/suffix_fst/bytemap.rs` | Ajouter `copy_bitmap(ordinal, &[u8; 32])` sur ByteBitmapWriter |

### Pas de changement

| Fichier | Raison |
|---|---|
| `sfxpost_v2.rs` | `SfxPostWriterV2::add_entry` inchangé |
| `posmap.rs` | `PosMapWriter::add` inchangé |
| `collector.rs` | Build initial inchangé (pas de merge) |
| `segment_writer.rs` | Écriture fichiers inchangée |

## Ordre d'implémentation

1. Vérifier que l'ordinal term dict == ordinal sfxpost (assertion en debug)
2. Ajouter `ByteBitmapWriter::copy_bitmap(ordinal, bitmap: &[u8; 32])`
3. Réécrire la boucle dans `merge_sfx_deferred` avec le merge-sort N-way
4. Tests : merge 2 segments, merge 3+ segments, merge avec deletes
5. Bench : comparer temps de merge avant/après sur 90K docs
