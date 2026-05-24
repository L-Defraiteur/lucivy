# Design : Split Table — Index-time split detection

## Note : fix rapide possible

Avant d'implémenter la split table, tester une approche plus simple :
**content-only keys dans le FST principal + suppression du DFS look-ahead**.
Les content-only keys ajoutent un noeud final au content boundary, le walk
normal le trouve, le look-ahead est inutile. Si ça marche → on ship ça
et la split table reste en parachute. Si ça foire → on branche la split
table.

---

## Problème

La détection de split dans le falling walk dépend de la **finalité des
noeuds FST**. Cette finalité est un artefact de la structure du FST, pas
de la sémantique. Quand l'index grandit, des noeuds finaux deviennent
non-finaux → splits perdus → FN.

Le FST est un outil de compression de clés. On lui demande de faire
quelque chose qu'il n'est pas conçu pour : encoder des points de
coupure sémantiques. La split table sépare les deux responsabilités.

## Design

### Principe

Un fichier d'index séparé qui encode, pour chaque ordinal word-stripped,
le point de split exact et les métadonnées nécessaires au chain builder.
Le falling walk n'a plus besoin d'atteindre un noeud final — il consulte
la split table pour savoir si un split existe à la position courante.

### Format : `word_splits` (extension `.wordsplits`)

Tableau compact indexé par ordinal. Chaque ordinal a une entrée de
taille fixe (8 bytes) :

```
WordSplitEntry {
    content_len: u16,    // nombre de bytes content du mot (split point)
    sep_len:     u8,     // sep après le content
    overlap_len: u8,     // bytes d'overlap indexés
    flags:       u8,     // bit 0 = is_word_start, bits 1-7 = réservés
    _reserved:   [u8; 3] // padding pour alignement 8 bytes
}
```

Le fichier :
```
Header:
  magic: "WSP2" (4 bytes)
  num_ordinals: u32

Body:
  entries: [WordSplitEntry; num_ordinals]  // 8 bytes chacun
```

Taille totale : 8 + num_ordinals × 8 bytes.
Pour 100K ordinals : 800 KB. Pour 1M : 8 MB. Négligeable.

### Quels ordinals ont une entrée ?

Tous les ordinals word-stripped (partition 0x02). Les ordinals chunk
(partitions 0x00/0x01) ont `content_len = 0` (pas de split via cette
table — leur split est géré par les markers existants dans le FST).

En pratique : la table est dense (indexée par ordinal), les entrées
chunk ont content_len=0. Pas de hashmap, pas de lookup — accès O(1)
par index.

### Construction (index-time)

Dans `collector_v3.rs::into_data()`, après l'assignation des ordinals
finaux :

```rust
let mut split_entries = vec![WordSplitEntry::default(); num_final_ords];

for ws in &word_stripped_entries {
    let final_ord = intern_to_final[ws.first_intern_ord as usize];
    let content_len = ws.word_content.len() as u16;
    let sep_len = ws.last_sep_len;
    let overlap_len = ws.content_overlap.len() as u8;
    split_entries[final_ord as usize] = WordSplitEntry {
        content_len,
        sep_len,
        overlap_len,
        flags: if ws.is_word_start { 1 } else { 0 },
        _reserved: [0; 3],
    };
}
```

Sérialisé dans `SfxCollectorDataV3::word_splits: Vec<u8>`.
Enregistré comme registry file `"wordsplits"` dans `sfx_dag_v3.rs`.

### Lecture (query-time)

```rust
pub struct WordSplitReader<'a> {
    data: &'a [u8],
    num_ordinals: u32,
}

impl<'a> WordSplitReader<'a> {
    pub fn content_len(&self, ordinal: u32) -> u16 {
        // O(1) lookup, 8 bytes at offset 8 + ordinal * 8
        let offset = 8 + ordinal as usize * 8;
        u16::from_le_bytes([self.data[offset], self.data[offset + 1]])
    }
}
```

### Utilisation dans le falling walk

Le falling walk actuel :
```
for each byte of query:
    walk FST
    if node.is_final():
        decode parents
        check content_len vs prefix_len → split?
```

Avec split table, on ajoute une deuxième source de splits. Mais le
falling walk ne sait pas quels ordinals sont "sous" le noeud courant
sans atteindre un noeud final. On ne peut pas consulter la split table
à chaque byte du walk.

**Solution : la split table est utilisée PAR LE CHAIN BUILDER, pas par
le falling walk.**

Le flow devient :

```
1. falling_walk_words(query)     → splits (comme avant, aux noeuds finaux)
2. fst_candidates(query)         → candidats single-token (déja appelé)
3. Pour chaque candidat de fst_candidates :
     split_table.content_len(ord) → cl
     if cl > 0 && cl - sti <= query.len():
         → ajouter comme split supplémentaire
4. Merge splits de (1) et (3), dedup
5. build_chains_from_splits (inchangé)
```

L'étape 3 est O(K) où K = nombre de candidats fst_candidates (typiquement
< 50). Coût négligeable.

**Avantage** : le falling walk reste inchangé. La split table ne sert que
de filet de sécurité pour les splits ratés. Si le falling walk trouve le
split (noeud final), la split table confirme. Si le falling walk rate
(noeud non-final), la split table rattrape.

### Pourquoi fst_candidates + split table couvre tous les cas

Le falling walk rate un split quand le query s'épuise DANS l'overlap
(entre content_len et content_len + overlap_len). Dans ce cas :

- Le query EST un préfixe d'une clé FST (la clé avec overlap)
- fst_candidates(query) TROUVE cette clé (range query)
- La split table donne le content_len de l'ordinal
- On sait que c'est un split

Pour les queries qui s'arrêtent AVANT le content_len : pas de split
(le query n'a pas assez consommé). Le falling walk et fst_candidates
s'accordent.

Pour les queries qui dépassent le content_len + overlap_len : le
falling walk trouve le split au noeud final (la clé entière est
consommée). Pas besoin de la split table.

Le seul cas raté par le falling walk ET couvert par fst_candidates +
split table : **query s'arrête entre content_len et content_len +
overlap_len, et le noeud au content_len n'est pas final dans le FST**.
C'est exactement le bug qu'on corrige.

## Fichiers à modifier

| Fichier | Changement |
|---------|------------|
| `src/suffix_fst/word_splits.rs` | NEW — WordSplitWriter/Reader, format, index entry |
| `src/suffix_fst/mod.rs` | `pub mod word_splits;` |
| `src/suffix_fst/collector_v3.rs` | Construire `word_splits` dans `into_data()` |
| `src/suffix_fst/index_registry.rs` | Enregistrer `WordSplitsIndex` dans `all_indexes()` |
| `src/indexer/sfx_dag_v3.rs` | Ajouter `"wordsplits"` aux registry_files |
| `src/suffix_fst/briques/composite.rs` | `find_literal_v3` : ajouter splits depuis fst_candidates + split table |
| `src/query/contains_query_v3.rs` | Charger word_splits du segment reader |
| `src/query/fuzzy_query_v3.rs` | Idem |

## Résultat attendu

- Ground truth : **15/15** (uint64_t relaxed : 23/23)
- Zéro dépendance à la finalité des noeuds FST pour la détection de splits
- Falling walk inchangé (pas de look-ahead, pas de DFS)
- Coût index : ~800 KB pour 100K ordinals
- Coût query : O(K) lookup dans la split table, K = fst_candidates count

## Et les chunks (best_consumed) ?

La split table est conçue pour les ordinals word-stripped (0x02). Les
ordinals chunk (0x00/0x01) ont `content_len = 0` dans la table.

Pour éliminer `best_consumed` dans le chunk pipeline, la même approche
pourrait s'appliquer : stocker le content_len des chunks dans la split
table et utiliser fst_candidates + split table au lieu des markers.
Mais les chunks au SI>0 ont des suffixes courts qui collisionnent
naturellement dans fst_candidates — le problème est différent.

**Prochaine étape** : implémenter la split table pour le word pipeline,
valider 15/15, puis réfléchir à l'extension aux chunks.
