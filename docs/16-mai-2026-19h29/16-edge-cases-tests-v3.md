# Edge Cases — Tests v3 poussifs

**Date** : 17 mai 2026  
**Objectif** : lister tous les cas limites imaginables pour tester la chaîne complète v3 (indexation → query → résultats).

---

## 1. Tokenizer — division égale

| # | Cas | Input | Attendu |
|---|-----|-------|---------|
| T1 | Token court sans sep | `"ab"` | 1 chunk: `"ab"` |
| T2 | Token exact MAX_TOKEN | `"abcdefgh"` (8 bytes) | 1 chunk |
| T3 | Token MAX_TOKEN+1 → split | `"abcdefghi"` (9) | 2 chunks: (5, 4) |
| T4 | Mot long → 3 chunks | `"internationalization"` (20) | 3 chunks: (7, 7, 6) |
| T5 | Sep court absorbé | `"a_b"` | 2 tokens: `"a_"`, `"b"` |
| T6 | Sep long → split segment | `"a________b"` (11) | seg `"a________"` (9) → split (5,4) + `"b"` |
| T7 | Que des seps | `"________"` | 1 segment pure sep, chunked si > MAX_TOKEN |
| T8 | Leading seps | `"__init"` | seg `"__"` + seg `"init"` |
| T9 | Trailing seps sans mot après | `"hello__"` | seg `"hello__"` (7 ≤ 8) → 1 chunk |
| T10 | UTF-8 multi-byte | `"café_latte"` | Pas de split au milieu de 'é' |
| T11 | Emoji | `"🦀_rust"` | 🦀 = 4 bytes, segment `"🦀_"` (5) + `"rust"` |
| T12 | Texte vide | `""` | 0 tokens |
| T13 | Un seul char | `"x"` | 1 token: `"x"` |
| T14 | Double colon C++ | `"std::vector"` | seg `"std::"` + seg `"vector"` |
| T15 | Tabs et newlines | `"hello\t\nworld"` | `"hello\t\n"` + `"world"` |

---

## 2. Overlap

| # | Cas | Tokens | Overlap attendu |
|---|-----|--------|----------------|
| O1 | Normal 2 bytes | `["mutex_", "lock"]` | `"mutex_lo"`, `"lock"` |
| O2 | Token suivant court (1 byte) | `["abc_", "x"]` | overlap=1 (`"abc_x"`), pas 2 |
| O3 | Dernier token → pas d'overlap | `["init"]` | `"init"` (overlap=0) |
| O4 | Token suivant = 0 bytes | impossible (vide filtré) | — |
| O5 | Overlap sur char UTF-8 multi-byte | `["café_", "über"]` | overlap="üb"? Non: overlap=2 BYTES, "ü"=2bytes → overlap="ü" (2 bytes = 1 char) |

---

## 3. Partition stripped (0x02)

| # | Cas | Query | Texte indexé | strict_sep | Attendu |
|---|-----|-------|-------------|:---:|---------|
| S1 | Trigram cross-sep | `"exl"` | `"mutex_lock"` | false | Trouvé via `"exlo"` dans 0x02 |
| S2 | Même trigram strict=true | `"exl"` | `"mutex_lock"` | true | PAS trouvé (pas dans 0x01) |
| S3 | Query sans sep, texte avec | `"mutexlock"` | `"mutex_lock"` | false | Trouvé (query stripped) |
| S4 | Query avec sep diff | `"mutex lock"` | `"mutex_lock"` | false | Trouvé (query stripped → "mutexlock") |
| S5 | Query avec même sep | `"mutex_lock"` | `"mutex_lock"` | true | Trouvé (bytes identiques) |
| S6 | Query avec plus de seps | `"mutex__lock"` | `"mutex_lock"` | false | Trouvé (query stripped) |
| S7 | Texte avec longs seps | `"mutexlock"` | `"mutex________lock"` | false | Trouvé (traverse pure-sep tokens) |
| S8 | Que des seps dans la query | `"___"` | `"a___b"` | false | Query stripped = "" → vide |
| S9 | Que des seps dans la query | `"___"` | `"a___b"` | true | Trouvé dans 0x01 si suffixe `"___"` existe |

---

## 4. Falling walk — split et chaînage

| # | Cas | Query | Tokens | Attendu |
|---|-----|-------|--------|---------|
| F1 | Split simple 2 tokens | `"mutex_lock"` | `["mutex_lo", "lock"]` | Chain [0, 1] |
| F2 | Split 3 tokens | `"mutex_lock_init"` | `["mutex_lo", "lock_in", "init"]` | Chain [0, 1, 2] |
| F3 | Query dans un seul token | `"tex"` | `["mutex_lo"]` | Single-token match, pas de chain |
| F4 | Query commence à la fin d'un token | `"ck_init"` | `["lock_in", "init"]` | Chain commençant dans overlap zone |
| F5 | Query = exactement un token | `"mutex_lo"` | `["mutex_lo"]` | fst_candidates exact, pas de split |
| F6 | Query = overlap zone seulement | `"lo"` | `["mutex_lo", "lock"]` | Trouvé à STI=6 (overlap) ET STI=0 de "lock" |
| F7 | Query dépasse 3 tokens | `"a_b_c_d"` | 4 tokens | Chain [0,1,2,3] |
| F8 | Pure sep tokens traversés | `"mutexlock"` | `["mutex__", "____", "lock"]` | Chain traverse le pure-sep |

---

## 5. Fuzzy — trigram pigeonhole

| # | Cas | Query | Texte | d | Attendu |
|---|-----|-------|-------|:-:|---------|
| FZ1 | Exact match d=0 | `"mutex_lock"` | `"mutex_lock"` | 0 | Trouvé |
| FZ2 | Typo 1 char | `"mutex_lck"` | `"mutex_lock"` | 1 | Trouvé |
| FZ3 | Typo 2 chars | `"mutx_lk"` | `"mutex_lock"` | 2 | Trouvé |
| FZ4 | Pas de match | `"zzzzzzzzz"` | `"mutex_lock"` | 1 | Pas trouvé |
| FZ5 | Query courte → bigrams | `"ab"` | `"abc"` | 1 | Trouvé via bigrams |
| FZ6 | Query 1 char | `"m"` | `"mutex"` | 0 | Trouvé (single char) |
| FZ7 | d=0 strict_sep=false sans sep | `"mutexlock"` | `"mutex_lock"` | 0 | Trouvé via stripped |
| FZ8 | d=1 strict_sep=false | `"mutexlck"` | `"mutex_lock"` | 1 | Trouvé (stripped + fuzzy) |
| FZ9 | Multi-occurrence | `"lock"` | `"lock_lock_lock"` | 0 | 3 occurrences |

---

## 6. Regex — littéraux + gaps

| # | Cas | Pattern | Texte | Attendu |
|---|-----|---------|-------|---------|
| R1 | Littéral simple | `"mutex"` | `"mutex_lock"` | Trouvé |
| R2 | Dot-star | `"mutex.*lock"` | `"mutex_lock"` | Trouvé (AcceptAnything gap) |
| R3 | Char class | `"mutex[_]+lock"` | `"mutex_lock"` | Trouvé |
| R4 | Pas de match | `"mutex[0-9]+lock"` | `"mutex_lock"` | Pas trouvé (digits attendus) |
| R5 | Multi-littéral | `"hello.*mutex.*lock"` | `"hello_mutex_lock"` | Trouvé |
| R6 | Pas de littéral viable | `"[a-z]+"` | `"hello"` | Fallback (pas de littéral ≥ 2 chars) |
| R7 | Regex spéciaux | `"std::vector"` | `"std::vector<int>"` | Trouvé (:: échappé en regex) |

---

## 7. Adjacence et multi-doc

| # | Cas | Docs | Query | Attendu |
|---|-----|------|-------|---------|
| A1 | Trouvé dans bon doc | `["mutex_lock", "hello"]` | `"mutex"` | Doc 0 seulement |
| A2 | Trouvé dans 2 docs | `["mutex_lock", "mutex_core"]` | `"mutex"` | Docs 0 et 1 |
| A3 | Cross-token adjacence | `["mutex_lock"]` | `"mutex_lock"` | Chain vérifie pos+1 |
| A4 | Pas d'adjacence faux positif | `["mutex", "lock"]` (docs séparés) | `"mutex_lock"` | Pas trouvé (tokens dans docs diff) |
| A5 | Multi-value boundary | doc avec values `["mutex_lock", "hello"]` | `"lock_hello"` | PAS trouvé (value boundary) |

---

## 8. Highlights

| # | Cas | Query | Texte | Highlight attendu |
|---|-----|-------|-------|--------------------|
| H1 | Single-token | `"tex"` | `"mutex_lock"` | byte_from=2, byte_to=5 |
| H2 | Cross-token | `"mutex_lock"` | `"mutex_lock"` | byte_from=0, byte_to=10 |
| H3 | Multi-occurrence | `"lock"` | `"lock_lock"` | 2 highlights séparés |

---

## 9. Exact match et anchor_start

| # | Cas | Query | anchor | exact | Texte | Attendu |
|---|-----|-------|:---:|:---:|-------|---------|
| E1 | contains normal | `"tex"` | false | false | `"mutex_lock"` | Trouvé |
| E2 | anchor_start | `"mutex"` | true | false | `"mutex_lock"` | Trouvé (SI=0) |
| E3 | anchor rejects substring | `"tex"` | true | false | `"mutex_lock"` | PAS trouvé |
| E4 | exact_match | `"mutex_lo"` | false | true | `"mutex_lock"` | Trouvé si byte span = query len |
| E5 | exact rejects partial | `"mute"` | false | true | `"mutex_lock"` | PAS trouvé (partial) |
| E6 | term = anchor + exact | `"mutex_lo"` | true | true | `"mutex_lock"` | Trouvé |

---

## 10. Cas extrêmes

| # | Cas | Description |
|---|-----|-------------|
| X1 | Query très longue | 2000+ bytes → rejeté par MAX_QUERY_LEN |
| X2 | Query vide | `""` → résultat vide |
| X3 | Doc vide | Document sans contenu → ignoré |
| X4 | Doc avec un seul char | `"x"` → indexé, cherchable |
| X5 | Emoji dans texte et query | `"🦀_rust"` cherché dans `"🦀_rust"` |
| X6 | Caractères chinois | `"漢字_テスト"` → multi-byte UTF-8, char boundaries respectés |
| X7 | Null bytes | `"hello\x00world"` → traité comme bytes normaux |
| X8 | Très long mot sans sep | `"a".repeat(100)` → split en chunks de 8, overlap entre chunks |
| X9 | Index vide | 0 documents → résultats vides pour toute query |
| X10 | 1000 docs identiques | Même texte × 1000 → 1000 résultats |

---

## 11. Multi-split content + sep dans un même mot

Le cas le plus complexe : un mot long + un sep long produisent un segment qui se split en 4+ chunks, dont certains sont du pur contenu, certains un mix content/sep, et certains du pur sep.

### Cas X11 — Segment 28 bytes → 4 chunks

```
Texte : "internationalization________initialization"

Mot 1 : "internationalization" (20) + sep "________" (8) = segment 28 bytes
  ceil(28/8) = 4 chunks : (7, 7, 7, 7)
  
  TI=0 : "interna"         content=7, sep=0, is_word_start=true,  WI=0
  TI=1 : "tionali"         content=7, sep=0, is_word_start=false, WI=0
  TI=2 : "zation_"         content=6, sep=1, is_word_start=false, WI=0  ← mix content/sep
  TI=3 : "_______"         content=0, sep=7, is_word_start=false, WI=0  ← pure sep

Mot 2 : "initialization" (14) = segment 14 bytes
  ceil(14/8) = 2 chunks : (7, 7)
  
  TI=4 : "initial"         content=7, sep=0, is_word_start=true,  WI=1
  TI=5 : "ization"         content=7, sep=0, is_word_start=false, WI=1

Overlaps :
  TI=0 + "ti" → "internati"    (9 bytes)
  TI=1 + "za" → "tionaliza"    (9 bytes)
  TI=2 + "__" → "zation___"    (9 bytes)  ← overlap = 2 sep bytes
  TI=3 + "in" → "_______in"    (9 bytes)  ← overlap = premiers bytes du mot suivant
  TI=4 + "iz" → "initializ"    (9 bytes)
  TI=5 : "ization"             (7 bytes, dernier, pas d'overlap)
```

### Queries à tester

| # | Query | strict_sep | Mécanisme attendu |
|---|-------|:---:|-------------------|
| X11a | `"nationalization________init"` | true | falling walk : chain TI=1→TI=2→TI=3→TI=4, seps matchés byte par byte |
| X11b | `"nationalizationinit"` | false | stripped partition : query stripped, traverse pure-sep TI=3 |
| X11c | `"zation_______initial"` | true | falling walk : TI=2→TI=3→TI=4, sep exact (7 underscores) |
| X11d | `"zationinitial"` | false | stripped : skip seps dans TI=2 et TI=3 |
| X11e | `"internati"` | true | single-token match dans TI=0 étendu "internati" (overlap "ti") |
| X11f | `"tionalization"` | true | chain TI=1→TI=2, cross-chunk dans le même mot |
| X11g | `"initialization"` | true | chain TI=4→TI=5, cross-chunk dans le même mot (mot 2) |
| X11h | `"ization"` | true | trouvé à TI=5 STI=0 (token entier) ET à TI=1+TI=2 cross-chunk ("...alizat" + "ion...") |

### Cas X12 — Séparateur très long (> 2× MAX_TOKEN)

```
Texte : "a" + "_".repeat(20) + "b"   →  "a____________________b"

Segment "a____________________" (21 bytes) → ceil(21/8) = 3 chunks : (7, 7, 7)
  TI=0 : "a______"         content=1, sep=6
  TI=1 : "_______"         content=0, sep=7    ← pure sep
  TI=2 : "_______"         content=0, sep=7    ← pure sep

Segment "b" (1 byte)
  TI=3 : "b"               content=1, sep=0
```

| # | Query | strict_sep | Attendu |
|---|-------|:---:|---------|
| X12a | `"a____________________b"` | true | chain TI=0→TI=1→TI=2→TI=3, seps exacts |
| X12b | `"ab"` | false | stripped + traverse 2 pure-sep tokens |
| X12c | `"a_b"` | false | stripped → "ab", traverse pure-sep |
| X12d | `"______"` | true | trouvé comme substring dans TI=0 ou TI=1 ou TI=2 |

### Cas X13 — Mot très long sans séparateur (100 bytes)

```
Texte : "a".repeat(100)

Segment "aaa...aaa" (100 bytes) → ceil(100/8) = 13 chunks
  TI=0..11 : "aaaaaaaa" (8 bytes each, content=8, sep=0)
  TI=12    : "aaaa"     (4 bytes, content=4, sep=0)

Tous les chunks sont identiques ("aaaaaaaa") sauf le dernier !
→ Même ordinal pour TI=0..11 (shared postings)
→ Overlap "aa" entre chaque chunk
```

| # | Query | Attendu |
|---|-------|---------|
| X13a | `"aaaaaaaaaaaa"` (12 a's) | chain cross-token (> 1 chunk de 8+2=10) |
| X13b | `"a"` | trouvé (single char, beaucoup d'occurrences) |
| X13c | `"aaaa"` (4 a's) | trouvé dans un seul chunk |

### Cas X14 — Mix emoji + séparateurs + contenu

```
Texte : "🦀__rust_lang"

🦀 = 4 bytes UTF-8

Segment "🦀__" : contenu 🦀 (4 bytes) + sep "__" (2 bytes) = 6 bytes
  → 1 chunk : "🦀__" (6 ≤ 8), content=4, sep=2

Segment "rust_" : contenu "rust" (4) + sep "_" (1) = 5 bytes
  → 1 chunk : "rust_" (5), content=4, sep=1

Segment "lang" : contenu "lang" (4) = 4 bytes
  → 1 chunk : "lang" (4), content=4, sep=0
```

| # | Query | strict_sep | Attendu |
|---|-------|:---:|---------|
| X14a | `"🦀"` | true | trouvé (4 bytes UTF-8, dans le FST) |
| X14b | `"🦀__rust"` | true | chain TI=0→TI=1, seps exacts |
| X14c | `"🦀rust"` | false | stripped → trouvé |
| X14d | `"rust_lang"` | true | chain TI=1→TI=2 |
| X14e | `"rustlang"` | false | stripped → trouvé |
