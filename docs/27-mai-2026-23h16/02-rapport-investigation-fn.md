# Rapport investigation FN — session 7

> 28 mai 2026, branche feature/dag-query-refactor

## Score ground truth

11/15 a 5000 docs (meme que session 6). 4 FN, tous SCALE-DEPENDENT.

## Infra construite cette session

1. **execute_sequential** dans luciole (runner DAG sans scheduler)
2. **LocalDag / LocalNode<S>** (DAG sans Send/'static, fan-out via Rc)
3. **9 noeuds find_literal_v3** (FstCandidates, ResolveSingle, ChunkChain, SiblingChunk, ResolveChunk, WordChain, SiblingWord, ResolveWord, Merge)
4. **Mermaid explain** (DagResult::dump_mermaid)
5. **Edge annotations** (annotate_output + explain mode, JSON par arete)
6. **FST keys dump** dans FstCandidatesNode (cles brutes + parents)
7. **Per-candidate postings** dans ResolveSingleNode (ordinal → doc_ids)
8. **Doc forensics** dans le ground truth test (find segment, tokenize, reverse scan)

## Preuve du bug

### Le doc FN

- Global idx: 4742
- Path: `tools/rust_api/rag3db-src/extension/rag3weaver/codeparsers/src/base/language_parser.rs`
- Contenu pertinent: `"common functionality"` (ligne 19, tout en minuscule)
- Segment: 33, local doc_id: 30

### Ce que le tokenizer produit

`"functionality"` (13 chars) est un mot unique. `equal_chunks("functionality", "", 8)` :
- num_chunks = ceil(13/8) = 2
- base = 13/2 = 6, extra = 13%2 = 1
- Chunk 0: 7 bytes → `"functio"`, overlap = premier 2 bytes du chunk 1 = `"na"` → extended = `"functiona"`
- Chunk 1: 6 bytes → `"nality"`, overlap = "" → extended = `"nality"`

NOTE: own_len=7, PAS 8. Le tokenizer divise egalement (pas stride fixe).

### Ce que le FST contient

Le range scan `ge="function" lt="functioo"` trouve la cle `"functiona"` (9 bytes) car elle commence par "function". Cette cle existe dans 8 segments sur 80 (ceux qui ont un doc avec "functionality" ou similaire).

### Ce que les postings montrent

Dans le segment 33 (celui du doc FN) :
- 11 candidats FST pour "function" — **AUCUN** n'a doc 30 dans ses postings
- 155 ordinals totaux pour doc 30 — **AUCUN** dont le texte contient "functio"
- Les postings pour doc 30 passent de pos=95 `"common "` (bytes 549-556) directement a pos=97 `"ality\n\n"` (bytes 564-571)
- **Le posting pour pos=96 (bytes 556-564, le chunk "functio") n'existe pas**

### Conclusion

Le chunk pour "functionality" est bien produit par le tokenizer (pos 96, 7 bytes). Mais sa posting (doc_id=30, pos=96, byte_from=556, byte_to=564) n'a jamais ete ecrite dans le sfxpost.

C'est un **bug du collector_v3** a l'indexation. Le posting est perdue quelque part entre `add_value()` (qui appelle `token_postings[intern_id].push(...)`) et `into_data()` (qui construit `ord_map` et assigne les ordinals finaux).

## Fix 1 : separation des namespaces intern (11/15 → 13/15)

### Le bug

`collector_v3.rs:intern_extended()` utilise le texte etendu comme cle unique.
Mais chunk (partition 0x00/0x01) et word-stripped (partition 0x02) peuvent
avoir le **meme texte etendu** (ex: "functional").

`intern_extended()` retourne l'ord existant sans mettre a jour le meta.
Si word-stripped est interned en premier → `is_word_stripped: true`.
Le chunk qui arrive apres herite de ce flag.
`into_data()` fait `if is_word_stripped { continue }` → **postings du chunk ignorees**.

### Le fix

```rust
fn intern_extended(&mut self, text: &str, meta: TokenMetaV3) -> u32 {
    let key = if meta.is_word_stripped {
        format!("\x00ws:{text}")
    } else {
        text.to_string()
    };
    // ... lookup par key au lieu de text
}
```

Fichier : `src/suffix_fst/collector_v3.rs`, fonction `intern_extended()`.

### Resultat

- `function strict` : 1466 → **1467** = OK
- `rag3db strict` : 3075 → **3076** = OK
- Score : 11/15 → 13/15

## Fix 2 : partition du ord_map (elimine 49 FP relax)

### Le bug

Le `ord_map` dans `into_data()` utilisait le texte brut comme cle BTreeMap.
Meme avec des intern_ids separes (fix 1), chunk "functional" et word-stripped
"functional" collisionnent dans le ord_map → fusionnes dans le meme OrdEntry.

Consequences :
- Le word-stripped entry (postings vides dans sfxpost, postings dans WordSfxPost)
  est fusionne avec le chunk entry → l'ordinal final a des postings chunk mais
  est aussi utilise comme word-stripped → doublons et FP en mode relax
- Le `intern_to_final` mapping est incorrect pour l'un des deux

### Le fix

Prefixer les cles du ord_map avec "C:" (chunk) ou "W:" (word-stripped).
Ajouter `entry.text` pour garder le texte brut (sans prefix) pour le builder.

```rust
let map_key = if is_word_stripped {
    format!("W:{text}")
} else {
    format!("C:{text}")
};
ord_map.entry(map_key).or_insert_with(|| OrdEntry {
    text: text.clone(), // texte reel sans prefix
    ...
});
```

### Fichier

`src/suffix_fst/collector_v3.rs`, fonction `into_data()`, section ord_map.

### Resultat

- `function relax` FP : 49 → **1**
- Score maintenu a 13/15

## Fix 3 (en cours) : tokens BTreeSet → Vec

### Le bug

`build_derived_indexes_v3` itere `tokens.iter().enumerate()` pour mapper
ordinal → texte. Mais `tokens` etait un `BTreeSet<String>` (dedup) tandis que
`content_postings` et `own_lens` sont indexes par `final_ord` (un par entree
dans ord_map, y compris les doublons chunk/ws). Desalignement = bytemap/posmap
corrompus.

### Le fix (en cours)

`tokens: BTreeSet<String>` → `tokens: Vec<String>` avec 1:1 correspondance
avec `content_postings` et `own_lens`.

### Fichiers

- `src/suffix_fst/collector_v3.rs` — struct `SfxCollectorDataV3`, `into_data()`
- `src/suffix_fst/index_registry.rs` — `build_derived_indexes_v3` signature
- `src/indexer/sfx_dag_v3.rs` — `merge_segments_v3`

## Invariant a graver

**Toute structure qui mappe texte → donnee dans le collector doit etre
partitionnee par type d'entree (chunk vs word-stripped).**

Les structures concernees :
1. `token_intern` (HashMap cle d'interning) — **fixe** (prefix `\x00ws:`)
2. `ord_map` (BTreeMap cle d'ordinal final) — **fixe** (prefix `C:`/`W:`)
3. `tokens` (set/vec de textes) — **fixe** (Vec au lieu de BTreeSet dedup)
4. `content_key_to_interns` — deja partitionne (filtre `!is_word_stripped`)
5. `sorted_indices` — utilise par le builder qui filtre par `is_word_stripped`

**Danger futur** : si quelqu'un remplace le Vec par un BTreeSet pour "optimiser",
les ordinals chunk et word-stripped avec le meme texte seront fusionnes →
regression silencieuse des memes FN qu'on vient de corriger.

## Piste pour le fix

Le bug est dans `collector_v3.rs`, probablement dans `into_data()`. Le chunk "functio" + overlap "na" = extended "functiona" est interned normalement via `intern_extended("functiona", ...)`. Le posting est pushee dans `token_postings[intern_id]`. Mais quand `into_data()` construit le `ord_map`, soit :

1. L'intern_id n'est pas ajoute a `sorted_indices` (filtre par `is_word_stripped` ?)
2. Les postings sont ecrasees par un `dedup` incorrect
3. L'intern_id est remapped vers un ordinal final qui ne recoit pas les postings

### Prochaine etape

Instrumenter le collector : quand le texte etendu contient "functio", logger chaque etape (intern, posting push, into_data mapping). Env var `V3_DIAG_COLLECTOR` + le mot cible.

## Fichiers generes

- `/tmp/v3_ground_truth_report.txt` — rapport complet
- `/tmp/v3_ground_truth_fails.json` — queries en echec
- `/tmp/v3_dag_function_strict.json` — DAG explain par segment (80 segments)
- `/tmp/v3_forensics_function_strict.json` — forensics du doc FN
- `/tmp/v3_diag_build.txt` — diag multi-parent du builder

## Commits cette session

```
4efaa89 feat(luciole): add execute_sequential
3c96d49 feat(luciole): add LocalDag / LocalNode
0819d2b feat: find_literal_v3 as LocalDag — 9 nodes, builder, parity tests
1b270f8 feat: mermaid explain for DAG
017e9ea feat: edge annotations for DAG explain
819c2dc diag: DAG explain per segment in V3_DIAG mode (fix sfx_file)
c985f45 diag: deep DAG explain — FST keys + per-candidate postings
dd8b24e diag: doc forensics — find FN doc segment, tokenize, check candidates + postings
321a94b diag: deep forensics — wider prefix scan + reverse ordinal scan
```
● VOILA LE BUG !                                     
                                                                             
  intern_extended retourne l'ord existant si le texte existe deja (ligne     
  507-508), mais il ne met PAS a jour les meta. La meta est seulement        
  enregistree a la PREMIERE insertion (ligne 514).                           
                                                                             
  Si le texte "functional" a ete interned en premier par la word-stripped    
  path (avec is_word_stripped: true), alors l'intern_id 3545 a meta          
  is_word_stripped: true. Quand le chunk "function" + overlap "al" essaie de 
  s'interner avec le meme texte "functional" mais is_word_stripped: false, il
   recoit l'intern_id deja existant avec les meta word_stripped.

  Ensuite, into_data() fait if self.token_meta[io as usize].is_word_stripped 
  { continue; } → saute l'intern 3545 parce que ses meta disent
  word_stripped. Les postings du chunk (doc 30) sont ignorees.               
                                                                           
  C'est le bug. Un conflit de meta quand le meme texte etendu est interned a 
  la fois comme chunk et comme word-stripped.
                                                                             
  Tu veux qu'on fixe ca maintenant ?                                         
   
✻ Cogitated for 1m 11s                          