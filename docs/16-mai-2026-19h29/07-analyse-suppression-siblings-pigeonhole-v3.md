# Analyse : suppression sibling table, pigeonhole trigram, et points de vigilance v3

**Date** : 16 mai 2026  
**Contexte** : avant de prototyper v3, vérifier que les choix du doc 05 tiennent face à l'implémentation réelle (code v2 lu en détail).

---

## 1. Comment fonctionnent les siblings dans chaque pipeline (v2 actuel)

### 1.1 Fuzzy contains (`fuzzy_contains.rs`)

Pipeline complet :
```
concat_query("rag3db_value") → "rag3dbvalue"     ← strip séparateurs
boundary_positions("rag3db_value") → [6]          ← position du "_" dans le concat
generate_trigrams("rag3dbvalue", d) → 8 trigrams
boundary_trigram_indices → [5] (trigram "bva" à pos 5 chevauche boundary 6)

Threshold = ngrams - n*d - (n-1)*boundaries = 8 - 3*1 - 2*1 = 3
                                                      ↑ pigeonhole    ↑ boundary compensation

Pour chaque trigram :
  fst_candidates(sfx, "bva")              → ordinals single-token
  cross_token_falling_walk(sfx, "bva", 0) → chaînes cross-token via siblings
    falling_walk("bva") → split à 1 byte ("b" fin de "rag3db"), remainder "va"
    sibling DFS: contiguous_siblings(ord_rag3db) → [ord_value]
    "value".starts_with("va") → OUI → chain [ord_rag3db, ord_value]
```

**Rôle des siblings** : résoudre les trigrams boundary qui chevauchent les frontières de tokens. Sans siblings, ces trigrams sont perdus → threshold doit être abaissé → recall dégradé.

### 1.2 Exact contains cross-token (`suffix_contains.rs` + `literal_pipeline.rs`)

```
Query : "db_value"
falling_walk("db_value") → SplitCandidate(prefix_len=2, ord="rag3db", si=4)
  → si(4) + 2 = 6 = token_len → SPLIT
  → remainder = "value" (4 bytes, après concat implicite : "db" fin + "value" début)

Wait, non. En v2 le falling walk opère sur la query telle quelle MAIS les tokens
n'ont pas de séparateur. Donc:
  falling_walk("db_value") :
    Walk FST : "d" → ok, "b" → ok, "_" → PAS dans le token "rag3db" → break
    → Pas de split à 2, juste un match partiel.

En réalité, en v2, contains cross-token passe par concat aussi:
  concat_query("db_value") → "dbvalue"
  falling_walk("dbvalue") → split à "db" (si=4, 4+2=6=token_len)
  remainder "value"
  sibling DFS : contiguous_siblings(ord_rag3db) → [ord_value]
  "value" == "value" → exact match → chain [ord_rag3db, ord_value]
```

**Fallback sans siblings** (ligne 269-287 du code) :
```
prefix_walk_si0(remainder) → cherche dans le FST tous les tokens commençant par "value"
Pour chaque résultat avec si=0 : ajouter chain [ord_first, ord_found]
```

Ce fallback existe déjà mais est moins efficace : il scanne le FST au lieu de consulter une lookup table.

### 1.3 Regex continuation (`regex_continuation_query.rs`)

```
Walk 1 : DFA × FST → ContinuationMatch{ordinal, si, end_state, is_accepting}
  → Chaque match est un bout de token qui satisfait le DFA
  → Si is_accepting : match complet, terminé
  → Sinon : DFA alive mais pas encore accepté → besoin de continuer

Walk 2 (sibling-accelerated) :
  Pour chaque candidat(doc_id, position, dfa_state) :
    gap_bytes = gapmap.read_separator(doc_id, pos, pos+1)
    Pour chaque sibling de cur_ordinal :
      next_text = ord_to_term(sibling.next_ordinal)
      Feed gap_bytes au DFA → si dead, skip
      Feed next_text au DFA → si dead, skip
      Vérifier position adjacency : next_ordinal à pos+1 dans ce doc ?
      Si DFA accepting → emit match
      Si DFA alive → push pour depth+1
```

**Rôle des siblings** : énumérer les tokens qui PEUVENT suivre, pour alimenter le DFA byte par byte. Sans siblings, le fallback `continuation_score` fait un FST DFA walk complet (plus coûteux mais fonctionnel).

**Rôle du gapmap** : fournir les bytes séparateurs réels entre deux tokens dans un document donné (car en v2 ils ne sont pas dans les tokens).

---

## 2. Le pigeonhole trigram — mécanisme détaillé

### 2.1 Principe

Pour une query avec d erreurs Levenshtein, au plus `n × d` trigrams (n=taille du ngram) peuvent être détruits par les d édits. Donc sur T trigrams, au moins `T - n*d` doivent matcher.

### 2.2 Correction boundary (v2)

En v2, `concat_query` fusionne les mots (strip séparateurs). Les trigrams qui chevauchaient une frontière de mot dans la query originale deviennent des trigrams "normaux" dans le concat, MAIS ils ne matchent rien dans le FST single-token car le FST ne contient pas ces concaténations.

Solution v2 : `cross_token_falling_walk` + sibling DFS pour résoudre ces trigrams. Mais c'est cher (~1-50ms par trigram boundary).

Compensation dans le threshold :
```
broken_by_boundaries = (n-1) × num_word_boundaries
threshold = T - n*d - broken_by_boundaries
```

Avec "rag3db_value" (1 boundary) et d=1, n=3 :
- 8 trigrams, 2 boundary trigrams (ceux chevauchant la position 6 dans le concat)
- threshold = 8 - 3 - 2 = 3 (au lieu de 5 sans compensation)

### 2.3 Ce que v3 change

Avec overlap=2, TOUS les trigrams cross-boundary sont dans le FST :
```
Token "rag3db_va" (rag3db + sep "_" + overlap "va")
Suffixes incluent : "b_v" (STI=5), "_va" (STI=6)
→ Le trigram "b_v" qui chevauchait la frontière est maintenant single-token ✓
```

**Impact sur le threshold** :
```
v2 : threshold = T - n*d - (n-1)*boundaries
v3 : threshold = T - n*d                        ← plus simple, plus strict
```

Plus strict = moins de faux positifs = moins de postings à résoudre = plus rapide.

### 2.4 Gain concret

```
Query : "mutex_lock" fuzzy d=1

v2 :
  concat → "mutexlock" (9 bytes)
  trigrams = ["mut","ute","tex","exl","xlo","loc","ock"] (7 grams)
  boundary = ["exl","xlo"] (2 boundary grams)
  threshold = 7 - 3 - 2 = 2
  Résolution : 7 FST walks + 2 cross_token_falling_walk + sibling DFS
  ~100-200ms

v3 :
  query telle quelle = "mutex_lock" (10 bytes, "_" inclus)
  trigrams = ["mut","ute","tex","ex_","x_l","_lo","loc","ock"] (8 grams)
  boundary = [] (0 ! l'overlap couvre tout)
  threshold = 8 - 3 = 5
  Résolution : 8 FST walks simples, ZERO cross-token
  ~2-5ms
```

---

## 3. Suppression des siblings — analyse par pipeline

### 3.1 Fuzzy contains : SUPPRESSION TOTALE

**Avant** : `cross_token_falling_walk` utilisé pour résoudre les boundary trigrams via sibling DFS.

**Après** : zéro boundary trigrams grâce à l'overlap. Chaque trigram est single-token. La fonction `cross_token_falling_walk` n'est PLUS appelée. On n'a besoin que de `fst_candidates` (FST walk simple).

**Code supprimé** :
- `concat_query()` — query utilisée telle quelle
- `boundary_positions()` — plus de boundaries
- `boundary_trigram_indices()` — plus de compensation
- `cross_token_falling_walk()` appelé dans la boucle trigram — supprimé
- `resolve_cross_with_parts()` — plus de chains à résoudre

**Code simplifié** :
- Threshold : `max(T - n*d, 1)` au lieu de `max(T - n*d - (n-1)*boundaries, min_threshold)`
- Résolution : seulement `fst_candidates` + `resolve_candidates` (single-token)

**Verdict** : aucune perte. Gain massif.

### 3.2 Exact contains cross-token : REMPLACEMENT PAR FALLING WALK CHAÎNÉ

**Avant** : `falling_walk` → split → sibling DFS (contiguous_siblings) → chain → posting adjacency check.

**Après** : `falling_walk` → split → continuation dans l'overlap → nouveau `falling_walk` sur le reste → posting adjacency check (position == TI_attendu + 1).

Mécanisme détaillé v3 :
```
Query : "db_value_co"

1. falling_walk("db_value_co") dans le FST :
   Walk dans le token "rag3db_va" (own_len=8, overlap=2) :
     d-b-_-v-a → 5 bytes consommés, STI=4
     STI(4) + 5 = 9 > own_len(8)
     Split à byte own_len - STI = 8 - 4 = 4 → prefix_len = 4
     (bytes 0..3 = "db_v" dans ce token, bytes 4..4 = "a" dans l'overlap)

   Attends — recalculons. Le falling walk émet un SplitCandidate quand
   STI + bytes_consumed == own_len. Ici :
     Après "d" : STI(4)+1=5 < 8 → continue
     Après "b" : STI(4)+2=6 < 8 → continue
     Après "_" : STI(4)+3=7 < 8 → continue
     Après "v" : STI(4)+4=8 = own_len → SPLIT POINT, prefix_len=4
     
   Le walk continue dans l'overlap zone :
     Après "a" : STI(4)+5=9 > own_len → overlap zone
     → 1 byte d'overlap validé ("a" = 1er byte du token suivant ✓)
   
   Puis "l" → pas de transition dans "rag3db_va" → break.
   Le walk est fini pour ce token. On a consommé 5 bytes ("db_va").

2. Remainder de la query : "lue_co" (bytes 5..10)
   Mais on sait que le prochain token commence par "va" (overlap validé).
   On fait un nouveau falling_walk("lue_co") en cherchant STI=2 du prochain token.
   
   Ou mieux : on fait falling_walk("value_co") (depuis le split point, pas depuis la
   fin de l'overlap) pour chercher un token qui COMMENCE par "value_co" ou dont un
   suffixe matche.
   
   falling_walk("value_co") dans "value_co" (token "value_co" avec own_len=7, overlap=2) :
     v-a-l-u-e-_-c → 7 bytes, STI(0)+7=7=own_len → SPLIT
     o → overlap zone, 1 byte validé
   
3. Remainder : "" → done. Match sur tokens [TI_rag3db, TI_value, TI_count].
   Vérification postings : pos(TI_value) == pos(TI_rag3db) + 1 ET pos(TI_count) == pos(TI_value) + 1.
```

**Le fallback `prefix_walk_si0` (ligne 269-287) remplace parfaitement le sibling DFS**. En v3, ce fallback est même MIEUX car :
- L'overlap a déjà validé 2 bytes du prochain token
- Le nouveau falling_walk démarre avec un prefix connu
- Pas de branching sur multiple siblings → un seul chemin déterministe par document

**Coût comparé** :
| Opération | v2 (sibling DFS) | v3 (falling walk chaîné) |
|-----------|:-:|:-:|
| Lookup sibling table | O(1) par ord | N/A |
| Énumérer siblings | ~5-20 candidats | N/A |
| Text lookup par sibling | O(1) × ~10 | N/A |
| Nouveau falling_walk | N/A | O(remainder_len) |
| Position adjacency check | O(postings) | O(postings) |
| **Total per split** | O(siblings × text_check) ~µs | O(remainder_len) ~µs |

Coûts similaires. Mais v3 est plus simple et déterministe.

### 3.3 Regex continuation : REMPLACEMENT PAR DFA ÉTENDU

**Avant** (`continuation_score_sibling`) :
```
Walk 1 : DFA × FST → candidates(dfa_state)
Walk 2 : Pour chaque candidat :
  gap = gapmap.read_separator(doc, pos, pos+1)
  feed gap_bytes au DFA
  pour chaque sibling :
    feed next_text au DFA
    vérifier position adjacency
```

**Après** (v3) :
```
Walk 1 : DFA × FST → candidates(dfa_state)
  Le token inclut trailing sep + overlap → le DFA consomme 
  PLUS de bytes dans Walk 1. Le sep est dans le token, l'overlap 
  donne 2 bytes du suivant.

Walk 2 : Pour chaque candidat avec DFA non-accepting :
  PAS de gap à feeder (gap = 0, sep dans le token)
  search_continuation(DFA, dfa_state, si_zero_only=true) → FST walk
  avec le DFA state sauvé → trouve le token suivant directement
```

**Le `continuation_score` (fallback actuel sans siblings) fait EXACTEMENT ça.** Il appelle `search_continuation` pour trouver les ordinals dont le texte fait avancer le DFA. C'est déjà implémenté.

**Gain en v3** :
- Le Walk 1 va plus loin (sep + overlap → plus de bytes consommés → plus de queries résolues en 1 walk)
- Le Walk 2 n'a plus besoin de feeder des gap_bytes (ils sont dans le token)
- Le Walk 2 utilise `search_continuation` (FST DFA walk) au lieu d'énumérer des siblings et feeder byte par byte

**Le gapmap disparaît** : les gap_bytes sont dans le token. Le DFA les traverse naturellement dans Walk 1.

### 3.4 Multi-token contains : PAS DE CHANGEMENT

Le code multi-token dans `suffix_contains_multi_token_impl` (lignes 527-763) n'utilise PAS directement la sibling table. Il résout chaque sous-token indépendamment, puis vérifie la position adjacency :

```rust
let found = per_token_postings[step].iter().find(|e| {
    e.doc_id == doc_id && e.token_index + e.span == expected_ti
});
```

C'est déjà du TI+1 implicite. Aucun changement nécessaire.

---

## 4. Points de vigilance pour v3

### 4.1 Falling walk : split point avec overlap

En v2 : `si + prefix_len == token_len` → split.
En v3 : `STI + bytes_consumed == own_len` → split.

Mais le walk CONTINUE après own_len dans l'overlap zone. Il faut :
1. Émettre le SplitCandidate au moment exact où STI + consumed == own_len
2. Continuer de walker les bytes d'overlap (pour valider les 2 premiers bytes du token suivant)
3. Quand le walk se termine (fin de l'overlap ou fin de la query), noter combien de bytes d'overlap ont été validés

```rust
// Pseudocode v3
for (i, &byte) in query_bytes.iter().enumerate() {
    let idx = node.find_input(byte)?;
    // ... advance node ...
    
    let consumed = i + 1;
    let pos_in_token = parent.sti as usize + consumed;
    
    if pos_in_token == parent.own_len {
        // SPLIT POINT — on est à la frontière du token
        emit SplitCandidate { prefix_len: consumed, ... }
        // Mais on NE break PAS — on continue dans l'overlap
    }
    
    if pos_in_token > parent.own_len + parent.overlap_len {
        break; // Fin de l'overlap, plus de bytes à valider
    }
}
```

### 4.2 Fuzzy falling walk : fst_depth et overlap

En v2, `fuzzy_falling_walk` utilise `fst_depth` comme split point : `si + fst_depth == token_len`.

En v3, il faut `STI + fst_depth == own_len`. Mais aussi : le DFA doit pouvoir continuer dans l'overlap zone. Le problème : le DFA walk est un DFS sur tout le FST, pas un walk linéaire. Les bytes d'overlap sont des bytes du FST comme les autres — le DFA les traverse naturellement.

Le split point émet un candidat, mais le DFS continue. Si le DFA traverse l'overlap et reste alive, c'est juste de la validation gratuite. Le candidat reste valid.

**Changement** : remplacer `parent.token_len` par `parent.own_len` dans la condition de split. C'est tout.

### 4.3 Overlap et tokens très courts

Si un token fait 1 byte de contenu + 1 sep + 2 overlap = 4 bytes dans le FST, c'est viable. Mais si le token suivant fait aussi 1 byte, on a un overlap qui COUVRE le token suivant en entier. C'est OK — le FST a un multi-parent entry pour ces bytes.

Cas extrême : token "a_" (content=1, sep=1) + overlap 2 bytes du suivant "bc" = "a_bc" (4 bytes). Le token suivant est "bc..." avec proprement ses suffixes. Les bytes "bc" apparaissent dans les deux tokens → multi-parent. Pas de problème fonctionnel, juste un peu plus d'entries multi-parent.

### 4.4 Le choix de MAX_TOKEN et la stratégie de chunking

**Décision** : division égale du segment (mot + trailing sep), pas de maxlen sur le contenu seul.

Le segment = mot + séparateurs trailing jusqu'au prochain mot. C'est l'unité de chunking. Si `segment.len() > MAX_TOKEN`, on divise en `ceil(len / MAX_TOKEN)` chunks de taille égale (± 1 byte pour le reste). Pas d'orphelins.

```
MAX_TOKEN = 8

"mutex_" (6 ≤ 8) → ["mutex_"]                              pas de split
"pthread_" (8 ≤ 8) → ["pthread_"]                          pas de split
"getElementById" (14 > 8) → ["getEleme", "ntById"]         (8, 6)
"mutex________" (13 > 8) → ["mutex__", "______"]           (7, 6)
"a________________" (17 > 8) → ["a_____", "______", "_____"]  (6, 6, 5)
```

Avec overlap=2 en plus, les tokens dans le FST font `chunk_size + 2` bytes max = 10 suffixes max. Acceptable.

**MAX_TOKEN=8** est le bon choix : 90%+ des mots de code (+ 1 sep) font ≤ 8 bytes → pas de split. Les longs identifiants se divisent proprement sans orphelins.

Le `sep_len` pour chaque chunk se déduit de la position dans le segment : seul le dernier chunk d'un segment contient des bytes de séparateur (puisque le sep est trailing). Les chunks intermédiaires sont du pur contenu alphanumérique.

### 4.5 next_word table — toujours nécessaire ?

**OUI**, pour ces cas :

1. **`term("pthread")`** : doit matcher le MOT entier "pthread" qui est chunké en ["pthre", "ad_"]. Le falling walk matche "pthre" dans un chunk, puis besoin de vérifier que les chunks suivants complètent le mot. `next_word` permet de sauter au prochain mot pour vérifier `exact_match`.

2. **`startsWith("get")`** : `is_word_start=true` filtre les chunks intermédiaires. Mais pour `startsWith("getElement")`, le match traverse les chunks "getEl" et "ement" — il faut savoir que "ement" est ENCORE dans le même mot.

3. **BM25 scoring** : la TF est par MOT, pas par token. `token_to_word[TI]` est nécessaire pour agréger les scores.

**Mais** : next_word n'est PAS nécessaire pour le cross-token falling walk (TI+1 suffit). C'est une table auxiliaire pour les requêtes sémantiques.

### 4.6 SWI (Suffix Word Index) — à reconsidérer ?

Le doc 04 propose un dual SI (STI + SWI). Après analyse, **STI seul suffit** avec le bit `is_word_start` :

- Le falling walk opère token par token (STI)
- `is_word_start` identifie les débuts de mot (pour startsWith)
- SWI serait utile uniquement pour savoir "combien de bytes depuis le début du mot" — mais cette info est dérivable de `word_start_token[WI]` + somme des content_len des tokens précédents dans le mot

**Conclusion** : garder STI, `is_word_start`, et la word_map. Pas besoin de SWI explicite dans l'output u64.

### 4.7 L'interaction overlap + regex DFA

En v3, le regex DFA traverse les bytes de séparateurs dans le FST (ils sont dans le token). C'est MIEUX que v2 où le DFA doit être alimenté manuellement avec les gap_bytes du gapmap.

Exemple : regex `"mutex.*lock"`
```
v2 : 
  Walk 1 : DFA matche "mutex" dans le FST → candidate, DFA state alive
  Walk 2 : feed gap "_" via gapmap → DFA state "mutex_", still alive (. matche _)
           feed "lock" via sibling → DFA accepting

v3 :
  Walk 1 : DFA matche "mutex_lo" dans le FST (token "mutex_lo" inclut sep + overlap)
           → DFA state "mutex_lo", still alive (. matche _ et l et o)
           → SPLIT au byte 6 (own_len)
  Walk 2 : search_continuation(DFA_state_after_overlap) → find "ck..." 
           → DFA accepting
```

Le Walk 1 en v3 avance de 8 bytes au lieu de 5 → le DFA est beaucoup plus avancé → le Walk 2 est plus court et plus sélectif.

### 4.8 strict_separators=false — dans le falling walk, pas en post-filtering

~~Le doc 05 conclut que c'est un post-FST filtering~~ → **CORRIGÉ** : le post-filtering ne suffit pas. Le walk lui-même échoue avant de produire des résultats quand les bytes de séparateurs diffèrent entre query et token. Il faut intégrer `strict_separators=false` **dans** le falling walk.

#### Le problème

En v3, les séparateurs sont des bytes normaux dans les tokens. Trois cas se présentent :

| Query | Texte | Sep query | Sep texte | Walk byte par byte |
|-------|-------|-----------|-----------|-------------------|
| `"mutex_lock"` | `mutex_lock` | `_` | `_` | **OK** — bytes identiques |
| `"mutex lock"` | `mutex_lock` | ` ` | `_` | **KO** — espace ≠ underscore, walk break |
| `"mutexlock"` | `mutex_lock` | aucun | `_` | **KO** — "l" ≠ "_", walk break |

Les cas 2 et 3 échouent car le walk compare byte par byte et les seps ne matchent pas. On ne peut pas filtrer après coup — le walk n'a rien produit.

#### La solution : sep-skip dans le falling walk

Le falling walk connaît `content_len` et `sep_len` depuis l'output u64. Quand on atteint la fin du contenu alphanumérique (STI + consumed == content_len), on sait qu'on entre dans la zone de trailing sep. Si `strict_separators=false`, on **saute** les bytes de sep dans le token ET dans la query :

```rust
// Pseudocode v3 falling walk avec strict_sep=false
for (i, &byte) in query_bytes.iter().enumerate() {
    let consumed = i + 1;
    let pos_in_token = parent.sti as usize + consumed;
    
    if pos_in_token == parent.content_len && !strict_separators {
        // On est à la frontière contenu/sep dans le TOKEN
        // Skip sep bytes dans le token (on connaît sep_len)
        // Skip sep bytes dans la query (avancer tant que !is_alphanumeric)
        // Reprendre le walk à l'overlap zone directement
        // → content_len + sep_len = début de l'overlap
    }
    
    // Walk normal : suivre le byte dans le FST
    let idx = node.find_input(byte)?;
    // ...
}
```

#### Exemples

**Cas "mutex lock" → "mutex_lock"** (sep différent) :
```
Walk : m-u-t-e-x → 5 bytes, STI(0)+5 = 5 = content_len
  strict_sep=false → skip sep token (1 byte "_"), skip sep query (1 byte " ")
  Reprendre dans l'overlap zone : "lo" → match ✓
  Split à own_len → TI+1 → walk "ck" → match ✓
```

**Cas "mutexlock" → "mutex_lock"** (pas de sep dans la query) :
```
Walk : m-u-t-e-x → 5 bytes = content_len
  strict_sep=false → skip sep token (1 byte "_"), skip sep query (0 bytes, "l" est alphanum)
  Reprendre dans l'overlap zone : "lo" → match ✓
  Split à own_len → TI+1 → walk "ck" → match ✓
```

**Cas "mutex__lock" → "mutex_lock"** (plus de seps dans la query que dans le texte) :
```
Walk : m-u-t-e-x → 5 bytes = content_len
  strict_sep=false → skip sep token (1 byte "_"), skip sep query (2 bytes "__")
  Reprendre dans l'overlap zone : "lo" → match ✓
  → Fonctionne aussi ✓
```

#### Impact sur le fuzzy

Avec `strict_sep=false`, le fuzzy n'a plus besoin de compter les seps comme des edits. Le falling walk les saute. Les trigrams sont générés depuis la query **telle quelle** (seps inclus ou non), et chaque trigram est résolu en single-token grâce à l'overlap.

Pour les trigrams qui tombent DANS la zone de sep de la query (ex: "x l" dans "mutex lock"), ils ne matcheront pas le FST (qui a "x_l"). Mais c'est au plus 1 trigram par boundary, et le threshold le tolère naturellement via `n*d`.

Si d=0, ces trigrams manquent → pas de match fuzzy. C'est cohérent : le sep-skip dans le falling walk gère le cas exact, et le fuzzy gère les variations.

#### Interaction avec l'overlap

Le sep-skip fonctionne AVEC l'overlap car :
1. On skip les bytes de sep (positions content_len..content_len+sep_len)
2. On reprend le walk dans l'overlap zone (positions content_len+sep_len..own_len)
3. L'overlap valide les 2 premiers bytes du token suivant → même mécanique qu'avant

Le FST a toujours les bytes de sep dans ses chemins. Le sep-skip ne change pas le FST — il change juste le **parcours** du falling walk (sauter des positions au lieu de les comparer byte par byte).

---

## 5. Récapitulatif : ce qui change réellement dans les index

### Changements confirmés (doc 05 validé)

| Composant | Action | Justification |
|-----------|--------|---------------|
| Sibling table | **SUPPRIMÉE** | Remplacée par TI+1 implicite + falling walk chaîné |
| GapMap | **SUPPRIMÉ** | Seps dans les tokens, gap=0 partout |
| SepMap | **SUPPRIMÉ** | Seps dans le FST, DFA les traverse naturellement |
| concat_query | **SUPPRIMÉ** | Query telle quelle, seps inclus |
| boundary_trigram_indices | **SUPPRIMÉ** | Plus de boundary trigrams |
| cross_token_falling_walk (pour fuzzy) | **SUPPRIMÉ** | Tous trigrams single-token |
| continuation_score_sibling | **SUPPRIMÉ** | Remplacé par continuation_score (DFA FST walk) |

### Ajouts confirmés

| Composant | Forme | Justification |
|-----------|-------|---------------|
| overlap (2 bytes) | Dans le SFX builder | Zéro boundary trigrams |
| is_word_start (1 bit) | Dans output u64 | startsWith, term filtering |
| own_len | Remplace token_len dans output u64 | Split point sans overlap |
| overlap_len (4 bits) | Dans output u64 | Reconstruire la zone d'overlap |
| sep_len (2-6 bits) | Dans output u64 | Savoir où finit le contenu |
| next_word table | `[u32; num_tokens]` | term(), startsWith() cross-chunk |
| **Term dict** | FST mots entiers → posting lists (`.terms`) | O(1) term/prefix/range, BM25 stats |
| **Sep dict** | FST séparateurs → posting lists (`.seps`) | Fast sep lookup pour regex |
| word_map | Dérivé du term dict : WI ↔ TI mapping | BM25 scoring, term exact match |

### Points à reconsidérer

| Point | Statut doc 05 | Recommandation après analyse |
|-------|---------------|------------------------------|
| SWI (dual SI) | Proposé dans doc 04 | **PAS NÉCESSAIRE** — STI + is_word_start + word_map suffisent |
| next_sep table | Abandonné dans doc 05 | **Confirmé abandonné** — pas de cas d'usage concret |
| strict_sep=false avec d=0 | ~~"Post-FST filtering"~~ | **CORRIGÉ** — sep-skip intégré dans le falling walk (sauter les bytes sep des deux côtés à content_len) |
| maxlen | "5" dans doc 04 | **MAX_TOKEN=8** — division égale du segment mot+sep, pas de maxlen fixe sur le contenu seul. 8 évite le chunking pour 90%+ des mots. Le sep fait partie du segment (pas d'orphelin de 1 byte). |
| Overlap variable | "Toujours 2" | **Confirmé fixe à 2** — suffisant pour trigrams et bigrams |
| Word content_len table | Proposé | **Nécessaire** pour term exact match cross-chunk |

---

## 6. Risques et cas limites à tester

### 6.1 Token identique avec et sans overlap

Le token "lock" sans overlap (dernier token) et "lock_in" avec overlap coexistent. Les suffixes "lo" de "lock" et "lo" de "lock_in" donnent des multi-parent entries. Le FST gère ça naturellement. **Pas de risque.**

### 6.2 Query plus longue que le token étendu

Query : "rag3db_value_count_init" (4 mots). Le falling walk fera 3 splits successifs. Chaque split → nouveau falling_walk sur le remainder. En v3, chaque falling walk est O(remainder_len). Total : O(query_len). **Même complexité que v2** (qui fait 3 sibling DFS).

### 6.3 Fuzzy avec d > nombre de seps

Query "mutxlck" d=3, texte "mutex_lock". En v3, les trigrams de "mutxlck" sont "mut","utx","txl","xlc","lck" (5 grams). Threshold = 5 - 3*3 = -4 → max(1) = 1. Un seul trigram suffit. "mut" existe dans "mutex_lo" → match. Le scoring tiendra avec miss_count élevé. **Fonctionne.**

### 6.4 Regex cross-token long

Pattern : `"pthread_mutex_lock_init_destroy"`. En v2, `continuation_score_sibling` fait 4 walks (1 initial + 3 continuations). En v3, `continuation_score` fait pareil via `search_continuation` répétée. Le DFA avance plus loin par walk grâce à l'overlap → possiblement **1 walk de moins**. **Pas de régression.**

### 6.5 strict_separators=false — tous les cas

| Query | Texte | strict_sep | d | Mécanisme v3 | Résultat |
|-------|-------|:---:|:---:|---|---|
| `"mutex_lock"` | `mutex_lock` | true | 0 | falling walk exact | **OK** |
| `"mutex_lock"` | `mutex_lock` | false | 0 | falling walk exact | **OK** |
| `"mutex lock"` | `mutex_lock` | false | 0 | sep-skip (espace→skip, `_`→skip) | **OK** |
| `"mutexlock"` | `mutex_lock` | false | 0 | sep-skip (0 bytes query→skip, `_`→skip) | **OK** |
| `"mutex__lock"` | `mutex_lock` | false | 0 | sep-skip (`__`→skip, `_`→skip) | **OK** |
| `"mutex lock"` | `mutex_lock` | true | 0 | walk break (espace ≠ `_`) | **KO attendu** |
| `"mutxlock"` | `mutex_lock` | false | 1 | fuzzy trigrams, sep-skip au falling walk | **OK** |

### 6.6 Tokens UTF-8 multi-byte

Token "café_" = 5 bytes contenu (c-a-f-é[2 bytes]) + 1 sep = 6 bytes. L'overlap prend les 2 premiers bytes du token suivant. Si le token suivant commence par un char UTF-8 de 2+ bytes, l'overlap peut couper au milieu d'un char. **Ce n'est pas un problème** car le FST opère sur des bytes, pas des chars. Le falling walk avance byte par byte. Le trigram est aussi en bytes.

---

## 7. Plan d'implémentation révisé

1. **MaxLenEqualChunkTokenizer** (`src/tokenizer/`)
   - Split sur non-alphanum → segments (mot + trailing sep = unité)
   - Division égale de chaque segment : `num_chunks = ceil(segment.len() / MAX_TOKEN)`
   - Taille des chunks : `base = segment.len() / num_chunks`, premiers chunks +1 pour le reste
   - Respect des frontières UTF-8
   - Pas d'orphelins : chunk min = `segment.len() / ceil(segment.len() / MAX_TOKEN)`
   - Le trailing sep fait partie du segment, chunké avec le contenu
   - Produit : tokens + metadata (content_len, sep_len, is_word_start, word_id)
   - Exemples (MAX_TOKEN=8) :
     - `"mutex_"` (6 ≤ 8) → `["mutex_"]` — 1 token
     - `"pthread_"` (8 ≤ 8) → `["pthread_"]` — 1 token
     - `"getElementById"` (14 > 8) → `["getEleme", "ntById"]` — (8, 6)
     - `"mutex________"` (13 > 8) → `["mutex__", "______"]` — (7, 6)
     - `"a________________"` (17 > 8) → `["a_____", "______", "_____"]` — (6, 6, 5)

2. **SFX builder** (`src/suffix_fst/builder.rs`)
   - Ajouter overlap : pour chaque token, étendre avec min(2, len(next_token)) bytes
   - Encoder own_len (pas total_len) dans le output u64
   - Encoder is_word_start, sep_len, overlap_len dans les bits libres
   - Supprimer la construction de la sibling table
   - Supprimer la construction du gapmap et du sepmap

3. **Falling walk** (`src/suffix_fst/file.rs`)
   - Split condition : `STI + consumed == own_len` (au lieu de `token_len`)
   - Continuer après split dans l'overlap zone (ne pas break)
   - Retourner overlap_validated_bytes dans le SplitCandidate
   - **Sep-skip** : quand `strict_separators=false` et `STI + consumed == content_len`, sauter les bytes de sep dans le token (sep_len connu) et dans la query (avancer tant que `!is_alphanumeric`), puis reprendre le walk dans l'overlap zone

4. **Fuzzy falling walk** (`src/suffix_fst/file.rs`)
   - Même ajustement : `STI + fst_depth == own_len`
   - Le DFS continue naturellement dans l'overlap

5. **Fuzzy contains** (`src/query/phrase_query/fuzzy_contains.rs`)
   - Supprimer concat_query, boundary_positions, boundary_trigram_indices
   - Threshold : `max(T - n*d, 1)`
   - Résolution : seulement fst_candidates + resolve_candidates
   - Supprimer appels à cross_token_falling_walk

6. **Exact contains cross-token** (`suffix_contains.rs` + `literal_pipeline.rs`)
   - Remplacer sibling DFS par falling walk chaîné (utiliser le fallback existant)
   - Vérification position adjacency inchangée

7. **Regex continuation** (`regex_continuation_query.rs`)
   - Supprimer `continuation_score_sibling`
   - Utiliser uniquement `continuation_score` (FST DFA walk)
   - Supprimer les appels au gapmap

8. **Term dict + Sep dict** (NOUVEAU — `src/suffix_fst/term_dict_v3.rs`)
   - FST de mots entiers → posting lists (fichier `.terms`)
   - FST de séparateurs → posting lists (fichier `.seps`)
   - Construit pendant l'indexation en parallèle du SFX builder
   - Le word_map est dérivé du term dict (WI = ordinal dans le term dict)
   - Routing des queries :
     - `term("mutex")` → term dict lookup direct O(1)
     - `startsWith("get")` → term dict prefix scan
     - `range("a".."z")` → term dict range scan
     - `contains("tex")` → SFX FST (inchangé)
     - `fuzzy("mutx", d=1)` → SFX trigrams (inchangé)
   - BM25 stats (TF, DF) stockées dans le term dict par mot

9. **Word map + next_word table**
   - Dérivé du term dict : word_ordinal (WI) = ordinal dans le term dict
   - `token_to_word[TI] → WI` : quel mot contient ce chunk
   - `word_start_token[WI] → TI` : premier chunk du mot
   - `next_word[TI] → TI` : prochain début de mot (pour skip cross-chunk)
   - Sérialisé dans le `.sfx` (section additionnelle)

10. **Cleanup**
    - Supprimer `sibling_table.rs`
    - Supprimer `gapmap.rs` (ou marquer deprecated pour compat lecture)
    - Supprimer `sepmap.rs`
    - Supprimer `concat_query` et fonctions boundary

---

## 8. Architecture fichiers segment v3

| Fichier | Contenu | Taille estimée | Queries servies |
|---------|---------|:-:|---|
| `.terms` | FST mots entiers + postings | ~10% | term, prefix, range, BM25 |
| `.seps` | FST séparateurs + postings | ~2% | regex sep lookup |
| `.sfx` | SFX FST suffixes + parent lists + next_word + word_map | ~55% | contains, fuzzy, regex |
| `.sfxpost` | SFX posting lists | ~25% | (résolution SFX) |
| `.termtexts` | Token texts pour reconstruction | ~8% | highlights, cross-token |

**Supprimés** : `.gapmap`, `.sepmap`, sibling table (dans `.sfx`).

**Nouveaux** : `.terms`, `.seps`.

**Taille totale estimée** : ×1.10-1.15 par rapport à v2 (on ajoute term/sep dict mais on supprime gapmap+sepmap+sibling).

---

## 9. Conclusion

L'analyse du code v2 **confirme** que la suppression des siblings est le bon choix. Chaque pipeline a un chemin de remplacement clair :

- **Fuzzy** : l'overlap élimine les boundary trigrams → cross_token_falling_walk inutile
- **Exact cross-token** : falling walk chaîné (fallback existant) → même efficacité, code plus simple
- **Regex** : continuation_score (fallback existant) + tokens plus longs → DFA avance plus loin par walk
- **Term/prefix/range** : term dict direct → O(1), plus rapide que le routing SFX actuel

Le `strict_separators=false` est géré par un sep-skip intégré dans le falling walk (pas du post-filtering).

L'overlap de 2 bytes est la clé de tout : il transforme le problème cross-token en problème single-token pour les trigrams, et réduit le coût des falling walks chaînés pour le reste.

Le term dict complète l'architecture en offrant un fast-path pour les queries simples, tout en permettant au SFX de se concentrer sur ce qu'il fait le mieux : le substring matching.
