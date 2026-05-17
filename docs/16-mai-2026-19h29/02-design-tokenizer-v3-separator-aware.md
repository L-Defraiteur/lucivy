# Design : Tokenizer v3 — séparateurs dans le FST

**Date** : 16 mai 2026  
**Contexte** : le bottleneck fuzzy cross-token vient du fait que les séparateurs sont invisibles pour le SFX engine. On explore comment les réintégrer.

## Le problème fondamental

Aujourd'hui :
```
Texte : "pthread_mutex_lock"
Tokens : ["pthread", "mutex", "lock"]
Séparateurs : jetés, reconstruits via GapMap/SiblingTable
```

Conséquences :
- Fuzzy sur `"mutex_lock"` d=1 → concat en `"mutexlock"`, 16 FST walks, sibling DFS = 200ms+
- Le trigram `"x_l"` est un "boundary trigram" jeté car la frontière token est invisible
- Le falling walk doit traverser la sibling table pour chaque cross-token match

**Objectif** : rendre les séparateurs visibles dans le FST sans exploser l'index.

---

## Approche 1 — Trailing separators (max 3 chars)

Chaque token absorbe les caractères non-alphanumériques qui le suivent, jusqu'à 3.

```
"pthread_mutex_lock"  → ["pthread_", "mutex_", "lock"]
"Error::LucivyError"  → ["Error::", "LucivyError"]
"std::vector<int>"    → ["std::", "vector<", "int>"]    (< et > sont des séparateurs)
"a----b"              → ["a---", "-b"]                  (4ème séparateur déborde)
"my  variable"        → ["my  ", "variable"]            (2 espaces absorbés)
```

### Suffixes produits

Token `"mutex_"` (6 bytes) :
```
SI=0: mutex_    (partition 0x00 — début de token)
SI=1: utex_     (partition 0x01)
SI=2: tex_
SI=3: ex_
SI=4: x_
SI=5: _         (juste le séparateur trailing)
```

### Avantages
- Simple conceptuellement
- Tous les gaps deviennent 0 (contiguous) → plus besoin de GapMap pour validation
- Le trigram `"x_l"` se résout naturellement : suffix `"x_"` de `"mutex_"` à SI=4
- Fuzzy DFA traverse le `_` comme un byte normal

### Inconvénients
- Limite arbitraire de 3 : que faire avec `"---"` ou `"   "` (indentation) ?
- Le token `"mutex_"` ≠ `"mutex"` : une query `term("mutex")` ne match pas directement
- Besoin d'un `content_len` (ou `sep_offset`) pour distinguer contenu/séparateur dans chaque token

### Metadata nécessaire
Encoder `content_len` dans le output u64 : nombre de bytes de contenu (hors trailing sep).
Pour `"mutex_"` : content_len=5, token_len=6. Le `_` est trailing.

---

## Approche 2 — Max-length tokenizer pur

Pas de split sémantique. Le texte est chunké en tokens de taille fixe (ex: 48 bytes).

```
"pthread_mutex_lock_init" (23 bytes) → ["pthread_mutex_lock_init"]     (1 token)
"very long identifier name foo bar" → ["very long identifier name foo ", "bar"]  (split à 30)
```

### Avantages
- Le plus simple possible
- Pas de cross-token pour les queries courtes (<48 bytes)
- Fuzzy `"mutex_lock"` d=1 = 1 seul FST walk, toujours

### Inconvénients
- BM25 ruiné : chaque "token" est quasi-unique → TF=1, IDF≈max pour tous
- Un document contenant 3x `"mutex_lock"` dans des chunks différents aurait TF=3 pour 3 tokens DIFFÉRENTS
- `term("mutex")` impossible (le token c'est `"pthread_mutex_lock_init"`)
- L'index explose : un token de 48 bytes = 48 suffixes

### Verdict
Trop destructif pour le scoring. Pertinent seulement si on abandonne BM25 pour du pure presence/absence.

---

## Approche 3 — Trailing + maxlen fallback

Combiner les deux : split sur séparateurs avec trailing absorption (≤3 chars), ET un maxlen safety net (ex: 64 bytes).

```
"pthread_mutex_lock"           → ["pthread_", "mutex_", "lock"]           (trailing)
"getElementById"               → ["getElementById"]                       (pas de séparateur → 1 token, 14 bytes < 64)
"very_long_variable_name_here" → ["very_", "long_", "variable_", "name_", "here"]  (trailing)
"aaaa.....(200 chars)"         → ["aaaa...(64 bytes)", "...(64 bytes)", "...(reste)"]  (maxlen split)
```

### Avantages
- Best of both worlds : les séparateurs sont dans les tokens, et les tokens pathologiquement longs sont coupés
- BM25 préservé : les tokens ont une sémantique (un "mot" + ses séparateurs)

### Inconvénients
- maxlen split est arbitraire : coupe au milieu d'un mot si pas de séparateur
- CamelCase non géré : `"getElementById"` reste un seul gros token

### Verdict
Bon compromis. Mais on perd CamelCase.

---

## Approche 4 — "Word-aware chunks" avec SI reset

**L'idée** : un seul gros token contient tout (séparateurs inclus), mais le FST encode des **word boundaries** dans les SI pour savoir où commencent les "vrais mots".

```
Texte indexé : "pthread_mutex_lock"
Token unique : "pthread_mutex_lock" (18 bytes, tout inclus)

Suffixes avec word-SI :
  wSI=0, SI=0:  pthread_mutex_lock     ← début du mot "pthread"
  wSI=0, SI=1:  thread_mutex_lock
  ...
  wSI=0, SI=6:  _mutex_lock
  wSI=1, SI=7:  mutex_lock             ← début du mot "mutex" → RESET SI logique
  wSI=1, SI=8:  utex_lock
  ...
  wSI=1, SI=12: _lock
  wSI=2, SI=13: lock                   ← début du mot "lock" → RESET SI logique
  wSI=2, SI=14: ock
  wSI=2, SI=15: ck
  wSI=2, SI=16: k
```

### L'encodage

Dans le output u64, au lieu de juste `si`, on encode :
- `si` : position en bytes dans le token complet (0..token_len)
- `word_index` : numéro du mot logique (0, 1, 2, ...) — ou l'offset du dernier début de mot

Ou plus simplement : un **bit `is_word_start`** qui dit "ce suffix commence au début d'un mot" (= après un séparateur ou au début du token).

```
Partition 0x00 : SI=0, is_word_start=true  (début absolu)
Partition 0x01 : SI>0, is_word_start=false (milieu de mot)
Partition 0x01 : SI>0, is_word_start=true  (début d'un mot interne)
```

### Comment ça marche pour les queries

**contains("mutex")** :
- Walk le FST dans partition 0x01 (ou 0x00 pour startsWith)
- Trouve `"mutex_lock"` à SI=7, is_word_start=true
- Le suffix `"mutex_lock"` commence au début du mot "mutex" ✓

**contains("mutex_lock")** :
- Walk le FST dans partition 0x01
- Trouve `"mutex_lock"` à SI=7
- Le `_` et le `l` sont dans le FST, le DFA les traverse naturellement
- Pas de cross-token walk ! C'est un seul token !

**fuzzy("mutx_lck", d=2)** :
- 1 seul FST walk avec Levenshtein DFA
- Le DFA traverse `"mutex_lock"` en tolérant 2 edits
- ~0.1ms au lieu de ~200ms

**term("mutex")** (exact whole-word) :
- Walk le FST, trouve suffix `"mutex_lock"` à SI=7, is_word_start=true
- Mais `"mutex" != "mutex_lock"` → check que les bytes matchés correspondent exactement à un mot
- On sait que SI=7 est un word_start, et le prochain word_start est à SI=13 → content_len du mot "mutex" = 13-7 = 6 = `"mutex_"` (5 chars + 1 sep)
- Ou mieux : on encode `word_len` (longueur du mot sans trailing sep) = 5
- Query "mutex" = 5 bytes = word_len → exact match ✓

**startsWith("mutex")** :
- Walk partition 0x01 avec filtre is_word_start=true
- Trouve `"mutex_lock"` à SI=7, is_word_start=true, word_len=5
- `"mutex"` (5 bytes) ≤ word_len (5) → prefix match ✓

### Avantages
- **0 cross-token walks** : tout est dans un seul token
- **Fuzzy 50-100x plus rapide** : 1 FST walk au lieu de 16 + sibling DFS
- **Séparateurs dans le FST** : regex `"mutex.lock"` matche directement
- **BM25 préservé** : si on veut scorer par "mot", word_index sert de position logique
- **Pas de GapMap/SiblingTable** pour la plupart des cas
- **CamelCase gratuit** : si `"getElementById"` est un seul token, les suffixes `"Element"`, `"By"`, `"Id"` sont à SI>0. On peut marquer is_word_start=true aux transitions CamelCase
- **maxlen safety net** : si un token dépasse 128 bytes, on coupe — la sibling table gère le cross-chunk (rare)

### Inconvénients
- **Index plus gros** : `"pthread_mutex_lock"` = 18 suffixes au lieu de 7+5+4=16. Pas beaucoup plus (+12%).
- **Output u64 plus riche** : il faut encoder is_word_start + word_len (ou word_end_si)
- **TF/IDF différent** : un document avec `"pthread_mutex_lock"` a 1 token au lieu de 3. Le doc_freq de `"mutex"` change (on matche un suffix, pas un token).

### Encoding u64

```
Actuel :  [63:multi][55..40:token_len][39..24:si][23..0:ordinal]
          7 bits libres (56..62)

Proposition :
  bit 62 : is_word_start (1 = ce suffix commence au début d'un mot logique)
  bits 56..61 : word_content_len (6 bits, max 63 bytes — longueur du mot sans trailing sep)
```

Pour les entrées multi-parent, le OutputTable stocke la même info par parent.

---

## Approche 5 — Hybride : word-aware chunks + BM25 dual

Même tokenizer que l'approche 4, MAIS on maintient un **index BM25 classique en parallèle** basé sur les mots logiques (split sur séparateurs, comme aujourd'hui).

```
SFX index : tokens longs avec séparateurs, pour contains/fuzzy/regex
BM25 index : tokens courts (mots), pour term/phrase/scoring
```

Le SFX engine utilise les chunks longs. Le BM25 scorer utilise les mots courts. Quand une query `contains("mutex")` arrive :
1. SFX engine trouve les docs via le chunk long
2. BM25 scorer calcule le score via le mot court `"mutex"` dans l'index classique

### Avantages
- Scoring BM25 identique à aujourd'hui
- Fuzzy cross-token ultra rapide via SFX
- Aucun compromis

### Inconvénients
- Deux indexes à maintenir (taille disk ×1.5)
- Complexité code (deux chemins d'indexation)
- Merge doit sync les deux indexes

---

## Approche 6 — Tokens inchangés + cross-token suffixes pré-calculés

On garde le tokenizer actuel (split sur séparateurs), MAIS au moment de construire le SFX, on ajoute des **suffixes cross-token pré-calculés** dans la même partition 0x01 :

```
Tokens : ["mutex", "lock"]  (comme aujourd'hui)

SFX partition 0x01 (suffixes normaux) :
  "utex"  → ord=0, si=1, token_len=5
  "tex"   → ord=0, si=2, token_len=5
  ...

SFX partition 0x01 (suffixes cross-token AJOUTÉS) :
  "utex_lock"  → ord=0, si=1, token_len=5, is_cross=true, total_len=10
  "tex_lock"   → ord=0, si=2, token_len=5, is_cross=true, total_len=10
  "ex_lock"    → ...
  "x_lock"     → ...
  "_lock"      → ord=0, si=5, is_cross=true   (commence au séparateur)
```

C'est l'approche initiale du rapport 01 mais dans la même partition.

### Avantage : backward compatible
- Les tokens individuels restent, BM25 scoring inchangé
- Les cross-token suffixes accélèrent le fuzzy sans changer l'archi

### Inconvénient : index size
- Chaque paire de tokens adjacents ajoute `len(A)` suffixes supplémentaires
- Pour un doc de 100 tokens de 6 bytes chacun : +600 entrées FST

---

## Comparaison

| Critère | 1 (trailing) | 2 (maxlen) | 3 (trailing+maxlen) | 4 (word-aware) | 5 (dual index) | 6 (cross-suffix) |
|---------|:---:|:---:|:---:|:---:|:---:|:---:|
| Fuzzy speed | ×5 | ×50 | ×5-50 | **×50-100** | **×50-100** | **×20-50** |
| BM25 correct | ~ok | broken | ~ok | needs work | **perfect** | **perfect** |
| Index size | ×1.05 | ×1.2 | ×1.05 | **×1.1** | ×1.5 | ×1.3-1.5 |
| Impl complexity | low | low | low | **medium** | high | medium |
| CamelCase | lost | lost | lost | **free** | free | needs sibling |
| Regex cross-token | good | perfect | good | **perfect** | **perfect** | good |
| No GapMap needed | yes | yes | yes | **yes** | no | no |
| No SiblingTable | mostly | yes | mostly | **mostly** | no | no |
| Backward compat | break | break | break | **break** | compat | **compat** |

## Recommandation

**Approche 4 (word-aware chunks)** est la plus élégante :
- Un seul token `"pthread_mutex_lock"` avec des marqueurs word_start aux positions 0, 8, 14
- Le SFX engine est considérablement simplifié (plus de cross-token DFS)
- CamelCase "gratuit" (marquer les transitions upper→lower comme word_start)
- Le coût en index size est modeste (+10-15%)
- Le speedup fuzzy est maximal (×50-100)

**Si backward compat est prioritaire**, approche 6 (cross-suffix pré-calculés) est le choix safe.

**Si on veut BM25 parfait sans compromis**, approche 5 (dual index) au prix de la complexité.

## Questions ouvertes

1. **Profondeur cross-token** : approche 4 couvre tout un "chunk" (max 128 bytes). Si le texte dépasse, on a quand même un cross-chunk rare qui nécessite sibling. Est-ce acceptable ?

2. **BM25 word-level** : dans l'approche 4, on pourrait calculer le BM25 score au niveau du mot (word_index) plutôt qu'au niveau du token (chunk). Faut prototyper pour voir si le scoring reste bon.

3. **Reindex** : les approches 1-4 changent le format d'index. Migration nécessaire (reindex complet). Les snapshots .luce existants deviennent incompatibles.

4. **SFX size estimation** : pour le dataset linux 90K docs, estimer la taille SFX avec chaque approche. Chiffres nécessaires avant de décider.
