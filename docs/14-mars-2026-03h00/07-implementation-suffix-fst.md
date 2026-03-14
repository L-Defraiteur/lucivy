# Plan d'implémentation : Suffix FST avec redirection

Date : 14 mars 2026

Référence : doc 06 (design final)

## Approche

5 phases progressives. Chaque phase est testable en isolation avant de passer
à la suivante. On commence par les structures de données pures (zéro dépendance
lucivy), puis on intègre dans l'indexation, la recherche, et enfin le merger.

## Phase 1 — Structures de données pures (isolation totale)

Nouveau crate ou module `src/suffix_fst/`. Aucune dépendance sur le reste de
lucivy. Testable avec `cargo test` seul.

### Étape 1.1 — SuffixFstBuilder

Construit le FST des suffixes à partir d'une liste de tokens.

```rust
// Nouveau : src/suffix_fst/builder.rs

pub struct SuffixEntry {
    pub raw_term: String,       // token parent (lowercase)
    pub raw_ordinal: u64,       // ordinal du parent dans le FST ._raw
}

pub struct SuffixFstBuilder {
    // Accumule : suffix_term → Vec<(raw_ordinal, SI)>
    // Uniquement les termes uniques + leurs parents, pas les occurrences.
    suffix_to_parents: BTreeMap<String, Vec<(u64, u16)>>,
    min_suffix_len: usize,  // défaut = 3
}

impl SuffixFstBuilder {
    pub fn new() -> Self { Self::with_min_suffix_len(3) }
    pub fn with_min_suffix_len(min: usize) -> Self;

    /// Enregistre tous les suffixes d'un token.
    /// Appelé une fois par token UNIQUE du segment (pas par occurrence).
    /// Le builder ne stocke que les paires (suffix → parent), pas les
    /// posting data (qui restent dans ._raw).
    pub fn add_token(&mut self, token: &str, raw_ordinal: u64) {
        let lower = token.to_lowercase();
        for si in 0..lower.len() {
            if !lower.is_char_boundary(si) { continue; }
            let suffix = &lower[si..];
            if suffix.len() < self.min_suffix_len { break; } // suffixes trop courts
            self.suffix_to_parents
                .entry(suffix.to_string())
                .or_default()
                .push((raw_ordinal, si as u16));
        }
    }

    /// Construit le FST + parent lists.
    /// Retourne les bytes du FST et les bytes des parent lists.
    pub fn build(self) -> (Vec<u8>, Vec<u8>) {
        // Le BTreeMap est déjà trié (requis par le FST builder).
        // Pour chaque suffix :
        //   - 1 parent → encoder (raw_ordinal, SI) dans le u64 output
        //   - N parents → écrire dans parent_list, encoder offset dans u64
        // Utiliser tantivy-fst::MapBuilder
    }
}
```

**Encoding u64 :**
```
Bit 63 = 0 : single parent
  bits 0-23  = raw_ordinal (jusqu'à ~16M tokens)
  bits 24-31 = SI (jusqu'à 256 chars)

Bit 63 = 1 : multi-parent
  bits 0-31  = offset dans parent_list bytes
```

**min_suffix_len = 3** : les suffixes de 1-2 chars ne sont pas indexés
(trop de multi-parents, quasi jamais queried). Queries < 3 chars →
fallback prefix walk ._raw.

**Tests :**
- Round-trip : add_token → build → lookup FST → vérifier parent + SI
- Multi-parent : deux tokens finissant par le même suffix → vérifier liste
- UTF-8 : tokens avec accents, emojis → char boundaries correctes
- Token court (1-2 chars) → suffixes de 1 char

### Étape 1.2 — GapMapWriter / GapMapReader

Format binaire des séparateurs. Réutilise le format du doc 01/06.

```rust
// Nouveau : src/suffix_fst/gapmap.rs

pub struct GapMapWriter {
    data: Vec<u8>,
    doc_offsets: Vec<u64>,
}

impl GapMapWriter {
    pub fn new() -> Self;

    /// Enregistre les gaps d'un document.
    /// gaps = [prefix, sep_0, sep_1, ..., suffix]
    /// gaps.len() = num_tokens + 1
    pub fn add_doc(&mut self, gaps: &[&[u8]]);

    /// Sérialise : header + offset table + data.
    pub fn serialize(&self) -> Vec<u8>;
}

pub struct GapMapReader<'a> {
    data: &'a [u8],  // mmap'd
}

impl<'a> GapMapReader<'a> {
    pub fn open(data: &'a [u8]) -> Self;

    /// Lit le séparateur à la position gap_index pour le doc doc_id.
    /// gap_index = 0 → prefix (avant token 0)
    /// gap_index = Ti + 1 → séparateur après token Ti
    pub fn read_gap(&self, doc_id: u32, gap_index: u32) -> &'a [u8];

    /// Nombre de tokens dans le doc.
    pub fn num_tokens(&self, doc_id: u32) -> u16;
}
```

**Tests :**
- Round-trip : write → serialize → open → read_gap → vérifier bytes
- Gaps vides (tokens collés, pas de séparateur)
- Gaps longs (> 254 bytes → extended length)
- Doc sans tokens (num_tokens = 0)
- Accès multi-docs (offset table correct)

### Étape 1.3 — SfxFileWriter / SfxFileReader

Assemble FST + parent lists + GapMap dans un seul fichier `.sfx`.

```rust
// Nouveau : src/suffix_fst/file.rs

pub struct SfxFileWriter {
    fst_data: Vec<u8>,
    parent_list_data: Vec<u8>,
    gapmap_data: Vec<u8>,
}

impl SfxFileWriter {
    pub fn new(fst: Vec<u8>, parent_lists: Vec<u8>, gapmap: Vec<u8>) -> Self;

    /// Écrit le fichier .sfx complet : header + sections.
    pub fn write<W: Write>(&self, writer: &mut W) -> io::Result<()>;
}

pub struct SfxFileReader<'a> {
    data: &'a [u8],         // mmap'd
    fst: tantivy_fst::Map,  // FST des suffixes
    parent_list: &'a [u8],  // section parent lists
    gapmap: GapMapReader<'a>,
}

impl<'a> SfxFileReader<'a> {
    pub fn open(data: &'a [u8]) -> Self;

    /// Résout un suffix vers ses parents.
    /// Retourne Vec<(raw_ordinal, SI)>.
    pub fn resolve_suffix(&self, suffix: &str) -> Vec<(u64, u16)>;

    /// Prefix walk : trouve tous les suffixes commençant par `prefix`.
    /// Retourne un itérateur de (suffix_term, Vec<(raw_ordinal, SI)>).
    pub fn prefix_walk(&self, prefix: &str) -> impl Iterator<Item = (String, Vec<(u64, u16)>)>;

    /// Accès GapMap.
    pub fn gapmap(&self) -> &GapMapReader<'a>;
}
```

**Tests :**
- Round-trip complet : builder → file → reader → resolve → vérifier
- Magic bytes + version → reject fichier invalide
- Prefix walk → vérifier tous les suffixes retournés

### Étape 1.4 — Tests d'intégration Phase 1

Test bout-en-bout sans lucivy :

```rust
#[test]
fn test_suffix_fst_full_flow() {
    // Simuler un "document" :
    // tokens = ["import", "rag3db", "from", "rag3db", "core"]
    // gaps = ["", " ", " ", " '", "_", "';"]

    // 1. Construire SuffixFstBuilder avec raw_ordinals simulés
    // 2. Construire GapMapWriter avec les gaps
    // 3. Écrire le .sfx
    // 4. Ouvrir le .sfx
    // 5. Vérifier :
    //    - resolve_suffix("g3db") → parent "rag3db" ordinal, SI=2
    //    - prefix_walk("g3d") → trouve "g3db"
    //    - gapmap.read_gap(0, 4) → "_"
    //    - resolve_suffix("core") → parent "core" ordinal, SI=0
    //    - resolve_suffix("e") → multi-parent (suffix de "core" et "import")
}
```

## Phase 2 — Intégration indexation

Brancher le SuffixFstBuilder dans le segment writer pour que chaque segment
produise un fichier `.sfx` à côté des fichiers existants.

### Étape 2.1 — SegmentComponent::SuffixFst

```
Fichier : src/index/segment_component.rs (lignes 9-52)

Ajouter :
  SuffixFst  // variante de l'enum

Fichier : src/index/index_meta.rs (lignes 134-148)

Ajouter dans relative_path() :
  SegmentComponent::SuffixFst => ".sfx"

Mettre à jour iterator() pour inclure SuffixFst.
```

### Étape 2.2 — Capturer les tokens et gaps dans le segment writer

```
Fichier : src/indexer/segment_writer.rs (lignes 194-222)

Au moment où index_document() traite un champ texte FieldType::Str :
  - Le texte brut est disponible via value.as_str() (ou le flux de tokens)
  - Ajouter : pour chaque token du flux, capturer (texte, offset_from, offset_to)
  - Calculer les gaps entre tokens : texte[offset_to_prev..offset_from_curr]
  - Accumuler dans un SuffixFstCollector par champ

Nouveau struct SuffixFstCollector :
  - Accumulé au fil des documents
  - Per doc : tokens + gaps → GapMapWriter.add_doc()
  - Per token unique : token texte + raw_ordinal → SuffixFstBuilder.add_token()
  - Le raw_ordinal vient du FST ._raw du même segment (disponible au close)

Note : le raw_ordinal n'est connu qu'après la sérialisation du ._raw.
Deux options :
  A. Deux passes : d'abord indexer normalement, puis relire le FST ._raw
     pour obtenir les ordinals et construire le .sfx
  B. Accumuler les termes raw en mémoire, trier, assigner les ordinals
     au moment du build du .sfx (même ordre que le FST ._raw)

Option B est plus simple : les termes dans le FST sont triés
alphabétiquement, donc ordinal = position dans l'ordre trié.
On accumule un BTreeSet<String> des termes raw, et l'ordinal d'un terme
= son rang dans le set trié. Pas besoin de lire le FST ._raw.
```

### Étape 2.3 — Écrire le .sfx au close du segment

```
Fichier : src/indexer/segment_serializer.rs (lignes 21-46, 80-88)

Dans for_segment() :
  - Ouvrir le WritePtr pour SegmentComponent::SuffixFst

Dans close() :
  - Finaliser le SuffixFstCollector :
    collector.build() → SfxFileWriter
  - Écrire via le WritePtr
```

### Étape 2.4 — Tests Phase 2

```rust
// Dans les tests d'indexation existants ou nouveau test :
// 1. Créer un index avec quelques documents
// 2. Commit
// 3. Vérifier que le fichier .sfx existe dans le segment
// 4. Ouvrir le .sfx et vérifier les suffixes / gaps
// 5. Vérifier la cohérence : resolve_suffix("g3db") → raw_ordinal
//    → lookup ._raw FST par ordinal → terme "rag3db" ✓
```

## Phase 3 — Recherche single token contains

Remplacer la vérification stored text par le .sfx dans NgramContainsQuery.

### Étape 3.1 — Ouvrir le .sfx dans le SegmentReader

```
Le SegmentReader ouvre les fichiers du segment. Ajouter l'ouverture du .sfx :
  - Si le fichier .sfx existe → mmap → SfxFileReader
  - Si absent → None (fallback stored text pour les vieux segments)
```

### Étape 3.2 — SuffixContainsQuery (nouveau query type)

```rust
// Nouveau : src/query/suffix_contains_query.rs

// Remplace NgramContainsQuery pour les segments avec .sfx.
// Flow pour single token contains "g3d" d=0 :
//
// 1. sfx_reader.prefix_walk("g3d")
//    → [(suffix_term="g3db", parents=[(raw_ord=3, SI=2)])]
//
// 2. Pour chaque parent :
//    raw_fst.ord_to_term(3) → "rag3db"  (optionnel, pour debug)
//    raw_inverted_index.posting_list(raw_ord=3)
//    → [(doc=42, Ti=1, byte_from=7), (doc=42, Ti=3, byte_from=20)]
//
// 3. Ajuster byte_from += SI=2 :
//    → [(doc=42, Ti=1, byte_from=9), (doc=42, Ti=3, byte_from=22)]
//
// 4. Construire résultat : doc_ids + highlights + BM25 score

// Le score BM25 vient du champ principal (inchangé).
// Les highlights viennent de byte_from + len(query).
```

**Point clé :** la posting list du ._raw est accessible via l'inverted index
existant du champ ._raw. L'ordinal du FST ._raw donne directement accès
à la posting list correspondante (c'est le term_ordinal standard de lucivy).

```
Fichiers impactés :
  src/query/phrase_query/ngram_contains_query.rs (lignes 277-374)
    → NgramContainsWeight::scorer() : si .sfx disponible, utiliser
      SuffixContainsQuery au lieu du flow trigram + stored text

  src/query/phrase_query/contains_scorer.rs
    → Nouveau scorer qui utilise le .sfx
```

### Étape 3.3 — Tests Phase 3

```
Tests de recherche avec comparaison :
  1. Indexer des documents (le .sfx est écrit automatiquement)
  2. contains "g3d" → résultat via .sfx
  3. Vérifier : mêmes doc_ids, mêmes highlights que l'implémentation actuelle
  4. Mesurer : temps de réponse (objectif < 1ms vs ~300ms actuel)

Tests edge cases :
  - Query = 1 char ("a") → prefix walk, tout SI
  - Query = token entier ("rag3db") → SI=0 match direct
  - Query plus long que tous les tokens → aucun résultat
  - Query avec chars multi-byte (UTF-8)
  - Token très long (100+ chars) → beaucoup de suffixes
```

## Phase 4 — Recherche avancée

### Étape 4.1 — Fuzzy single token (d>0)

```
Brancher le Levenshtein DFA existant sur le suffix FST du .sfx.

Le code existe déjà pour startsWith fuzzy :
  src/query/automaton_phrase_query.rs — AutomatonPhraseQuery
  FuzzyTermQuery::new_prefix() utilise un DFA Levenshtein

Pour contains fuzzy : même DFA, mais walk sur le .sfx FST au lieu du ._raw.
Le DFA parcourt les suffixes, trouve les termes à distance ≤ d,
résout les parents, fetch les posting lists ._raw.

Fichier : probablement dans le nouveau suffix_contains_query.rs
```

### Étape 4.2 — Multi-token

```
Flow pour contains "g3db is a cool fram" d=0 :

Premier token "g3db" (tout SI) :
  .sfx exact lookup "g3db" → parent "rag3db" SI=2
  → ._raw posting "rag3db" → curseur A

Tokens milieu "is", "a", "cool" (SI=0) :
  ._raw exact lookup chacun → curseurs B, C, D

Dernier token "fram" (SI=0, prefix) :
  .sfx prefix walk "fram" filtrer SI=0 → parent "framework"
  → ._raw posting "framework" → curseur E

Intersection curseurs :
  Merge trié A ∩ B ∩ C ∩ D ∩ E sur (doc_id, Ti consécutifs)

Vérification premier token fin de mot :
  SI + len(suffix) == len(parent) → 2 + 4 == 6 ✓

GapMap (si strict) :
  sfx_reader.gapmap().read_gap(doc, Ti+1) → séparateur → check
```

### Étape 4.3 — Tests Phase 4

```
Fuzzy :
  - contains "rag3db" d=1 → trouve aussi "rag3dc" etc.
  - contains "g3d" d=1 → résultats cohérents

Multi-token :
  - "rag3db core" → match avec check séparateur
  - "g3db is a cool fram" → premier=suffix, dernier=prefix
  - "import rag3db" → avec séparateur " "
  - "rag3db core" avec séparateur "_" → rejeté en strict, OK en relaxed

Benchmarks :
  - Comparer avec l'implémentation actuelle (stored text)
  - Objectif single token d=0 : < 1ms (vs ~300ms)
  - Objectif single token d=1 : < 5ms (vs ~1400ms)
  - Objectif multi-token d=0 : < 2ms
```

## Phase 5 — Merger + nettoyage

### Étape 5.1 — Merger .sfx

```
Fichier : src/indexer/merger.rs

Le .sfx ne contient PAS de posting data liée aux doc_ids.
Il contient :
  - FST suffixes → raw_ordinals (les ordinals changent au merge !)
  - Parent lists → raw_ordinals
  - GapMap → indexée par doc_id (les doc_ids changent au merge !)

Stratégie : reconstruire le .sfx du segment mergé from scratch.

Pour le FST + parent lists :
  - Le merged segment a un nouveau FST ._raw avec de nouveaux ordinals
  - Relire les termes du FST ._raw mergé
  - Reconstruire le SuffixFstBuilder avec les nouveaux ordinals
  - C'est un rebuild complet mais c'est rapide (CPU only, pas d'I/O)

Pour la GapMap :
  - Lire les GapMaps des segments source
  - Concaténer dans le nouveau doc_id order (après remapping)
  - Pattern similaire à write_storable_fields() (merger.rs lignes 514-546)
```

### Étape 5.2 — Supprimer ._ngram

```
Fichier : lucivy_core/src/handle.rs (lignes 273-275)

Le champ ._ngram n'est plus nécessaire. Le .sfx le remplace.

Retirer :
  - La création du champ ._ngram dans build_schema()
  - L'enregistrement du NGRAM_TOKENIZER (lignes 357-362)
  - ngram_field_pairs dans LucivyHandle
  - Le paramètre ngram_field_pairs dans build_query()

Fichiers impactés :
  - lucivy_core/src/handle.rs — build_schema(), configure_tokenizers()
  - lucivy_fts/rust/src/bridge.rs — auto_duplicate_field() (lignes 275-303)
  - lucivy_fts/rust/src/query.rs — build_query() ngram_field_pairs param
  - src/tokenizer/ngram_tokenizer.rs — peut être marqué dead_code ou supprimé
  - Tous les bindings qui passent ngram_field_pairs

Impact : réduit la taille de l'index (plus de FST ngram + posting lists ngram)
et le temps d'indexation (plus de tokenisation ngram).
```

### Étape 5.3 — Compatibilité

```
Les segments existants (sans .sfx) doivent continuer à fonctionner.

Stratégie :
  - Si le segment a un .sfx → utiliser SuffixContainsQuery
  - Sinon → fallback sur NgramContainsQuery (stored text, code actuel)
  - Au merge, les vieux segments sont convertis (le .sfx est reconstruit)
  - Après un merge complet, tous les segments ont un .sfx
```

### Étape 5.4 — Tests Phase 5

```
Merger :
  - Créer 2 segments avec .sfx → merger → vérifier .sfx mergé
  - Search sur le segment mergé → résultats corrects
  - Vérifier que les raw_ordinals sont corrects après merge

Compatibilité :
  - Ouvrir un vieil index (sans .sfx) → fallback stored text ✓
  - Merger vieux segment + nouveau segment → .sfx reconstruit
  - Après merge → search utilise .sfx

Suppression ._ngram :
  - Créer un index sans ._ngram → taille réduite
  - contains search fonctionne via .sfx seul
```

## Résumé des fichiers

```
NOUVEAUX :
  src/suffix_fst/mod.rs           — module principal
  src/suffix_fst/builder.rs       — SuffixFstBuilder
  src/suffix_fst/gapmap.rs        — GapMapWriter / GapMapReader
  src/suffix_fst/file.rs          — SfxFileWriter / SfxFileReader
  src/query/suffix_contains_query.rs — SuffixContainsQuery

MODIFIÉS :
  src/index/segment_component.rs  — + SuffixFst variant
  src/index/index_meta.rs         — + ".sfx" extension
  src/indexer/segment_writer.rs   — capturer tokens + gaps
  src/indexer/segment_serializer.rs — écrire le .sfx
  src/indexer/merger.rs           — merger les .sfx
  src/query/phrase_query/ngram_contains_query.rs — router vers .sfx si dispo
  lucivy_core/src/handle.rs       — retirer ._ngram (phase 5)
  lucivy_fts/rust/src/bridge.rs   — retirer ngram duplication (phase 5)

DÉPENDANCES :
  tantivy-fst = "0.5"             — DÉJÀ dans Cargo.toml ✓ (pas de nouvelle dep)
```

## Risques

### Taille du FST suffixes

Estimation ~15-20MB mais à valider sur corpus réel. Si les identifiants sont
très longs (30+ chars), le nombre de suffixes explose. Mitigation : limiter
la longueur max des suffixes indexés (ex: max 64 chars).

### Ordinals ._raw au build time

Le SuffixFstBuilder a besoin des raw_ordinals. Ceux-ci ne sont connus qu'après
la construction du FST ._raw. Solution : accumuler les termes raw triés
(BTreeSet), l'ordinal = rang dans l'ordre trié = même ordre que le FST.

### UTF-8 char boundaries

Les suffixes doivent commencer sur des frontières de caractères UTF-8.
`str::is_char_boundary(si)` filtre les positions invalides.

### Performance du prefix walk

Si un prefix court ("a") matche beaucoup de suffixes, le walk retourne
beaucoup de termes → beaucoup de parents → beaucoup de posting list reads.
Mitigation : le même pattern existe déjà pour startsWith avec un prefix
court, et c'est géré par le scoring + early termination.
