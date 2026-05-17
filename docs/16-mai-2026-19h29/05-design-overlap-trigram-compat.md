# Design : Overlap trigram-compatible entre tokens

**Date** : 16 mai 2026  
**Contexte** : chaque token doit avoir un overlap avec le suivant tel que tout trigram cross-token soit indexé dans au moins un des deux tokens.

---

## Le problème

Token A = `"mutex_"` (6 bytes), Token B = `"lock"` (4 bytes).  
Le trigram `"x_l"` chevauche A et B. Aujourd'hui il est un "boundary trigram" perdu.

Si A contient dans ses suffixes les 2 premiers bytes de B (overlap=2), alors :
- Suffixes de A étendu `"mutex_lo"` incluent `"x_lo"` à SI=4 → le trigram `"x_l"` est dedans ✓

**Règle** : overlap = `trigram_size - 1` = 2 bytes. Chaque token est étendu de 2 bytes du token suivant.

---

## La contrainte % 3

Idée de Lucie : chaque token (incluant overlap) a une longueur telle que `len % 3 == 0`.

Pourquoi : si la longueur du token étendu est un multiple de 3, les trigrams le couvrent uniformément sans reste. Aucun trigram partiel en fin de token — les trigrams s'alignent proprement.

Mais est-ce vraiment nécessaire ? Réfléchissons.

### Cas sans % 3

Token étendu `"mutex_lo"` = 8 bytes. Trigrams :
```
pos 0: "mut"
pos 1: "ute"
pos 2: "tex"
pos 3: "ex_"
pos 4: "x_l"  ← celui qu'on voulait
pos 5: "_lo"
```
6 trigrams. 8 % 3 = 2. Il reste 2 bytes à la fin ("lo") qui ne forment pas un trigram complet seuls, mais c'est pas grave — ils sont couverts par le trigram à pos 5. **Tout va bien sans % 3.**

### Cas avec % 3

Si on forçait 9 bytes (% 3 == 0), on prendrait 3 bytes d'overlap au lieu de 2 :
Token étendu `"mutex_loc"` = 9 bytes. Trigrams :
```
pos 0: "mut"
pos 1: "ute"
pos 2: "tex"
pos 3: "ex_"
pos 4: "x_l"
pos 5: "_lo"
pos 6: "loc"  ← bonus
```
7 trigrams. L'overlap de 3 donne un trigram bonus "loc" qui est aussi un suffix de "lock". C'est redondant mais pas inutile pour le scoring (plus de matches → meilleur recall pour le pigeonhole).

### Verdict sur % 3

Le `% 3 == 0` n'est pas strictement nécessaire pour la correction (overlap=2 suffit). Mais il garantit un bonus : chaque token couvre un nombre ENTIER de trigrams, et le dernier trigram du token A overlap avec le premier trigram du token B. C'est plus propre pour le pigeonhole threshold.

**On peut le faire comme optimisation : ajuster l'overlap pour atteindre un multiple de 3.**

---

## Mécanique d'overlap — comment déterminer la taille

### Règle

Pour un token A de contenu `content_bytes` (après le tokenizer), suivi d'un token B :

```
base_len = content_len(A) + sep_len(A)           // token A avec trailing sep
overlap_needed = 3 - (base_len % 3)              // combien de bytes ajouter pour % 3
if overlap_needed == 3 { overlap_needed = 0 }    // déjà multiple de 3
overlap_needed = max(overlap_needed, 2)           // minimum 2 pour trigram compat

// Mais aussi : ne pas dépasser le token B
actual_overlap = min(overlap_needed, len(B))

extended_len = base_len + actual_overlap
```

### Exemples

```
A = "mutex" (5) + sep "_" (1) = 6 bytes
  base_len = 6, 6 % 3 = 0, overlap_needed = max(0, 2) = 2
  B = "lock" (4 bytes available)
  actual_overlap = min(2, 4) = 2
  Extended A : "mutex_lo" (8 bytes)
  8 % 3 = 2... pas multiple de 3 !

Hmm, on voulait % 3. Recalculons :
  base_len = 6, target = 6 + 2 = 8, 8 % 3 = 2
  Pour atteindre % 3 : 9 bytes → overlap = 3
  Extended A : "mutex_loc" (9 bytes) ✓ 9 % 3 == 0
```

Autre approche : **on calcule l'overlap comme le plus petit nombre ≥ 2 tel que (base_len + overlap) % 3 == 0** :

```python
def compute_overlap(base_len, available_from_next):
    for ov in range(2, min(5, available_from_next + 1)):
        if (base_len + ov) % 3 == 0:
            return ov
    return 2  # fallback : au moins 2
```

### Table des cas

| base_len % 3 | overlap pour % 3 == 0 | overlap effectif |
|:---:|:---:|:---:|
| 0 | 3 (0+3=3 ✓) | 3 |
| 1 | 2 (1+2=3 ✓) | 2 |
| 2 | 4 (2+4=6 ✓) ou 1... mais min=2 → 4 | 4 |

Hmm, quand base_len % 3 == 2, il faut overlap=4 pour atteindre % 3 == 0. C'est beaucoup. Alternative : overlap=1 donne base+1 qui est % 3 == 0, mais overlap=1 < minimum 2.

**Peut-être que forcer % 3 sur le token étendu n'est pas le bon angle.** Le vrai besoin c'est : **overlap ≥ 2** pour que tout trigram cross-token soit couvert. Le % 3 est un nice-to-have.

---

## Approche simplifiée : overlap fixe = 2, pas de contrainte % 3

```
Règle : chaque token dans le FST est étendu de min(2, len(next_token)) bytes.

Token A dans le FST = content(A) + trailing_sep(A) + first_2_bytes(B)
```

### Exemples complets

```
Texte : "pthread_mutex_lock_init"
Mots : ["pthread", "mutex", "lock", "init"]
Tokens (max-len=5, trailing sep) : ["pthre", "ad_", "mutex_", "lock_", "init"]

Token étendu dans le FST (overlap=2 du suivant) :
  TI=0 : "pthre" + "ad" = "pthread"       (7 bytes, overlap 2 du TI=1)
  TI=1 : "ad_"   + "mu" = "ad_mu"         (5 bytes, overlap 2 du TI=2)
  TI=2 : "mutex_"+ "lo" = "mutex_lo"      (8 bytes, overlap 2 du TI=3)
  TI=3 : "lock_" + "in" = "lock_in"       (7 bytes, overlap 2 du TI=4)
  TI=4 : "init"          = "init"          (4 bytes, dernier token, pas d'overlap)
```

### Suffixes dans le FST pour TI=2 "mutex_lo"

```
SI=0: mutex_lo    → ord=2, STI=0, token_len=8, is_word_start=true, sep_len=1, overlap=2
SI=1: utex_lo     → ord=2, STI=1, ...
SI=2: tex_lo
SI=3: ex_lo
SI=4: x_lo        ← contient le trigram "x_l" ✓
SI=5: _lo         ← contient le trigram "_lo" ✓
SI=6: lo          ← overlap zone (ces 2 bytes sont AUSSI dans TI=3)
SI=7: o           ← overlap zone
```

Les suffixes SI=6 et SI=7 sont dans la **zone d'overlap** — ils sont aussi des suffixes de TI=3 "lock_in" (à SI=0 et SI=1). Ils sont indexés deux fois dans le FST, mais avec des ordinals différents :
- `"lo"` → multi-parent : (ord=2, STI=6) ET (ord=3, STI=0)

C'est le "indexés deux fois" que Lucie mentionne — pas grave, c'est juste des entrées OutputTable multi-parent.

### Ce que ça résout pour les trigrams

```
Query "mutex_lock" → trigrams : "mut", "ute", "tex", "ex_", "x_l", "_lo", "loc", "ock"

"mut" → suffix de "mutex_lo" SI=0 ✓ (single token)
"ute" → suffix de "mutex_lo" SI=1 ✓ (single token)
"tex" → suffix de "mutex_lo" SI=2 ✓ (single token)
"ex_" → suffix de "mutex_lo" SI=3 ✓ (single token)
"x_l" → suffix de "mutex_lo" SI=4 ✓ (single token !)
"_lo" → suffix de "mutex_lo" SI=5 ✓ (single token !)
"loc" → suffix de "lock_in" SI=0 ✓ (single token)
"ock" → suffix de "lock_in" SI=1 ✓ (single token)

ZERO boundary trigrams.
ZERO falling walk cross-token pour les trigrams.
Chaque trigram = 1 simple FST lookup.
```

---

## Comment distinguer overlap zone vs contenu

Le falling walk doit savoir : "est-ce que je suis encore dans CE token, ou dans l'overlap du suivant ?"

### Option A — overlap_len dans l'output

```
Encoder : token_own_len = token_len - overlap
          overlap_len = 2 (ou variable)

SI < token_own_len   → dans ce token
SI >= token_own_len  → dans l'overlap (= début du token suivant)
```

Quand le falling walk atteint SI >= token_own_len, il sait qu'il a dépassé ce token et est entré dans le suivant. C'est l'équivalent du split point actuel.

### Option B — token_own_len dans l'output

Plus simple : stocker `own_len` (longueur du token SANS overlap) au lieu de `token_len`.

```
STI + bytes_consumed == own_len  → SPLIT POINT (fin du vrai token)
```

Le falling walk continue naturellement dans l'overlap zone (les bytes sont là dans le FST), et le split point dit "à partir d'ici, les bytes appartiennent au token suivant".

### Encoding u64

```
[63: multi_flag]
[62: is_word_start]
[61..58: overlap_len]       ← 4 bits (0..15 bytes d'overlap, 2 suffit)
[57..56: sep_len_bits]      ← 2 bits pour sep_len court (0..3), ou flag "long sep"
[55..40: own_len]           ← 16 bits (longueur du token sans overlap)
[39..24: sti]               ← 16 bits
[23..0: token_ordinal]      ← 24 bits
```

`token_len_total = own_len + overlap_len` (dérivé, pas stocké).

---

## Falling walk avec overlap

```
Query : "x_lock_i"

1. Enter partition 0x01
2. Walk FST avec "x_lo" → match suffix de "mutex_lo" à STI=4
   STI(4) + 4 bytes consumed = 8
   own_len = 6 (mutex_ sans overlap)
   STI(4) + 2 = 6 = own_len → SPLIT POINT à byte 2 du walk !
   
   Mais on continue de walker dans l'overlap zone :
   bytes 2-3 du walk ("lo") sont dans l'overlap = vérification gratuite
   
3. À ce stade on a consommé "x_lo" (4 bytes) dont 2 dans l'overlap.
   next_token(TI=2) = TI=3 ("lock_in")
   
4. Walk reste de la query "ck_i" sur "lock_in" :
   STI=2 : ck_i → 4 bytes consumed
   own_len = 5, STI(2) + 4 = 6 > own_len(5) → dans l'overlap !
   SPLIT POINT à byte 3 du walk (STI+3 = 5 = own_len)
   
5. Continue : next_token(TI=3) = TI=4 ("init")
   Walk "i" sur "init" STI=0 → match

Result : tokens [2, 3, 4], "x_lock_i" trouvé.
```

**Le falling walk n'a JAMAIS besoin de la sibling table** car :
- next_token = TI + 1 (trivial)
- L'overlap garantit que les 2 premiers bytes du token suivant sont déjà dans le FST walk courant → validation gratuite

---

## Impact sur le fuzzy

### Avant (v2 actuelle)
```
"mutex_lock" fuzzy d=1 :
  concat → "mutexlock" (9 bytes)
  8 trigrams, 3 boundary (jetés)
  16 FST walks + sibling DFS
  ~200ms
```

### Après (cette vision)
```
"mutex_lock" fuzzy d=1 :
  Query telle quelle (10 bytes, "_" inclus)
  8 trigrams, 0 boundary
  8 FST walks simples (single-token chacun grâce à l'overlap)
  ~2-5ms
```

**Speedup : ×40-100 sur le fuzzy cross-token.**

---

## Questions restantes

1. **Max-len** : 5 ? 8 ? Benchmarker avec le linux dataset. Plus c'est petit, plus il y a de chunks (plus de suffixes dans le FST, mais tokens plus courts = moins de suffixes par token → peut s'annuler).

2. **Overlap variable** : toujours 2 ? Ou adapter au trigram_size (qui pourrait être 2 pour les queries courtes) ? Fixer à 2 est le plus simple.

3. **Dernier token** : pas d'overlap (rien après). Le trigram qui commence dans l'avant-dernier byte du dernier token est perdu. Acceptable ? Oui — le falling walk classique couvre ce cas (rare).

4. **Tokens très courts** : si un token fait 1-2 bytes (ex: `"a_"`), l'overlap de 2 double sa taille. Acceptable ? L'index grossit un peu mais c'est rare.

5. **Custom tokenizer** : les utilisateurs implémentent leur propre tokenizer. L'overlap est ajouté APRÈS le tokenizer (dans le SFX builder), pas dans le tokenizer lui-même. Le tokenizer produit des tokens normaux, le SFX builder ajoute l'overlap.

6. **Doublon dans le FST** : les bytes d'overlap apparaissent dans deux tokens. Le FST les déduplique naturellement (shared prefixes). Le OutputTable a des multi-parent entries. Le coût est dans la taille de la parent list, pas dans le FST lui-même.

---

## Conclusion — strict_separators

`strict_separators` est une logique de **matching**, pas d'**indexation**.

Les séparateurs sont indexés tels quels dans le FST (comme des bytes normaux). C'est au moment du search qu'on décide :

- `strict_separators=true` : le `_` dans la query doit matcher le `_` dans le FST. Distance Levenshtein classique.
- `strict_separators=false` : les bytes séparateurs dans le FST sont tolérés même si la query ne les a pas. Post-filtering après le FST walk / falling walk.

Pas de byte sentinel, pas de partition spéciale, pas de DFA modifié. Juste un post-filter sur les résultats du walk. Si un match traverse des bytes séparateurs qui ne sont pas dans la query, on accepte quand même (ou on ajuste la distance).

Raison : les séparateurs sont des bytes valides comme les autres. Un utilisateur pourrait vouloir chercher `":::"` ou `"___"`. L'indexation ne doit pas faire d'hypothèse sur ce qui est "spécial".

---

## Récapitulatif des modifications aux index

### Ce qui CHANGE par rapport à v2 actuelle

#### 1. Tokenizer

| Avant (v2) | Après (v3) |
|------------|------------|
| Split sur non-alphanumérique | Split sur non-alphanumérique (inchangé) |
| CamelCase split | **Supprimé** — remplacé par max-len |
| Pas de max-len | **Max-len** (ex: 8 bytes) sur chaque mot |
| Séparateurs jetés | **Trailing sep absorbés** dans le dernier chunk du mot |

#### 2. Output u64 du FST

| Champ | Avant | Après |
|-------|-------|-------|
| `ordinal` (24 bits) | token ordinal | **token_ordinal (TI)** — index du chunk |
| `si` (16 bits) | position dans le token | **STI** — position dans le chunk (inclut trailing sep + overlap) |
| `token_len` (16 bits) | longueur du token | **own_len** — longueur du chunk SANS overlap |
| `multi_flag` (1 bit) | multi-parent | inchangé |
| bit 62 | libre | **is_word_start** — ce suffix commence au début d'un mot logique |
| bits 61..58 | libres | **overlap_len** (4 bits) — bytes d'overlap du token suivant |
| bits 57..56 | libres | **sep_len** (2 bits) — bytes de séparateur trailing (0..3) |

Dérivés :
- `content_len = own_len - sep_len - overlap_len` (longueur du contenu alphanumérique pur)
- `extended_len = own_len + overlap_len` (longueur totale dans le FST)
- `is_in_overlap = STI >= own_len`
- `is_in_separator = STI >= content_len && STI < content_len + sep_len`

#### 3. Sibling table

| Avant | Après |
|-------|-------|
| `SiblingEntry { next_ordinal, gap_len }` | **Supprimé** — remplacé par deux tables |
| Contiguous vs gap filtering | Plus nécessaire (tous gap=0) |
| DFS sur sibling chains | Plus nécessaire |

Nouvelles tables :

| Table | Contenu | Lookup |
|-------|---------|--------|
| **next_token** | Implicite : TI + 1 | O(1), pas de table |
| **next_word** | `[u32; num_tokens]` — TI du premier token du prochain mot | O(1) |

`next_sep` (prochain token avec trailing sep) abandonné — pas assez utile.

#### 4. Word map (NOUVEAU)

| Table | Contenu | Usage |
|-------|---------|-------|
| `token_to_word[TI]` → WI | Quel mot contient ce token | BM25 scoring |
| `word_start_token[WI]` → TI | Premier token du mot WI | startsWith, term |
| `word_content_len[WI]` → u16 | Longueur du mot (tous chunks, sans sep) | term exact match |

#### 5. GapMap

| Avant | Après |
|-------|-------|
| Stocke les bytes séparateurs entre tokens | **Supprimé** — les séparateurs sont dans les tokens (trailing) |

Les bytes séparateurs sont reconstruits depuis le token lui-même (bytes entre content_len et content_len + sep_len).

#### 6. SepMap

| Avant | Après |
|-------|-------|
| Bitmap 256 bits des separator bytes observés par ordinal | **Simplifié** ou supprimé — les sep bytes sont dans le FST directement |

Pour regex, les sep bytes sont visibles dans le FST walk. Plus besoin de bitmap externe.

#### 7. SFX builder — overlap

| Avant | Après |
|-------|-------|
| Chaque token génère ses suffixes indépendamment | Chaque token génère ses suffixes **+ overlap** (2 bytes du token suivant) |
| Cross-token trigrams sont des "boundary trigrams" jetés | **Tous les trigrams** sont dans le FST grâce à l'overlap |

L'overlap est ajouté dans le SFX builder, PAS dans le tokenizer. Le tokenizer est inchangé (sauf max-len).

#### 8. Falling walk

| Avant | Après |
|-------|-------|
| Split point : `si + prefix_len == token_len` | Split point : `STI + bytes_consumed == own_len` |
| Après split : lookup sibling table (DFS) | Après split : **TI + 1** (trivial) |
| Gap validation via GapMap | Plus nécessaire (gap=0, sep dans le token) |
| Cross-token = expensive sibling DFS | Cross-token = **simple TI increment** |

#### 9. Fuzzy contains

| Avant | Après |
|-------|-------|
| `concat_query()` strip les séparateurs | **Supprimé** — query telle quelle |
| Boundary trigrams jetés (threshold abaissé) | **Plus de boundary trigrams** (overlap) |
| 16 FST walks + sibling DFS | **8 FST walks simples** |
| ~200ms | **~2-5ms** |

#### 10. Fichiers par segment

| Fichier | Avant | Après |
|---------|-------|-------|
| `.sfx` | FST + parent lists + sibling table + GapMap | FST + parent lists + **next_word table** + **word_map** |
| `.sfxpost` | inchangé | inchangé |
| `.termtexts` | inchangé | inchangé |
| `.gapmap` | séparateurs per-doc per-token | **supprimé** |
| `.sepmap` | bitmap separator bytes per ordinal | **supprimé** ou simplifié |
