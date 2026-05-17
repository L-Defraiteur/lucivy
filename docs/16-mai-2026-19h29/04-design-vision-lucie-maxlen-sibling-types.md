# Design : Vision Lucie — MaxLen tokenizer + typed sibling links + dual SI

**Date** : 16 mai 2026  
**Statut** : Design retenu — à prototyper

---

## Principe fondamental

Un seul tokenizer simple : **split sur séparateurs, puis max-len sur chaque mot**.  
Plus de CamelCase, plus de logique sémantique. Juste :
1. Couper aux séparateurs (non-alphanumérique)
2. Si un mot dépasse max-len (ex: 5 bytes), le couper en chunks de max-len
3. Chaque token absorbe ses trailing separators

Les sibling links deviennent **typés** pour encoder la structure sémantique que le tokenizer ne capture plus.

---

## Tokenizer

### Règles

```
Input : "pthread_mutex_lock"

Étape 1 — Split sur séparateurs :
  Mots :        ["pthread",  "mutex",  "lock"]
  Séparateurs : ["_",        "_",      ""]

Étape 2 — Max-len split (max=5) sur chaque mot :
  "pthread" (7 bytes > 5) → ["pthre", "ad"]
  "mutex"   (5 bytes = 5) → ["mutex"]
  "lock"    (4 bytes < 5) → ["lock"]

Étape 3 — Chaque token absorbe ses trailing separators :
  Le dernier chunk d'un mot absorbe le séparateur qui suit.
  "pthre" → "pthre"    (pas le dernier chunk de "pthread")
  "ad"    → "ad_"      (dernier chunk de "pthread", absorbe "_")
  "mutex" → "mutex_"   (seul chunk, absorbe "_")
  "lock"  → "lock"     (pas de sep trailing)

Tokens finaux : ["pthre", "ad_", "mutex_", "lock"]
```

### Autres exemples

```
"Error::LucivyError"
  Mots : ["Error", "LucivyError"]
  Seps : ["::", ""]
  Max-len split : ["Error", "Luciv", "yErro", "r"]
  Avec trailing sep : ["Error::", "Luciv", "yErro", "r"]
  Tokens : ["Error::", "Luciv", "yErro", "r"]

"getElementById"
  Mots : ["getElementById"]  (pas de séparateur)
  Max-len split : ["getEl", "ement", "ById"]  (15 bytes → 3 chunks)
  Tokens : ["getEl", "ement", "ById"]

"my_var"
  Mots : ["my", "var"]
  Seps : ["_"]
  Max-len : pas besoin (tous < 5)
  Tokens : ["my_", "var"]

"a    b"  (4 espaces)
  Mots : ["a", "b"]
  Seps : ["    ", ""]
  Trailing sep (max 3) : "a" absorbe "   " (3 chars), le 4ème espace est perdu ? 
  → Ou bien : on absorbe TOUS les séparateurs (pas de max 3).
  Tokens : ["a    ", "b"]   (si pas de max)
  Tokens : ["a   ", " b"]   (si max 3, le 4ème déborde dans le token suivant?)
  
  → Décision : absorber TOUS les trailing sep. Pas de limite.
    Le max-len (5) s'applique au CONTENU alphanumérique, pas aux séparateurs.
    Un token fait : [contenu ≤ 5 bytes] + [sep trailing, longueur variable]
```

### Metadata par token

Chaque token porte :
- `content_len` : nombre de bytes de contenu alphanumérique (≤ max-len)
- `sep_len` : nombre de bytes de séparateurs trailing (≥ 0)
- `token_len` : content_len + sep_len (longueur totale dans le FST)

---

## Indexes : Word et Token

Deux niveaux d'identité :

### Word (mot logique)

Un mot = la séquence alphanumérique entre deux ranges de séparateurs.  
Identifié par un **word_ordinal** (WI = word index).

```
"pthread_mutex_lock"
  Word 0 : "pthread"  (tokens: ["pthre", "ad_"])
  Word 1 : "mutex"    (tokens: ["mutex_"])
  Word 2 : "lock"     (tokens: ["lock"])
```

### Token (chunk indexé)

Un token = un chunk de max-len + trailing sep.  
Identifié par un **token_ordinal** (TI = token index).

```
  TI=0 : "pthre"   → WI=0, position_in_word=0
  TI=1 : "ad_"     → WI=0, position_in_word=1, has_trailing_sep=true
  TI=2 : "mutex_"  → WI=1, position_in_word=0, has_trailing_sep=true
  TI=3 : "lock"    → WI=2, position_in_word=0
```

---

## Typed Sibling Links

Trois types de liens entre tokens :

### 1. `sibling_next_token`

Prochain token dans la séquence linéaire. Toujours contigu (gap=0).

```
TI=0 "pthre"  →next_token→  TI=1 "ad_"
TI=1 "ad_"    →next_token→  TI=2 "mutex_"
TI=2 "mutex_" →next_token→  TI=3 "lock"
```

Utilisation : falling walk standard. Quand on atteint la fin d'un token, on suit next_token pour continuer.

### 2. `sibling_next_word`

Prochain token qui commence un NOUVEAU MOT. Saute les chunks intermédiaires du mot courant ET les séparateurs (déjà absorbés dans le trailing).

```
TI=0 "pthre"  →next_word→  TI=2 "mutex_"   (saute "ad_" qui est la suite de "pthread")
TI=1 "ad_"    →next_word→  TI=2 "mutex_"   (même chose, "ad_" est le dernier chunk de "pthread")
TI=2 "mutex_" →next_word→  TI=3 "lock"
TI=3 "lock"   →next_word→  None
```

Utilisation : query `term("mutex")` ou `startsWith("mutex")` — on veut sauter directement au prochain mot, pas au prochain chunk.

### 3. `sibling_next_sep`

Prochain token qui CONTIENT un séparateur trailing. Utile pour regex patterns comme `".*_.*"` où on cherche un séparateur spécifique.

```
TI=0 "pthre"  →next_sep→  TI=1 "ad_"      (premier token avec trailing sep)
TI=1 "ad_"    →next_sep→  TI=2 "mutex_"   (prochain avec trailing sep)
TI=2 "mutex_" →next_sep→  None             ("lock" n'a pas de trailing sep)
```

Utilisation : regex `"[a-z]+_[a-z]+"` — vérifier qu'il existe un `_` entre deux matches.

---

## SFX Encoding — Dual SI

Chaque entrée du suffix FST encode deux niveaux de SI :

### STI (Suffix Token Index)

Position du suffix dans le token courant. Comme aujourd'hui.

```
Token "mutex_" (6 bytes) :
  STI=0 : mutex_
  STI=1 : utex_
  STI=2 : tex_
  STI=3 : ex_
  STI=4 : x_
  STI=5 : _
```

### SWI (Suffix Word Index)

Position du suffix depuis le DÉBUT DU MOT (pas du token). Si le mot a été chunké en plusieurs tokens, SWI continue à travers les chunks.

```
Word "pthread" → tokens ["pthre", "ad_"]

Token "pthre" (TI=0) :
  STI=0, SWI=0 : pthre...    (SWI=0 = début du mot)
  STI=1, SWI=1 : thre...
  STI=2, SWI=2 : hre...
  STI=3, SWI=3 : re...
  STI=4, SWI=4 : e...

Token "ad_" (TI=1) :
  STI=0, SWI=5 : ad_         (SWI=5 car "pthre" fait 5 bytes)
  STI=1, SWI=6 : d_
  STI=2, SWI=7 : _           (SWI=7 = le séparateur)

Token "mutex_" (TI=2) :
  STI=0, SWI=0 : mutex_      (SWI reset à 0 : nouveau mot !)
  STI=1, SWI=1 : utex_
  ...
```

### Pourquoi deux SI ?

- **STI** : pour le falling walk — savoir quand on atteint la fin du TOKEN (split point → next_token)
- **SWI** : pour le scoring BM25 et le matching sémantique — savoir quand on est au début d'un MOT (SWI=0 = `startsWith` candidat)

### Encodage u64

```
Actuel :  [63:multi][55..40:token_len][39..24:si][23..0:ordinal]

Proposition :
  [63: multi_flag]
  [62: is_word_start]        ← 1 bit : SWI==0 (début de mot)
  [61..56: sep_len]          ← 6 bits (max 63 bytes de trailing sep)
  [55..40: token_len]        ← 16 bits (longueur totale du token avec sep)
  [39..24: sti]              ← 16 bits (position dans le token)
  [23..0: token_ordinal]     ← 24 bits (TI)
```

Pour reconstruire SWI : besoin du `word_offset` du token (= somme des content_len des tokens précédents dans le même mot). Stocké dans une table annexe `word_map` ou dérivé depuis le sibling `next_token` chain.

Alternative plus simple : stocker SWI directement à la place de STI, et dériver STI = SWI - word_offset. Mais word_offset n'est pas dans l'output...

**Décision pragmatique** : encoder STI (comme aujourd'hui), et ajouter `is_word_start` (1 bit) + `word_ordinal` (dans OutputTable pour les cross-word entries). Ça couvre 90% des besoins sans changer le format radicalement.

---

## FST Partition

Toujours 2 partitions (pas 3) :

```
0x00 — STI=0 (début de token)
       Contient TOUS les tokens : "pthre", "ad_", "mutex_", "lock"
       Utilisé par : startsWith quand on veut le début d'un token
       
0x01 — STI>0 (substring)
       Contient tous les suffixes internes : "thre", "hre", "utex_", "tex_", etc.
       Utilisé par : contains
```

Le bit `is_word_start` dans l'output permet de filtrer :
- `startsWith("mutex")` : partition 0x00, filtre is_word_start=true → match "mutex_" à STI=0, is_word_start=true
- `startsWith("ad")` : partition 0x00, filtre is_word_start=false → match "ad_" à STI=0, is_word_start=false (c'est un chunk, pas un début de mot) — filtré OUT
- `contains("tex")` : partition 0x01, pas de filtre word → match "tex_" à STI=2 dans "mutex_"

---

## Falling Walk — Comment ça marche

### Exact : "utex_lo"

```
1. Enter partition 0x01
2. Walk : u-t-e-x-_ → 5 bytes consumed dans "mutex_" (STI=1)
   STI(1) + 5 = 6 = token_len(6) → SPLIT POINT
3. sibling_next_token("mutex_") → "lock" (TI=3), gap=0
4. Walk : l-o → 2 bytes consumed dans "lock" (STI=0)
5. Pas encore token_len → match partiel, OK pour contains
Result : match dans tokens [2, 3], words [1, 2]
```

### Exact cross-chunk : "thread_"

```
Le mot "pthread" est chunké en ["pthre", "ad_"]
Chercher "thread_" = 7 bytes

1. Enter partition 0x01
2. Walk : t-h-r-e → 4 bytes consumed dans "pthre" (STI=1)
   STI(1) + 4 = 5 = token_len(5) → SPLIT POINT (fin de chunk, pas fin de mot)
3. sibling_next_token("pthre") → "ad_" (TI=1), gap=0
4. Walk : a-d-_ → 3 bytes consumed dans "ad_" (STI=0)
   STI(0) + 3 = 3 = token_len(3) → SPLIT POINT (fin de chunk ET fin de mot car trailing sep)
5. Split at mot boundary → done
Result : match dans tokens [0, 1], word [0]
   "thread_" = suffix de word "pthread_"
```

### Fuzzy : "mutx_lck" d=2

```
1. Pas de concat_query — query = "mutx_lck" telle quelle (8 bytes)
2. Trigrams : "mut", "utx", "tx_", "x_l", "_lc", "lck"
3. AUCUN boundary trigram — le "_" est dans le token "mutex_"
4. Chaque trigram : FST walk normal
   - "tx_" → match suffix de "mutex_" à STI=2 (3 bytes, single token)
   - "x_l" → match suffix "x_" de "mutex_" STI=4 (2 bytes) → split → next_token → "lock" → "l" (1 byte)
5. Threshold = 6 - 3*2 = 0 → au moins 0 trigrams requis (très permissif à d=2)
   En pratique avec 6 trigrams et d=2, threshold = max(0, min_threshold) = 1
6. Resolve postings → score → done

Coût : 6 FST walks simples + 1-2 falling walks pour les trigrams cross-token
Pas de sibling DFS ! Le "x_l" cross-token est résolu en 1 split + 1 next_token lookup.
```

---

## Sibling Table — Format

```rust
enum SiblingType {
    NextToken,    // prochain chunk dans la séquence linéaire
    NextWord,     // prochain début de mot (saute chunks intermédiaires + sep)
    NextSep,      // prochain token avec trailing sep
}

struct SiblingEntry {
    next_ordinal: u32,    // TI du prochain token
    sibling_type: u8,     // NextToken | NextWord | NextSep
    // gap_len supprimé : toujours 0 car trailing sep absorbé
}
```

Ou plus compact : 3 arrays séparés par type :

```
next_token_table : [u32; num_tokens]   — next_token[ti] = ti+1 (trivial, juste ti+1)
next_word_table  : [u32; num_tokens]   — next_word[ti] = TI du prochain début de mot
next_sep_table   : [u32; num_tokens]   — next_sep[ti] = TI du prochain token avec sep
```

En fait `next_token` est toujours `ti + 1` (les tokens sont linéaires), donc pas besoin de le stocker. On garde :
- `next_word[ti]` : O(1) lookup
- `next_sep[ti]` : O(1) lookup (optionnel, pour regex)

---

## Word Map — Mapping token ↔ word

Table annexe pour relier tokens et mots :

```
word_map :
  token_to_word[ti] → wi          (quel mot contient ce token)
  word_start_token[wi] → ti       (premier token du mot wi)
  word_content_len[wi] → u16      (longueur contenu du mot, sans sep)
```

Utilisation :
- BM25 scoring : term frequency par WORD, pas par token
- `term("mutex")` exact match : vérifier que match couvre exactement word_content_len
- `startsWith("mutex")` : vérifier que STI=0 ET is_word_start=true

---

## Résumé complet

```
Texte : "pthread_mutex_lock_init"

Mots :     ["pthread",         "mutex",   "lock",  "init"]
Word IDs :  WI=0               WI=1       WI=2     WI=3
Seps :     ["_",               "_",       "_",     ""]

Tokens (max-len=5, trailing sep) :
  TI=0 : "pthre"     WI=0, pos=0, content=5, sep=0, is_word_start=true
  TI=1 : "ad_"       WI=0, pos=1, content=2, sep=1, is_word_start=false
  TI=2 : "mutex_"    WI=1, pos=0, content=5, sep=1, is_word_start=true
  TI=3 : "lock_"     WI=2, pos=0, content=4, sep=1, is_word_start=true
  TI=4 : "init"      WI=3, pos=0, content=4, sep=0, is_word_start=true

Sibling next_token :
  TI=0 → TI=1    ("pthre" → "ad_")
  TI=1 → TI=2    ("ad_" → "mutex_")
  TI=2 → TI=3    ("mutex_" → "lock_")
  TI=3 → TI=4    ("lock_" → "init")

Sibling next_word :
  TI=0 → TI=2    ("pthre" → "mutex_")
  TI=1 → TI=2    ("ad_" → "mutex_")
  TI=2 → TI=3    ("mutex_" → "lock_")
  TI=3 → TI=4    ("lock_" → "init")

FST entries (partition 0x01, subset) :
  "thre"    → ord=0, STI=1, token_len=5, is_word_start=true, sep_len=0
  "ad_"     → ord=1, STI=0, token_len=3, is_word_start=false, sep_len=1
  "d_"      → ord=1, STI=1, token_len=3, is_word_start=false, sep_len=1
  "_"       → ord=1, STI=2, token_len=3, is_word_start=false, sep_len=1
  "utex_"   → ord=2, STI=1, token_len=6, is_word_start=true, sep_len=1
  "tex_"    → ord=2, STI=2, token_len=6, is_word_start=true, sep_len=1
  "x_"      → ord=2, STI=4, token_len=6, is_word_start=true, sep_len=1
  "_"       → ord=2, STI=5, token_len=6, is_word_start=true, sep_len=1
             (note: "_" a maintenant 2 parents — multi-parent dans OutputTable)
```

---

## Ce que ça résout

| Query | Avant (v2 actuelle) | Après (cette vision) |
|-------|--------------------|--------------------|
| `contains("mutex_lock")` | concat → "mutexlock", 16 FST walks, sibling DFS ~200ms | Query telle quelle, 8 trigrams, 0 boundary, falling walk gap=0 ~5ms |
| `fuzzy("mutx_lck", d=2)` | concat, boundary trigrams jetés, sibling DFS | Query telle quelle, tous trigrams utiles, ~5ms |
| `term("mutex")` | Match token "mutex" exact | Match suffix SWI=0 de "mutex_", check content_len=5 |
| `startsWith("get")` | Match SI=0 de "get..." | Match STI=0, is_word_start=true |
| `regex("mutex.*lock")` | Literal extraction + SepMap | Literal extraction, `_` dans le FST, DFA traverse naturellement |
| `contains("thread")` | Cross-token "pthre"→"ad", sibling DFS | Chunks "pthre"→"ad_", next_token gap=0, rapide |

---

## Index size estimation

Pour le dataset linux 90K docs, tokens moyens ~6 bytes :
- Suffixes par token : ~6 (content) + ~1 (sep) = 7 entrées FST au lieu de 6
- Augmentation : **+15-20%** par rapport à v2 actuelle
- next_word table : 4 bytes × num_tokens (négligeable)
- word_map : ~8 bytes × num_tokens (négligeable)

**Total estimé : index ×1.15-1.20**

---

## Prochaines étapes

1. **Prototyper le tokenizer** : MaxLenTrailingSepTokenizer dans src/tokenizer/
2. **Modifier SFX builder** : ajouter sep_len, is_word_start dans l'encodage u64
3. **Modifier sibling table** : next_word array
4. **Modifier falling walk** : utiliser next_token (= ti+1) au lieu de sibling DFS
5. **Modifier fuzzy_contains** : supprimer concat_query, supprimer boundary handling
6. **Benchmark** : comparer index size et query speed sur linux 90K docs
7. **Décider max-len** : 5 ? 8 ? 10 ? → benchmarker différentes valeurs
