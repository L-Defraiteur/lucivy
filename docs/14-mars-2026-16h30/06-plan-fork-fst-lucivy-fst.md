# Plan d'action : fork du FST — lucivy-fst

Date : 14 mars 2026 — 16h30

Statut : plan d'action validé, prêt à implémenter

## Base du fork

**Crate source** : `fst` (BurntSushi) v0.4.7
- ~4-5K LOC core (`src/raw/`), ~12K total
- MIT/Unlicense
- Cloné localement : `ld-lucivy/lucivy-fst/`
- On forke `fst` directement (pas `tantivy-fst` qui n'est qu'un clone cosmétique)

## Objectif

Créer un crate `lucivy-fst` qui :
1. Résout le problème multi-output (BUG-1/2 Unicode, multi-parent inline)
2. Maintient une **API compatible** avec l'existant (drop-in replacement)
3. Reste **généraliste** (full UTF-8, pas de restriction ASCII)
4. Prépare les optimisations futures (compression code, WASM layout, merge)

## Architecture actuelle du FST (résumé)

```
Map<D> / Set<D>              ← API publique (get, range, search, stream)
  └─ raw::Fst<D>             ← automate core (nodes, transitions, outputs)
       └─ &[u8] contiguous   ← format binaire immutable (mmap-friendly)

Output(u64)                  ← valeur unique par terme, additive le long du chemin
  .cat(o) = self + o         ← concaténation (addition)
  .prefix(o) = min(self, o)  ← préfixe commun
  .sub(o) = self - o         ← soustraction

Builder :
  - Termes en ordre lexicographique strict (vérifié, erreur sinon)
  - UnfinishedNodes stack (nœuds en construction, left-to-right)
  - Registry (LRU hash table, 10K entrées) → déduplication de sous-arbres
  - Streaming O(n) avec mémoire bornée

Node format binaire :
  - EmptyFinal              : 0 byte (adresse spéciale)
  - OneTransNext            : 1-2 bytes (1 transition, output=0, next node)
  - OneTrans                : variable (1 transition, output + adresse)
  - AnyTrans                : variable (N transitions, index table si N>32)
  - Varint packing (1-8 bytes), delta encoding des adresses
  - Common inputs table (64 bytes fréquents, 6 bits)

Automaton trait :
  - start() → State
  - accept(state, byte) → State
  - is_match(state) → bool
  - can_match(state) → bool      ← pruning (skip subtrees)
  - will_always_match(state)     ← fast path (accept tout)
```

## Surface API utilisée par lucivy

### Construction (MapBuilder)
```rust
MapBuilder::new(W: Write) → Result<MapBuilder<W>>
MapBuilder::memory() → MapBuilder<Vec<u8>>
.insert(key: &[u8], value: u64) → Result<()>
.into_inner() → Result<W>
```

Utilisé par : TermDictionary, SuffixFstBuilder, SSTable index v3

### Lecture (Map)
```rust
Map::from_bytes(bytes) → Result<Map<OwnedBytes>>
Map::from(fst: Fst<T>) → Map<T>
.get(key: &[u8]) → Option<u64>
.range() → StreamBuilder       // range queries
.search(automaton) → StreamBuilder  // automaton walk
.stream() → Stream             // full iteration
.as_fst() → &Fst<T>           // accès raw pour navigation
```

### Raw FST navigation (ord_to_term)
```rust
Fst::new(bytes) → Result<Fst>
.root() → Node
.node(addr) → Node
Node::is_final() → bool
Node::transitions() → Iterator<Transition>
Transition { inp: u8, out: Output, addr: NodeAddr }
Output::value() → u64
```

### Automaton trait
```rust
trait Automaton {
    type State: Clone;
    fn start(&self) → Self::State;
    fn is_match(&Self::State) → bool;
    fn can_match(&Self::State) → bool;
    fn accept(&Self::State, byte: u8) → Self::State;
    fn will_always_match(&Self::State) → bool;
}
```

Implémenté par : DfaWrapper (Levenshtein), FuzzySubstringAutomaton, Regex,
SetDfaWrapper, Str, AlwaysMatch

### Merge (OpBuilder)
```rust
OpBuilder::new().push(stream).union() → Union
Union::next() → Option<(&[u8], &[IndexedValue])>
IndexedValue { index: usize, value: u64 }
```

Utilisé par : merger de TermDictionary entre segments

## Problèmes que le fork résout

### P1 — Multi-output (BUG-1/2 Unicode + multi-parent)

**Situation actuelle** :
- Output = 1x u64 par terme
- Pour le .sfx : on encode (raw_ordinal, SI) dans le u64
- Problème : un même suffix lowercase peut avoir des SI différents selon
  l'original (İMPORT vs IMPORT → byte widths différents)
- Problème : les suffix multi-parent (5% des cas) nécessitent une table
  annexe (parent_list) avec un offset dans le u64

**Avec le fork** :
- Output = liste de valeurs par terme (varint-encoded)
- Chaque variante Unicode a son propre (raw_ordinal, SI_bytes) inline
- Les multi-parents sont aussi inline (plus de parent_list séparée)
- Le TermDictionary standard continue d'utiliser 1x u64 (compatible)

### P2 — Compression code source

**Situation actuelle** :
- Registry 10K entrées, heuristique texte naturel
- Common inputs table : 64 bytes (lettres, chiffres, ponctuation)

**Avec le fork** :
- Registry tuneable (taille paramétrable)
- Common inputs table étendue ou adaptative
- Pas de restriction ASCII — toute la surface UTF-8 fonctionne
- Les optimisations sont des bonus de compression, pas des restrictions

### P3 — WASM layout (futur)

- Supprimer le code mmap optionnel
- Varint 32-bit suffisant (WASM = 32-bit address space)
- Alignement compact

## Plan d'implémentation

### Phase F1 — Scaffolding (crate lucivy-fst)

**Objectif** : crate compilable, tests passent, API identique

1. Renommer le crate : `fst` → `lucivy-fst`
2. Mettre à jour Cargo.toml (name, version 0.1.0, edition 2021)
3. Supprimer les binaires (`fst-bin/`) et exemples non pertinents
4. Supprimer la dépendance optionnelle `memmap2` (pas besoin, on a OwnedBytes)
5. `cargo test` — tous les tests existants doivent passer
6. Vérifier que `cargo build --target wasm32-unknown-emscripten` compile

**Fichiers modifiés** : Cargo.toml, lib.rs (renommage)
**Risque** : faible, c'est du renommage

### Phase F2 — Output générique

**Objectif** : remplacer `Output(u64)` par un trait `FstOutput`

```rust
pub trait FstOutput: Clone + Eq + Default {
    /// Encode vers bytes pour stockage dans le FST
    fn encode(&self, wtr: &mut Vec<u8>);
    /// Decode depuis bytes
    fn decode(data: &[u8]) -> (Self, usize); // (value, bytes_consumed)
    /// Taille en bytes de l'encoding
    fn encoded_size(&self) -> usize;

    /// Algèbre FST (nécessaire pour la construction)
    fn cat(&self, other: &Self) -> Self;     // concaténation
    fn prefix(&self, other: &Self) -> Self;  // préfixe commun
    fn sub(&self, other: &Self) -> Self;     // soustraction
    fn is_zero(&self) -> bool;
}
```

**Implémentation par défaut** : `SingleOutput(u64)` — comportement identique
à l'actuel. `cat = +`, `prefix = min`, `sub = -`.

**Nouvelle implémentation** : `MultiOutput(Vec<u64>)` — liste de valeurs.
Algèbre : `cat = elementwise +`, `prefix = elementwise min`,
`sub = elementwise -`. Ou bien, plus simple :

```rust
/// Output = offset dans une table externe (pour multi-output)
/// La table est stockée dans le .sfx file, pas dans le FST
pub struct TableOutput(u64);
```

**Stratégie** : on paramétrise le FST par le type Output (`Fst<D, O>`), avec
un default `O = SingleOutput` pour la rétro-compatibilité.

**Fichiers modifiés** :
- `src/raw/mod.rs` : `Output(u64)` → trait + struct
- `src/raw/build.rs` : Builder paramétré par O
- `src/raw/node.rs` : encoding/decoding paramétré
- `src/raw/ops.rs` : merge paramétré
- `src/map.rs` : Map paramétré
- `src/bytes.rs` : helpers d'encoding

**Risque** : moyen. L'algèbre additive est fondamentale dans le builder.
Il faut que le trait préserve les invariants (associativité, élément neutre).

**Alternative plus simple** : garder `Output(u64)` mais ajouter une méthode
`Map::get_raw(key) -> Option<(u64, &[u8])>` qui retourne aussi les bytes bruts
du nœud final, permettant un encoding custom dans les bytes du nœud.

### Phase F3 — MultiOutput concret pour le .sfx

**Objectif** : le .sfx utilise le FST forké avec multi-output

Deux approches possibles :

**Approche A — Output = offset dans table externe (recommandée)**

Le FST reste à 1x u64 par terme. Le u64 est un offset dans une table
binaire annexe stockée dans le .sfx file. La table contient les variantes :

```
FST :  "mport" → offset=42

Table[42] :
  count=2
  entry[0] = { raw_ordinal=5, si_bytes=1 }  // pour "IMPORT"
  entry[1] = { raw_ordinal=5, si_bytes=2 }  // pour "İMPORT"
```

- Le FST ne change pas de format (toujours u64 additif)
- La table annexe est un simple Vec<u8> sérialisé
- Le SfxFileReader résout offset → entries
- Pattern identique au TermInfoStore de tantivy (terme → offset → TermInfo)

**Avantages** : minimum de modifications au FST. Le format binaire du FST
est préservé. La rétro-compatibilité est triviale.

**Inconvénients** : un indirection en plus (offset → table → entries).
Mais c'est un seul random access dans un buffer contiguous (cache-friendly).

**Approche B — Output = varint list inline**

Le FST stocke N valeurs par terme directement dans les nœuds. Nécessite
de modifier le format binaire : chaque transition et chaque final_output
peut avoir une taille variable.

**Avantages** : zéro indirection, tout est dans le FST.

**Inconvénients** : casse le format binaire, complexifie le builder (comment
accumuler additivement des listes ?), casse la déduplication du registry
(deux nœuds avec des listes de tailles différentes ne peuvent plus être
identiques).

**Recommandation** : Approche A. Le pattern offset-dans-table est prouvé
(TermInfoStore) et ne nécessite quasi aucune modification du FST core.
Le seul changement côté FST : rien. Tout est dans le SfxFileReader.

### Phase F4 — Adaptation du SuffixFstBuilder

**Objectif** : le SuffixFstBuilder utilise lucivy-fst au lieu de tantivy-fst

1. Remplacer `tantivy_fst` par `lucivy_fst` dans le .sfx builder
2. Le builder produit : FST bytes + table multi-output bytes
3. Le SfxFileWriter sérialise les deux dans le .sfx file (nouvelle section D)
4. Le SfxFileReader lit la table et résout les offsets

**Format .sfx mis à jour** :
```
HEADER (magic "SFX2", version, offsets...)
SECTION A : Suffix FST (lucivy-fst format)
SECTION B : Parent list (inchangé pour single-parent, deprecated pour multi)
SECTION C : GapMap (inchangé)
SECTION D : MultiOutput table (nouveau)
  - Per entry : count (varint) + N × (raw_ordinal: u24, si_bytes: u8)
```

### Phase F5 — Migration du TermDictionary

**Objectif** : le TermDictionary principal utilise lucivy-fst

1. Remplacer `tantivy_fst` par `lucivy_fst` dans Cargo.toml
2. Avec `SingleOutput(u64)`, le comportement est identique
3. Tous les tests existants passent sans modification
4. SSTable index, merger, fuzzy query : même API, même types

**Risque** : faible si Phase F2 est bien faite (trait avec default u64).

### Phase F6 — Optimisations compression (futur)

1. Registry taille paramétrable (constructeur avec options)
2. Common inputs table adaptative (analyser le corpus au build)
3. Benchmark sur corpus code source réel (5201 docs rag3db)
4. Comparer taille FST : lucivy-fst vs tantivy-fst

### Phase F7 — WASM layout (futur)

1. Feature flag `wasm` : supprimer code mmap, aligner 32-bit
2. Benchmark taille WASM : lucivy-fst vs tantivy-fst
3. Intégrer dans le build emscripten existant

## Ordre d'exécution et dépendances

```
F1  Scaffolding         ← aucune dépendance, on commence par là
 ↓
F2  Output générique    ← F1
 ↓
F3  MultiOutput .sfx    ← F2 (+ résout BUG-1/2 et multi-parent)
 ↓
F4  Adaptation builder  ← F3
 ↓
F5  Migration TermDict  ← F2 (indépendant de F3/F4)
 ↓
F6  Compression         ← F5, benchmarks
 ↓
F7  WASM layout         ← F5
```

**Chemin critique** : F1 → F2 → F3 → F4 (résout les bugs)
**Chemin secondaire** : F2 → F5 (migration sans multi-output)

## Impact sur les phases existantes

| Phase existante | Impact du fork |
|----------------|---------------|
| **PHASE-6** Branchement inverted index réel | Indépendant. Peut avancer en parallèle avec tantivy-fst. |
| **PHASE-4** Multi-token search | Indépendant. La logique de search ne dépend pas du type d'output. |
| **PHASE-5** Fuzzy d>0 | Indépendant. L'Automaton trait est préservé. |
| **PHASE-7** Normalisation Unicode BUG-1/2 | **Remplacée par F3**. Le multi-output résout le problème à la racine. Plus besoin du ByteWidthPreservingFilter. |
| **PHASE-8** Merger .sfx | Dépend de F4 (format .sfx mis à jour). |
| **PHASE-9** Supprimer ._ngram | Indépendant. |
| **PHASE-10** Benchmark | Inclut F6. |

## Estimation de complexité

| Phase | LOC estimé | Complexité |
|-------|-----------|------------|
| F1 | ~50 (config) | Faible |
| F2 | ~200-400 (trait + refactor) | Moyenne |
| F3 | ~150 (table + reader) | Faible-Moyenne |
| F4 | ~100 (adaptation builder) | Faible |
| F5 | ~50 (swap dépendance) | Faible |
| F6 | ~200 (registry, benchmarks) | Moyenne |
| F7 | ~100 (feature flags) | Faible |

**Total** : ~850-1050 LOC de modifications/ajouts.
Le gros du travail est en F2 (output générique) — le reste en découle.

## Décision clé : Approche A vs B pour multi-output

**Approche A (offset dans table externe)** :
- Le FST garde son format u64 standard
- La résolution multi est dans une table annexe (section D du .sfx)
- Quasi zéro modification du FST core
- Pattern prouvé (TermInfoStore)

**Approche B (output inline multi-valué)** :
- Le FST stocke N valeurs par nœud
- Modifie le format binaire (version bump)
- Complexifie le builder et la déduplication
- Plus performant (zéro indirection) mais plus risqué

**Recommandation : Approche A pour le .sfx, output générique (F2) pour le
long terme.** On commence par la table externe (simple, prouvé), puis si les
benchmarks montrent que l'indirection coûte, on passe en inline (F2 rend ça
possible).

## Relation avec l'API existante

L'API haut niveau reste la même :

```rust
// Construction (identique)
let mut builder = MapBuilder::memory();
builder.insert(b"mport", offset_42)?;
let fst_bytes = builder.into_inner()?;

// Lecture (identique)
let map = Map::from_bytes(&fst_bytes)?;
let offset = map.get(b"mport").unwrap();  // → 42

// Automaton walk (identique)
let stream = map.search(my_automaton).into_stream();
while let Some((key, value)) = stream.next() { ... }

// La résolution multi-output est HORS du FST :
let entries = multi_output_table.resolve(offset);
// → [(ordinal=5, si=1), (ordinal=5, si=2)]
```

Aucune surprise pour le code existant. Le changement est dans le .sfx reader,
pas dans le FST.
