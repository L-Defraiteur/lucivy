# Décisions algorithmiques v3 — Référence finale

**Date** : 16 mai 2026  
**Statut** : décisions verrouillées, prêt à prototyper  
**Documents sources** : docs 01-08 de cette session

Ce document est la référence unique de toutes les décisions prises. Il se suffit à lui-même — pas besoin de lire les docs précédents pour implémenter.

---

## 1. Tokenizer — Division égale du segment

### Unité de base : le segment

Un segment = un mot alphanumérique + les séparateurs trailing qui le suivent (jusqu'au prochain mot).

```
Texte : "pthread_mutex__lock"
Segments : ["pthread_", "mutex__", "lock"]
             mot+sep     mot+sep    mot
```

### Algorithme de chunking

```
MAX_TOKEN = 8  (bytes)

Pour chaque segment :
  Si segment.len() ≤ MAX_TOKEN → 1 token, pas de split
  Sinon :
    num_chunks = ceil(segment.len() / MAX_TOKEN)
    base_size = segment.len() / num_chunks
    extra = segment.len() % num_chunks
    Les premiers `extra` chunks font base_size + 1 bytes
    Les suivants font base_size bytes
    Respecter les frontières UTF-8 (ajuster ± 1 byte si nécessaire)
```

### Pas d'orphelins

La division égale garantit : chunk_min ≥ `segment.len() / ceil(segment.len() / MAX_TOKEN)`.
Pour MAX_TOKEN=8, un segment de 9 bytes donne 2 chunks de (5, 4) — pas de chunk de 1 byte.

### Metadata par token

Chaque token porte :
- `content_len` : bytes alphanumériques dans ce chunk
- `sep_len` : bytes de séparateur trailing (0 sauf pour le dernier chunk du segment)
- `is_word_start` : ce chunk est le premier du mot (= premier chunk du segment)
- `word_id` (WI) : identifiant du mot logique auquel ce chunk appartient

### Exemples complets

```
MAX_TOKEN = 8

"mutex_lock" :
  Segments : ["mutex_", "lock"]
  "mutex_" (6 ≤ 8) → 1 chunk
  "lock" (4 ≤ 8) → 1 chunk
  Tokens :
    TI=0 : "mutex_"   content=5, sep=1, is_word_start=true,  WI=0
    TI=1 : "lock"     content=4, sep=0, is_word_start=true,  WI=1

"pthread_mutex_lock" :
  Segments : ["pthread_", "mutex_", "lock"]
  Tokens :
    TI=0 : "pthread_" content=7, sep=1, is_word_start=true,  WI=0
    TI=1 : "mutex_"   content=5, sep=1, is_word_start=true,  WI=1
    TI=2 : "lock"     content=4, sep=0, is_word_start=true,  WI=2

"getElementById" :
  Segments : ["getElementById"]  (pas de sep)
  14 > 8 → 2 chunks : ceil(14/8)=2, 14/2=7, extra=0 → (7, 7)
  Tokens :
    TI=0 : "getElem"  content=7, sep=0, is_word_start=true,  WI=0
    TI=1 : "entById"  content=7, sep=0, is_word_start=false, WI=0

"Error::LucivyError" :
  Segments : ["Error::", "LucivyError"]
  "Error::" (7 ≤ 8) → 1 chunk
  "LucivyError" (11 > 8) → 2 chunks : (6, 5)
  Tokens :
    TI=0 : "Error::"  content=5, sep=2, is_word_start=true,  WI=0
    TI=1 : "Lucivy"   content=6, sep=0, is_word_start=true,  WI=1
    TI=2 : "Error"    content=5, sep=0, is_word_start=false, WI=1

"a________b" :
  Segments : ["a________", "b"]
  "a________" (9 > 8) → 2 chunks : (5, 4)
  Tokens :
    TI=0 : "a____"    content=1, sep=4, is_word_start=true,  WI=0
    TI=1 : "____"     content=0, sep=4, is_word_start=false, WI=0
    TI=2 : "b"        content=1, sep=0, is_word_start=true,  WI=1
```

### Pas de CamelCase split

Le CamelCase split est supprimé. `"getElementById"` n'est plus splitté à "get"/"Element"/"By"/"Id" mais par division égale à MAX_TOKEN. Raison : la division arbitraire par taille est plus simple, prévisible, et compatible avec le mécanisme d'overlap.

### BM25 tokenizer inchangé

Le tokenizer BM25 (pour le scoring) reste le même : split sur non-alphanum, pas de maxlen. Seul le tokenizer SFX change. Les deux produisent des mots identiques — seul le chunking interne au SFX diffère.

---

## 2. SFX Builder — Overlap de 2 bytes

### Principe

Chaque token (sauf le dernier du document) est étendu de `min(2, len(next_token))` bytes du token suivant. Ces bytes sont ajoutés **après** le token (content + sep + overlap).

### Ce que ça résout

Tout trigram (ou bigram) qui chevauche une frontière de token est indexé dans au moins un token grâce à l'overlap. Plus aucun "boundary trigram" perdu.

### Construction

```
Tokens :     ["mutex_",  "lock_",  "init"]
              TI=0        TI=1      TI=2

Extended :   ["mutex_lo", "lock_in", "init"]
              own=6 ov=2  own=5 ov=2  own=4 ov=0

Suffixes de "mutex_lo" dans le FST :
  STI=0 : mutex_lo   (8 bytes)
  STI=1 : utex_lo
  STI=2 : tex_lo
  STI=3 : ex_lo
  STI=4 : x_lo       ← trigram "x_l" cross-boundary ✓
  STI=5 : _lo        ← trigram "_lo" cross-boundary ✓
  STI=6 : lo          ← overlap zone (aussi suffix de "lock_in" STI=0)
  STI=7 : o           ← overlap zone
```

### Overlap fixe = 2

Pas d'overlap variable. 2 bytes suffit pour couvrir tous les trigrams (n=3, overlap ≥ n-1 = 2) et tous les bigrams (n=2, overlap ≥ 1). Fixe = simple, prévisible.

### Multi-parent dans l'overlap zone

Les bytes d'overlap apparaissent dans deux tokens. Le FST les partage naturellement via des entries multi-parent dans l'OutputTable. Pas de duplication du FST lui-même (shared prefixes).

### Pré-filtrage des postings

L'ordinal de `"mutex_lo"` est différent de `"mutex_co"` (si dans un autre doc "mutex_" est suivi de "core_"). La posting list de chaque ordinal ne contient que les documents où cette combinaison token+overlap apparaît → pré-filtrage gratuit du contexte droit.

---

## 3. Encoding output u64

### Format v3

```
Bit 63    : multi_flag        (1=pointer vers OutputTable, 0=inline)
Bit 62    : is_word_start     (1=premier chunk du mot, 0=chunk interne)
Bits 61-58: overlap_len       (4 bits, 0..15 — en pratique 0 ou 2)
Bits 57-55: sep_len           (3 bits, 0..7)
Bits 54-40: own_len           (15 bits, max 32767 — longueur du chunk sans overlap)
Bits 39-24: sti               (16 bits — position du suffix dans le chunk étendu)
Bits 23-0 : token_ordinal     (24 bits — TI)
```

### Valeurs dérivées

```
content_len = own_len - sep_len           (bytes alphanumériques du chunk)
extended_len = own_len + overlap_len      (longueur totale dans le FST)
is_in_content = STI < content_len
is_in_sep = STI >= content_len && STI < own_len
is_in_overlap = STI >= own_len
```

### Multi-parent (bit 63 = 1)

Identique à v2 : les 63 bits bas sont un offset dans l'OutputTable. Le format OutputTable change pour inclure les nouveaux champs (is_word_start, overlap_len, sep_len, own_len).

---

## 4. Falling walk exact — Algorithme v3

### Split point

```
Condition de split : STI + bytes_consumed == own_len
```

Le walk NE break PAS au split point — il continue dans l'overlap zone pour valider les premiers bytes du token suivant.

### Algorithme

```
Pour chaque partition (0x00 SI=0, 0x01 SI>0) :
  node = racine FST, suivre prefix byte
  Pour chaque byte[i] de la query :
    Suivre transition byte[i] dans node
    Si pas trouvé : break
    Avancer au noeud suivant, accumuler output
    
    consumed = i + 1
    Si node.is_final() :
      Décoder parents depuis output
      Pour chaque parent :
        pos_in_token = parent.sti + consumed
        Si pos_in_token == parent.own_len :
          → SPLIT POINT : émettre SplitCandidate(prefix_len=consumed)
          (ne pas break, continuer pour l'overlap)
        Si pos_in_token == parent.own_len + parent.overlap_len :
          → Fin de l'overlap, noter overlap_validated = overlap_len
  
  Trier candidats par prefix_len décroissant
```

### Cross-token par chaînage

```
Quand un split est détecté :
  1. Émettre SplitCandidate pour le token courant
  2. Calculer remainder = query[split_point..]
  3. Lancer un nouveau falling_walk(remainder)
  4. Les résultats donnent des ordinals candidats pour TI+1
  5. Résoudre les postings des deux côtés
  6. Vérifier position adjacency : pos_right == pos_left + 1
  7. Si le nouveau walk a aussi un split → répéter (boucle)
  
  Coût total : O(query_len), identique à v2
```

Pas de sibling table, pas de DFS, pas de text lookup de candidats. Le FST trie est le filtre le plus sélectif.

---

## 5. Sep-skip — strict_separators=false dans le falling walk

### Problème

En v3, les séparateurs sont des bytes dans le token. Si la query a un séparateur différent (ou pas de séparateur), le walk byte-par-byte échoue.

### Solution : skip intégré dans le falling walk

```
Quand STI + consumed == content_len ET strict_separators=false :
  1. Skip sep_len bytes dans le TOKEN (avancer le curseur FST sans comparer)
  2. Skip N bytes dans la QUERY (avancer tant que !is_alphanumeric)
  3. Reprendre la comparaison dans l'overlap zone
     (overlap commence à position content_len + sep_len dans le token)
```

### Cas couverts

| Query | Texte | Mécanisme |
|-------|-------|-----------|
| `"mutex_lock"` | `mutex_lock` | Walk exact, bytes identiques |
| `"mutex lock"` | `mutex_lock` | Sep-skip : skip espace query + skip `_` token |
| `"mutexlock"` | `mutex_lock` | Sep-skip : skip 0 bytes query + skip `_` token |
| `"mutex__lock"` | `mutex_lock` | Sep-skip : skip `__` query + skip `_` token |

### Ce que ça n'est PAS

Ce n'est PAS du post-filtering. Le walk lui-même échoue sans sep-skip — il n'y a rien à filtrer. Le sep-skip est intégré dans la boucle du falling walk.

---

## 6. Fuzzy contains — Pipeline simplifié

### Supprimé

- `concat_query()` : la query est utilisée telle quelle (lowercase seulement)
- `boundary_positions()` : plus de boundaries à calculer
- `boundary_trigram_indices()` : plus de compensation
- `cross_token_falling_walk()` dans la boucle trigram : plus de cross-token pour les trigrams
- `resolve_cross_with_parts()` : plus de chains

### Threshold simplifié

```
v2 : threshold = max(T - n*d - (n-1)*boundaries, min_threshold)
v3 : threshold = max(T - n*d, 1)
```

Plus strict → moins de faux positifs → moins de postings → plus rapide.

### Pipeline v3

```
1. lowercase(query)                    (PAS de concat, PAS de strip seps)
2. generate_trigrams(query, distance)  (trigrams incluent les bytes de sep)
3. Pour chaque trigram :
     fst_candidates(sfx, trigram)      (FST walk simple, single-token)
     selectivity = candidates.len()
4. Trier par sélectivité croissante (rarest first)
5. Résoudre rarest sans filtre → doc_filter
6. Résoudre reste avec doc_filter
7. build_hits_by_doc → find_matches (two-pointer sliding window)
8. Scoring : miss_count → miss_penalty * 1000 + bm25
```

Tous les trigrams sont single-token grâce à l'overlap. Aucun cross-token falling walk.

### Pourquoi ça marche

```
Query : "mutex_lock" → trigrams : "mut","ute","tex","ex_","x_l","_lo","loc","ock"

"x_l" → suffix de "mutex_lo" à STI=4 → single-token ✓
"_lo" → suffix de "mutex_lo" à STI=5 → single-token ✓

ZERO boundary trigrams. Chaque trigram = 1 FST lookup.
```

### Gain estimé

```
v2 : 7 FST walks + 2 cross_token_falling_walk + sibling DFS → ~100-200ms
v3 : 8 FST walks simples → ~2-5ms
Speedup : ×40-100
```

---

## 7. Exact contains — Falling walk chaîné + term dict fast-path

### Single-token

Inchangé : `prefix_walk` / `prefix_walk_si0` sur le FST.

### Cross-token

Remplace le sibling DFS par le falling walk chaîné (section 4). Le fallback `prefix_walk_si0(remainder)` qui existait déjà en v2 (lignes 269-287 de `literal_pipeline.rs`) est le mécanisme principal.

### Pas de fast-path term dict

Toutes les queries passent par le SFX pour ne pas manquer de candidats substring. Le term dict sert uniquement aux stats BM25 et au word_map, pas au routing des queries.

### Multi-token

Inchangé : résolution par sous-token + pivot optimization + position adjacency. Chaque sous-token utilise le pipeline simplifié (pas de sibling).

---

## 8. Regex continuation — DFA étendu sans siblings

### Supprimé

- `continuation_score_sibling()` : supprimé entièrement
- Appels au gapmap : supprimés (les gap bytes sont dans les tokens)

### Mécanisme v3

```
Walk 1 : DFA × FST → candidates(dfa_state)
  Le token inclut trailing sep + overlap
  → Le DFA consomme PLUS de bytes qu'en v2
  → Plus de queries résolues en 1 walk

Walk 2 : Pour chaque candidat non-accepting :
  search_continuation(DFA, dfa_state) → FST walk avec DFA state sauvé
  → Trouve directement les ordinals qui font avancer le DFA
  → Pas de sibling enumeration, pas de gap feeding
```

C'est le fallback `continuation_score` qui existait déjà en v2. Il devient le chemin principal.

### Sep dict (optimisation optionnelle)

Pour les regex avec littéraux séparateurs (ex: `"[a-z]+::[a-z]+"`), lookup dans le sep dict pour pré-filtrer les documents contenant `"::"`. Intersection avec les résultats SFX.

---

## 9. Term dict et Sep dict — Nouveaux index

### Term dict (`.terms`)

FST de mots entiers → posting lists. Construit pendant l'indexation en parallèle du SFX builder.

```
Pour chaque mot complet dans le document :
  term_dict.add(word_text, doc_id, word_position)
```

Sert à :
- `term()` : lookup O(1)
- `startsWith()` : prefix scan
- `range()` : range scan
- BM25 stats : TF et DF par mot

### Sep dict (`.seps`)

FST de séparateurs → posting lists.

```
Pour chaque séparateur non-vide entre deux mots :
  sep_dict.add(sep_text, doc_id, position)
```

Sert à :
- Regex avec littéraux séparateurs (pré-filtrage)
- Recherche de patterns de formatage

### Word map (dérivé du term dict)

```
token_to_word[TI] → WI     Quel mot contient ce chunk
word_start_token[WI] → TI  Premier chunk du mot WI
next_word[TI] → TI         Prochain token is_word_start=true
word_content_len[WI] → u16 Longueur totale du mot (tous chunks, sans sep)
```

Le WI est l'ordinal dans le term dict. Pas de structure dupliquée.

---

## 10. Structures supprimées

### Sibling table → SUPPRIMÉE

**Raison** : l'overlap de 2 bytes + falling walk chaîné remplace le sibling DFS. Le falling walk sur le remainder est aussi rapide (O(remainder_len) dans le trie) et ne nécessite pas de table de voisinage.

Les siblings étaient nécessaires en v2 car :
- Les tokens n'avaient pas de séparateurs → le walk cassait à la frontière
- Il fallait savoir "quels tokens peuvent suivre" pour continuer

En v3 :
- Les seps sont dans les tokens → le walk traverse les seps naturellement
- L'overlap donne 2 bytes du token suivant dans le FST → le walk va plus loin
- TI+1 est implicite (tokens linéaires) → pas besoin de table

### GapMap → SUPPRIMÉ

**Raison** : les séparateurs sont dans les tokens (trailing sep). Le gap entre deux tokens est toujours 0. Plus besoin de stocker les bytes de gap per-doc per-token.

### SepMap → SUPPRIMÉ

**Raison** : les bytes de séparateurs sont dans le FST directement (dans les tokens). Le DFA/regex les traverse naturellement. Pour le cas spécifique de "quels séparateurs existent dans le corpus", le sep dict le remplace.

### concat_query → SUPPRIMÉ

**Raison** : la query est utilisée telle quelle, séparateurs inclus. L'overlap garantit que tous les trigrams (y compris ceux contenant des seps) sont dans le FST.

### boundary_trigram_indices → SUPPRIMÉ

**Raison** : plus de boundary trigrams. L'overlap couvre tout. Le threshold se simplifie à `T - n*d`.

### cross_token_falling_walk (dans fuzzy) → SUPPRIMÉ

**Raison** : tous les trigrams sont single-token grâce à l'overlap. Pas besoin de cross-token pour la résolution des trigrams.

### continuation_score_sibling → SUPPRIMÉ

**Raison** : remplacé par `continuation_score` (FST DFA walk). Les gap bytes n'ont plus besoin d'être feedés manuellement au DFA (ils sont dans le token).

### CamelCaseSplitFilter → DEPRECATED

**Raison** : remplacé par la division égale à MAX_TOKEN. Plus simple, plus prévisible, compatible avec l'overlap.

---

## 11. Fichiers segment v3

| Fichier | Contenu | Taille estimée | Queries |
|---------|---------|:-:|---|
| `.terms` | **NOUVEAU** — FST mots entiers + postings | ~10% | term, prefix, range, BM25 |
| `.seps` | **NOUVEAU** — FST séparateurs + postings | ~2% | regex sep lookup |
| `.sfx` | FST suffixes + parent lists + next_word + word_map | ~55% | contains, fuzzy, regex |
| `.sfxpost` | SFX posting lists | ~25% | (résolution SFX) |
| `.termtexts` | Token texts pour reconstruction | ~8% | highlights, cross-token |

**Supprimés** : `.gapmap`, `.sepmap`, sibling table (section du `.sfx`).

**Taille totale** : ×1.10-1.15 vs v2.

---

## 12. Routing des queries

| Type | v2 (actuel) | v3 |
|------|-------------|-----|
| `term("mutex")` | SFX SI=0 + anchor_start + exact_match | **SFX (inchangé)** — term dict pour BM25 stats seulement |
| `startsWith("get")` | SFX SI=0 + anchor_start | **SFX (inchangé)** |
| `range("a".."z")` | SFX range scan | **SFX (inchangé)** |
| `contains("tex")` | SFX falling walk + sibling DFS | **SFX falling walk chaîné** |
| `contains_split("a b")` | Split → boolean should contains | Inchangé |
| `fuzzy("mutx", d=1)` | SFX trigrams + cross_token_falling_walk | **SFX trigrams single-token** |
| `regex("a.*b")` | SFX DFA + continuation_score_sibling | **SFX DFA + continuation_score** |
| `phrase("a b c")` | SFX multi-token + sibling | **SFX multi-token sans sibling** |
| `boolean` | Composite | Inchangé |
| `disjunction_max` | Composite | Inchangé |
| `more_like_this` | TF-IDF natif | Inchangé |

---

## 13. FST Partitions

Inchangé : 2 partitions.

```
0x00 — STI=0 (début de token/chunk)
  Utilisé par : startsWith, term (avec filtre is_word_start)
  
0x01 — STI>0 (substring)
  Utilisé par : contains
```

Le bit `is_word_start` dans l'output u64 permet de distinguer :
- `startsWith("mutex")` : partition 0x00, is_word_start=true → match début de MOT
- `contains("tex")` : partition 0x01, pas de filtre → match substring

---

## 14. Scoring BM25

### Inchangé structurellement

- BM25 standard, correct cross-shard
- `ExportableStats` sérialisable pour distributed search
- Fuzzy : tiers par miss_count (`miss_penalty * 1000 + bm25`)

### Changement : TF par mot via term dict

- En v2, TF est par token (ordinal SFX)
- En v3, TF est par MOT (ordinal term dict)
- `token_to_word[TI] → WI` permet d'agréger les TF des chunks d'un même mot
- Le term dict stocke directement les stats par mot

---

## 15. Paramètres constants

| Paramètre | Valeur | Justification |
|-----------|:------:|---------------|
| MAX_TOKEN | 8 | 90%+ des mots de code ≤ 8 bytes avec sep |
| OVERLAP | 2 | Couvre trigrams (n-1=2) et bigrams (n-1=1) |
| MIN_SUFFIX_LEN | 1 | Inchangé (configurable via env var) |
| MAX_CHUNK_BYTES | 256 | Force-split pour tokens très longs (inchangé) |
| Partitions | 2 (0x00, 0x01) | Inchangé |
| MAX_CONTINUATION_DEPTH | 64 | Inchangé (regex) |
| sfx_version | 3 | Identifiant de format |

---

## 16. Migration v2 → v3

- **Reindex obligatoire** : les fichiers segment v3 ne sont pas compatibles v2
- **Pas de migration incrémentale** : reindex complet
- **Flag sfx_version** : permet la coexistence v2/v3 pendant la transition
- **API inchangée** : les paramètres de query (field, value, distance, anchor_start, exact_match, regex, strict_separators) restent identiques
- **Résultats identiques** : mêmes documents trouvés, mêmes scores (sauf amélioration du threshold fuzzy)
