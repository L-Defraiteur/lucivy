# Design final : Suffix FST avec redirection — contains search

Date : 14 mars 2026

Statut : **design final**, remplace docs 02/03/04/05

## Problème

La recherche contains est lente à cause de la vérification par stored text (~300ms d=0,
~1400ms d=1 sur 5201 docs). Les designs précédents (token map + ngram positions composées)
ajoutaient de la complexité et un fichier .tmap de ~450MB.

## Idée : suffix indexation avec redirection vers ._raw

Deux idées combinées :

1. **Suffix FST** : indexer tous les suffixes de chaque token dans un FST. Une recherche
   contains devient un prefix walk sur ce FST.

2. **Redirection** : le suffix FST ne contient **aucune posting list propre**. Chaque suffix
   pointe vers son token parent dans le FST ._raw (qui existe déjà) avec un offset SI.
   Les posting lists ne vivent que dans ._raw.

### Règle d'indexation

```
Token "rag3db" (6 chars) → suffixes (min_suffix_len=3) :

  r a g 3 d b
  ├─────────────┤  "rag3db"   SI=0  (mot complet, = le token ._raw)
    ├───────────┤  "ag3db"    SI=1  → redirige vers "rag3db" + SI=1
      ├─────────┤  "g3db"    SI=2  → redirige vers "rag3db" + SI=2
        ├───────┤  "3db"     SI=3  → redirige vers "rag3db" + SI=3
          ├─────┤  "db"      ✗ non indexé (< 3 chars)
            ├───┤  "b"       ✗ non indexé (< 3 chars)

  SI = Suffix Index (0 = mot entier, >0 = suffix propre)
  Ti = Token Index (position du token dans le document)

  min_suffix_len = 3 (configurable). Les suffixes < 3 chars ne sont
  pas indexés : ils génèrent beaucoup de multi-parents ("s", "e", "a")
  pour quasi aucun bénéfice (les queries de 1-2 chars sont rares).
  Si query < 3 chars → fallback prefix walk sur ._raw.
```

### Pourquoi ça marche

```
contains "g3d" :

  ① .sfx FST prefix walk "g3d"
    → trouve terme "g3db"
    → output FST : parent="rag3db", SI=2

  ② ._raw : lookup posting list de "rag3db"
    → (doc=42, Ti=1, byte_from=7), (doc=42, Ti=3, byte_from=20)

  ③ Ajuster byte_from += SI :
    → (doc=42, Ti=1, byte_from=9), (doc=42, Ti=3, byte_from=22)

  ④ highlights = [(9, 9+3), (22, 22+3)] = [(9,12), (22,25)]

  Coût : 2 FST lookups (microsecondes). Zéro posting list dans le .sfx.
```

### Pourquoi plus de trigrams

```
AVANT (trigrams) :
  "rag" → [doc 42, 58, 87, ...]     pré-filtre, pas preuve
  "ag3" → [doc 42, 58, ...]         faux positifs possibles
  → intersection → candidats → vérification stored text / token map

APRÈS (suffix FST) :
  .sfx prefix walk "g3d" → "g3db" → parent "rag3db" → ._raw posting
  → PREUVE DIRECTE, zéro vérification, zéro faux positif
```

Les trigrams servaient de pré-filtre. Le suffix FST + redirection ._raw est à la fois
le filtre ET la preuve, sans aucune donnée dupliquée.

## Architecture : 2 FST, chacun son rôle

```
._raw (EXISTANT, inchangé)              .sfx (NOUVEAU, redirection pure)
──────────────────────                   ────────────────────────────────
FST des vrais tokens                     FST de tous les suffixes
Posting lists complètes                  ZÉRO posting list
(doc, Ti, byte_from, byte_to)            Output = (parent_ordinal, SI)

"rag3db" → posting_id=3                 "rag3db" → (→"rag3db", SI=0)
"core"   → posting_id=7                 "ag3db"  → (→"rag3db", SI=1)
"import" → posting_id=1                 "g3db"   → (→"rag3db", SI=2)
                                         "3db"    → (→"rag3db", SI=3)
                                         "db"     → (→"rag3db", SI=4)
                                         "b"      → (→"rag3db", SI=5)
                                         "core"   → (→"core",   SI=0)
                                         "ore"    → (→"core",   SI=1)
                                         "re"     → (→"core",   SI=2)
                                         ...

Rôle :                                   Rôle :
  multi-token milieu (SI=0)                single-token contains
  startsWith, equals                       premier/dernier mot multi-token
  BM25 scoring                             substring quelconque
  SOURCE DES POSTING LISTS                 PUR INDEX DE REDIRECTION
```

### Pourquoi la redirection marche

Un suffix "g3db" d'un token "rag3db" apparaît dans **exactement les mêmes documents
et positions** que "rag3db" lui-même. La posting list de "rag3db" dans ._raw contient
déjà toute l'information — il suffit d'ajuster byte_from += SI.

### Multi-parents

Un terme suffix peut provenir de plusieurs tokens parents :

```
"core" est :
  - SI=0 de "core"       (le token lui-même)
  - SI=4 de "hardcore"    (suffix)
  - SI=3 de "unicode"     (suffix)
```

Le .sfx FST pointe vers une **liste de parents**, pas un seul :

```
.sfx : "core" → parent_list = [
  (→"core",     SI=0),
  (→"hardcore", SI=4),
  (→"unicode",  SI=3),
]
```

Résolution par merge de posting lists triées :

```
① Fetch les posting lists ._raw de chaque parent :
   ._raw "core"     → [(doc=12, Ti=2, byte=30), (doc=42, Ti=4, byte=27), (doc=58, Ti=1, byte=5)]
   ._raw "hardcore" → [(doc=42, Ti=7, byte=88), (doc=77, Ti=3, byte=41)]
   ._raw "unicode"  → [(doc=58, Ti=9, byte=102)]

② Merge trié (les posting lists sont déjà triées par doc_id) :
   Curseurs A, B, C avancent en parallèle, prennent le plus petit doc_id :
   → (doc=12, Ti=2,  byte=30+0=30)    ← parent "core" SI=0
   → (doc=42, Ti=4,  byte=27+0=27)    ← parent "core" SI=0
   → (doc=42, Ti=7,  byte=88+4=92)    ← parent "hardcore" SI=4
   → (doc=58, Ti=1,  byte=5+0=5)      ← parent "core" SI=0
   → (doc=58, Ti=9,  byte=102+3=105)  ← parent "unicode" SI=3
   → (doc=77, Ti=3,  byte=41+4=45)    ← parent "hardcore" SI=4

   O(n) total, zéro allocation, zéro HashMap. Les curseurs avancent
   dans l'ordre — c'est le pattern standard d'union de posting lists.
```

Les multi-parents sont rares (~5% des suffixes, ~1-3 parents en pratique).
Le merge de 2-3 petites listes triées est quasi gratuit.

## Encoding FST output (u64)

Le FST `fst` crate mappe chaque terme vers un u64. On l'utilise pour encoder
le pointeur vers le parent :

```
┌─────────────────────────────────────────────────┐
│ Cas commun : 1 seul parent (~95% des suffixes)  │
│                                                 │
│ bit 63 = 0                                      │
│ bits 0-23 = ordinal du parent dans le FST ._raw │
│             (supporte jusqu'à ~16M tokens)       │
│ bits 24-31 = SI (supporte jusqu'à 256 chars)    │
│                                                 │
│ → self-contained dans le u64, zéro lookup annexe│
├─────────────────────────────────────────────────┤
│ Cas rare : plusieurs parents (~5% des suffixes) │
│                                                 │
│ bit 63 = 1                                      │
│ bits 0-31 = offset dans la section parent_list  │
│                                                 │
│ parent_list[offset] :                           │
│   num_parents: u8                               │
│   [(raw_ordinal: u32, SI: u16), ...]            │
│                                                 │
│ → 1 lecture mmap supplémentaire                 │
└─────────────────────────────────────────────────┘
```

## Structure du fichier `.sfx`

Un fichier par segment, mmap'd. **Pas de posting lists.** Taille estimée ~20-40MB
pour le FST (dépend de la distribution des identifiants, à mesurer sur corpus réel).

```
┌──────────────────────────────────────────────────┐
│ HEADER (fixe)                                    │
│   magic: "SFX1"                                  │
│   version: u8                                    │
│   num_docs: u32                                  │
│   num_suffix_terms: u32                          │
│   fst_offset: u64                                │
│   fst_length: u64                                │
│   parent_list_offset: u64                        │
│   parent_list_length: u64                        │
│   gapmap_offset: u64                             │
├──────────────────────────────────────────────────┤
│ SECTION A : SUFFIX FST                           │
│                                                  │
│   FST standard (crate `fst`)                     │
│   terme (suffix lowercase) → u64 (encoding       │
│   ci-dessus : parent ordinal + SI, ou offset     │
│   dans parent_list)                              │
│                                                  │
│   Taille estimée : ~15-20MB                      │
│                                                  │
├──────────────────────────────────────────────────┤
│ SECTION B : PARENT LISTS (cas multi-parents)     │
│                                                  │
│   Uniquement pour les suffixes avec >1 parent.   │
│   Array de :                                     │
│     num_parents: u8                              │
│     [(raw_ordinal: u32, SI: u16), ...]           │
│                                                  │
│   Taille estimée : ~2MB                          │
│                                                  │
├──────────────────────────────────────────────────┤
│ SECTION C : GAPMAP                               │
│                                                  │
│   Doc offset table: [u64 × (num_docs + 1)]       │
│                                                  │
│   Per doc :                                      │
│     num_tokens: u16                              │
│     per token (num_tokens + 1 gaps) :            │
│       sep_len: u8                                │
│       sep_bytes: [u8; sep_len]                   │
│                                                  │
│   gap[0] = avant token 0 (prefix doc)            │
│   gap[i] = entre token i-1 et token i            │
│   gap[N] = après dernier token (suffix doc)      │
│                                                  │
│   Taille estimée : ~10KB/doc, ~50MB total        │
│                                                  │
└──────────────────────────────────────────────────┘
```

## Indexation

### Par document

```
min_suffix_len = 3  (configurable)

pour chaque token (position Ti, texte, byte_from, byte_to) :

  1. lowercase(texte) → le token est un terme ._raw (SI=0, déjà indexé)
  2. pour chaque suffixe S à l'index K (K=1 à len-min_suffix_len) :
       si len(S) >= min_suffix_len :
         enregistrer dans le .sfx builder :
           terme S → parent = texte, SI = K
  3. capturer le séparateur entre token Ti et Ti+1
     → écrire dans GapMap
```

Note : les suffixes SI=0 (mot complet) sont dans le ._raw et le .sfx. Le .sfx pour
SI=0 redirige simplement vers le ._raw ordinal correspondant avec SI=0.

**min_suffix_len = 3** : les suffixes de 1-2 chars ("s", "e", "db") ne sont pas
indexés. Ils génèrent beaucoup de multi-parents pour quasi aucun bénéfice
(les queries contains de 1-2 chars sont très rares). Si une query fait < 3 chars,
fallback sur un prefix walk du FST ._raw.

**Builder streaming** : le SuffixFstBuilder accumule les paires (suffix → parent)
par token unique (pas par occurrence). Le BTreeMap trié est alimenté au fil de
l'indexation et le FST est construit en une passe à la fin du segment. Ne pas
accumuler en RAM toutes les occurrences — juste les termes uniques + leurs parents.

### Exemple complet

```
Texte : "import rag3db from 'rag3db_core';"
Tokens :  Ti=0       Ti=1     Ti=2    Ti=3     Ti=4
         "import"  "rag3db"  "from"  "rag3db"  "core"
         (0,6)     (7,13)    (14,18) (20,26)   (27,31)

._raw (existant, inchangé) :
  FST : {"core", "from", "import", "rag3db"}
  posting "rag3db" : [(doc=42, Ti=1, byte=7..13), (doc=42, Ti=3, byte=20..26)]
  posting "core"   : [(doc=42, Ti=4, byte=27..31)]
  posting "import" : [(doc=42, Ti=0, byte=0..6)]
  posting "from"   : [(doc=42, Ti=2, byte=14..18)]

.sfx (nouveau, redirection) :
  FST :
    "import"   → (→raw_ord("import"),  SI=0)
    "mport"    → (→raw_ord("import"),  SI=1)
    "port"     → (→raw_ord("import"),  SI=2)
    "ort"      → (→raw_ord("import"),  SI=3)
    "rt"       → (→raw_ord("import"),  SI=4)
    "t"        → (→raw_ord("import"),  SI=5)
    "rag3db"   → (→raw_ord("rag3db"),  SI=0)
    "ag3db"    → (→raw_ord("rag3db"),  SI=1)
    "g3db"     → (→raw_ord("rag3db"),  SI=2)
    "3db"      → (→raw_ord("rag3db"),  SI=3)
    "db"       → multi-parent? "rag3db" SI=4 et potentiellement d'autres
    "b"        → multi-parent? "rag3db" SI=5 et potentiellement d'autres
    "core"     → (→raw_ord("core"),    SI=0)
    "ore"      → (→raw_ord("core"),    SI=1)
    "re"       → multi-parent? "core" SI=2, "from" SI=2 (non: "re" ≠ suffix de "from")
                 en fait "re" n'est suffix que de "core" → single parent
    "e"        → multi-parent possible (suffix de "core", "import", ...)
    ... etc

GapMap doc 42 :
  gap[0] = ""      (avant "import")
  gap[1] = " "     (entre "import" et "rag3db")
  gap[2] = " "     (entre "rag3db" et "from")
  gap[3] = " '"    (entre "from" et "rag3db")
  gap[4] = "_"     (entre "rag3db" et "core")
  gap[5] = "';"    (après "core")
```

## Recherche

### Single token : contains "g3d" d=0

```
① .sfx : FST prefix walk "g3d"
   → terme "g3db" matche (prefix "g3d" ✓)
   → output : parent="rag3db", SI=2

② ._raw : lookup posting list de "rag3db"
   → (doc=42, Ti=1, byte=7), (doc=42, Ti=3, byte=20)

③ Ajuster byte_from += SI=2 :
   → (doc=42, Ti=1, byte=9), (doc=42, Ti=3, byte=22)

④ Résultat :
   doc=42, 2 occurrences
   highlights = [(9, 12), (22, 25)]

   2 FST lookups. Zéro posting dans .sfx. Zéro stored text.
```

### Single token : contains "g3d" d=1 (fuzzy)

```
① .sfx : FST prefix walk avec Levenshtein DFA (d=1)
   → "g3db" (d=0) → parent "rag3db" SI=2
   → "g3dc" (d=1) → parent "g3dcx" SI=0 (si existe)
   → "g3b"  (d=1) → parent "xg3b" SI=1 (si existe)
   → etc.

② Pour chaque parent trouvé → ._raw posting list → ajuster byte_from

③ Union des résultats avec distance minimale.

   Même DFA Levenshtein que startsWith fuzzy.
```

### Note : piste cascade skip + DFA réduit (non retenue pour v1)

Piste explorée : pour d≥3, décomposer en skips de premières lettres du query
(chaque skip = 1 délétion consommée du budget) + DFA de distance réduite.

```
Query "rag3db" d=3 → au lieu d'un DFA d=3 :
  skip 0 "rag3db" d=0, skip 1 "ag3db" d=0, ..., skip 0 "rag3db" d=1, ...
  → jamais besoin de DFA d=3, max = d=2
```

**Non retenue** : 9 walks FST × petit DFA vs 1 walk × gros DFA — pas clairement
plus rapide. Le FST est mmap'd et cache-friendly, le DFA tient en L1.
Les walks multiples produisent des doublons à dédupliquer. Et en pratique,
d=0 couvre 90%+ des queries, d=1 le reste, d≥3 est quasi inexistant.

**Approche retenue** : DFA Levenshtein direct jusqu'à d=2. Si d=3 est demandé,
payer le coût du gros DFA — c'est honnête et simple.

### Multi-token : contains "rag3db core" d=0

```
Tokens query : ["rag3db", "core"], séparateur attendu : " "

① ._raw : lookup exact "rag3db"
   posting (triée par doc_id) :
   → curseur A : (doc=42, Ti=1), (doc=42, Ti=3)

② ._raw : lookup exact "core"
   posting (triée par doc_id) :
   → curseur B : (doc=42, Ti=4)

③ Intersection par merge de curseurs :
   curseur A : doc=42, Ti=1  |  curseur B : doc=42, Ti=4
   même doc ✓, Ti+1 = 2 ≠ 4 → avancer A
   curseur A : doc=42, Ti=3  |  curseur B : doc=42, Ti=4
   même doc ✓, Ti+1 = 4 = 4 → MATCH ✓

④ GapMap(doc=42, gap[4]) = "_"
   Séparateur attendu = " "
   "_" ≠ " " → REJETÉ ✓

   Multi-token pur SI=0 : utilise ._raw directement, .sfx pas touché.
   Zéro HashMap, juste des curseurs triés qui avancent.
```

### Multi-token avec substring : contains "g3db is a cool fram" d=0

```
Document : "rag3db is a cool framework"
Tokens :    Ti=0     Ti=1 Ti=2 Ti=3  Ti=4

Tokens query : ["g3db", "is", "a", "cool", "fram"]

Premier token (suffix, tout SI) :
① .sfx : lookup exact "g3db"
   → parent="rag3db", SI=2
   → ._raw posting "rag3db" : (Ti=0, byte=0..6)
   Vérifie fin de token : SI + len("g3db") = 2+4 = 6 = len("rag3db") ✓

Tokens milieu (vrais tokens, SI=0) :
② ._raw : lookup exact "is"   → Ti=1 ✓
③ ._raw : lookup exact "a"    → Ti=2 ✓
④ ._raw : lookup exact "cool" → Ti=3 ✓

Dernier token (prefix, SI=0) :
⑤ .sfx : prefix walk "fram", SI=0 seulement
   → trouve "framework" → parent="framework", SI=0
   → ._raw posting "framework" : (Ti=4)
   prefix "fram" matche "framework" ✓

⑥ Ti consécutifs : 0→1→2→3→4 ✓

⑦ GapMap : tous les séparateurs = " " ✓

→ MATCH CONFIRMÉ
```

### Règle multi-token

```
                    FST utilisé     SI accepté    Type de walk
                    ───────────     ──────────    ────────────
Premier token       .sfx            tout SI       exact
Tokens milieu       ._raw           SI=0 seul     exact
Dernier token       .sfx            SI=0 seul     prefix
Token unique        .sfx            tout SI       prefix

Si query = 1 token  → single token (prefix walk .sfx, tout SI)
Si query = 2 tokens → premier (.sfx exact) + dernier (.sfx prefix SI=0)
Si query ≥ 3 tokens → premier + milieu(x) + dernier
```

**Pourquoi :**
- Le premier token peut entrer en milieu de mot (suffix) → .sfx, tout SI
- Les tokens milieu sont forcément des mots complets → ._raw, SI=0
- Le dernier token peut sortir en milieu de mot (prefix) → .sfx, SI=0 + prefix walk
- Un token unique est un substring quelconque → .sfx, tout SI + prefix walk

## Pas de stemming

Le .sfx et le ._raw stockent les tokens en **lowercase uniquement**, sans stemming.
Contains est une recherche de **substring exact dans le texte**. Le stemming est
réservé au champ principal (BM25, pertinence).

```
Champ principal  : stemming ✓  (BM25, "frameworks" → "framework")
._raw            : lowercase   (tokens exacts, positions, offsets)
.sfx             : lowercase   (suffixes, redirection vers ._raw)
```

Le stemming casserait les suffixes et les redirections vers ._raw
(les termes stemmés n'existent pas dans ._raw). Le fuzzy d=1 couvre
naturellement les variations morphologiques.

## Taille estimée

```
                          Avant            Après (redirection)
                          ─────            ───────────────────
._ngram FST + postings :  ~2MB             0  (SUPPRIMÉ)
._raw FST + postings :    ~5MB             ~5MB  (INCHANGÉ)
.tmap (token map) :       ~450MB           0  (SUPPRIMÉ)
stored text (LZ4) :       ~80MB            ~80MB  (gardé pour affichage,
                                                    pas sur le hot path)

.sfx suffix FST :         —                ~20-40MB  (NOUVEAU)
.sfx parent lists :       —                ~2MB  (NOUVEAU)
.sfx GapMap :             —                ~50MB  (NOUVEAU)

TOTAL hot path search :   ~537MB           ~75-95MB
```

### Pourquoi c'est si petit

Les posting lists n'existent QUE dans ._raw (~5MB). Le .sfx ne contient que :
- Un FST de suffixes (~20-40MB — plus gros que le ._raw FST car plus de termes,
  mais le FST compresse bien les suffixes communs. Dépend de la distribution
  des identifiants — code avec beaucoup de camelCase longs = plus de suffixes.
  À mesurer sur corpus réel.)
- Des parent lists pour les rares cas multi-parents (~2MB)
- La GapMap pour les séparateurs (~50MB)

Aucune duplication de posting data. Le ._raw est la **source unique de vérité**.

### Comparaison avec le design sans redirection

```
Sans redirection :  .sfx postings ~300-400MB  (chaque suffix = sa propre posting)
Avec redirection :  .sfx ~20MB + parent ~2MB  (zero postings, juste des pointeurs)

Gain : ~350MB économisés, pour le coût d'1 lookup ._raw supplémentaire (~μs).
```

## Couverture des features

```
                     Avant              Après
                     ─────              ─────
highlights           stored text        ._raw byte offsets + SI ✓
BM25                 champ principal    champ principal (inchangé) ✓
regex intra-token    ._raw FST          ._raw FST (inchangé) ✓
regex substring      stored text        .sfx prefix walk + regex DFA ✓
regex cross-token    stored text        stored text (inchangé)
contains d=0         stored text        .sfx → ._raw posting ✓
contains d>0         stored text        .sfx + Levenshtein DFA → ._raw ✓
contains multi       stored text        ._raw direct + .sfx bords ✓
startsWith           ._raw              ._raw (inchangé) ✓
equals               ._raw              ._raw (inchangé) ✓
```

Le stored text reste uniquement pour le regex cross-token et l'affichage
du document complet. Plus sur le hot path des queries normales.

## Ce que le suffix FST réutilise

```
startsWith fuzzy → FST prefix walk + Levenshtein DFA  ← sur ._raw
contains single  → FST prefix walk sur .sfx → redirect ._raw posting
contains fuzzy   → FST prefix walk + Levenshtein DFA sur .sfx → ._raw
contains multi   → ._raw direct (milieu) + .sfx (bords) → ._raw

Même mécanisme (AutomatonWeight, FuzzyTermQuery), juste un FST différent.
La redirection est une étape supplémentaire légère (~μs).
```

## GapMap : séparateurs stricts

La GapMap ne sert que pour le mode strict (vérifier le caractère exact
du séparateur entre deux tokens consécutifs). En mode relaxed (positions
consécutives = séparateur garanti par le tokenizer), la GapMap est ignorée.

Si le mode relaxed suffit pour le use case, la GapMap est optionnelle et
le .sfx se réduit au FST + parent lists seulement (~20MB total).

## Ordre d'implémentation

### Phase 1 — .sfx writer (indexation)

Écrire le fichier .sfx à l'indexation :
- Tokeniser le texte (réutiliser le tokenizer raw existant, lowercase)
- Pour chaque token, générer tous les suffixes (SI=1 à len-1)
- Pour chaque suffix, enregistrer le parent (raw term + SI)
- Regrouper les multi-parents (suffixes communs à plusieurs tokens)
- Construire le FST avec output = (raw_ordinal, SI) ou parent_list_offset
- Écrire les parent lists pour les cas multi-parents
- Écrire la GapMap

### Phase 2 — .sfx reader (recherche single token)

Lire le .sfx en mmap :
- FST prefix walk pour trouver les termes matchés
- Décoder output → (raw_ordinal, SI)
- Lookup ._raw FST par ordinal → posting list
- Ajuster byte_from += SI
- Produire highlights
- Brancher dans NgramContainsQuery (remplace la vérification stored text)

### Phase 3 — Fuzzy single token

Brancher le Levenshtein DFA existant sur le suffix FST du .sfx.
Même code que startsWith fuzzy, avec la redirection ._raw en plus.

### Phase 4 — Multi-token + GapMap

Queries multi-token :
- Premier token : .sfx exact, tout SI, vérifie fin de token
- Milieu : ._raw exact, SI=0 (direct, pas de .sfx)
- Dernier : .sfx prefix walk, SI=0
- Intersection Ti consécutifs entre les posting lists ._raw
- GapMap reader pour validation séparateurs (si strict)

### Phase 5 — Merger

Merger les .sfx lors du merge de segments :
- Le .sfx ne contient pas de posting data → pas de remapping doc_ids
- Reconstruire le FST suffixes (les raw ordinals changent au merge)
- Reconstruire les parent lists
- Concaténer les GapMap (remapping doc_ids)

### Phase 6 — Supprimer ._ngram

Retirer le champ ._ngram de l'indexation. Le .sfx le remplace complètement.
Le ._raw est gardé pour multi-token milieu, startsWith, equals, BM25.
