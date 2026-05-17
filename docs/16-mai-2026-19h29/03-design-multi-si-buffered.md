# Design : Multi-SI buffered vs Extended token

**Date** : 16 mai 2026

Deux visions pour intégrer les séparateurs dans le SFX. Les noter clairement pour comparer.

---

## Vision A — "Extended token" (ma proposition)

Chaque token absorbe ses trailing separators dans le SFX (pas dans le tokenizer BM25).

```
Texte : "rag3db_value_count"
Tokens BM25 : ["rag3db", "value", "count"]     ← inchangés pour le scoring
Tokens SFX  : ["rag3db_", "value_", "count"]    ← étendus avec trailing sep

Suffixes de "rag3db_" dans le FST :
  SI=0: rag3db_     (7 bytes)
  SI=1: ag3db_
  SI=2: g3db_
  SI=3: 3db_
  SI=4: db_
  SI=5: b_
  SI=6: _           (juste le sep)

Sibling link : "rag3db_"(ord=0) → "value_"(ord=1), gap=0
```

Le falling walk sur "db_va" :
1. Walk FST : "db_" matche suffix de "rag3db_" à SI=4
2. SI(4) + 3 bytes = 7 = token_len → SPLIT POINT
3. Sibling → "value_" (ord=1), gap=0
4. Walk "va" sur "value_" à SI=0 → match

**Problème** : pour chercher "db_value_co" (3 tokens), le falling walk doit :
1. Match "db_" dans "rag3db_" → split → sibling → "value_"
2. Match "value_" complet dans "value_" → split → sibling → "count"
3. Match "co" dans "count"
→ 2 traversées de sibling table, toujours du DFS (mais avec gap=0 c'est plus simple)

**Limite** : on ne gagne que le premier cross-token en un seul FST walk. Les 2ème et 3ème cross-token passent toujours par sibling.

---

## Vision B — "Multi-SI buffered" (ta proposition)

Chaque entrée dans le FST porte plusieurs niveaux de SI :
- **SI_T** : position dans le token courant (comme aujourd'hui)
- **SI_W** : position depuis le début du "mot étendu" (token + trailing sep)
- Et conceptuellement : la possibilité de savoir que le suffix s'étend sur N tokens en arrière

### L'idée clé : le SFX indexe les suffixes qui TRAVERSENT les frontières

Pour le texte "rag3db_value_count" avec tokens ["rag3db", "value", "count"] :

```
Suffixes normaux de "rag3db" (comme aujourd'hui) :
  SI_T=0: rag3db        ord=0
  SI_T=1: ag3db         ord=0
  ...
  SI_T=5: b             ord=0

Suffixes normaux de "value" :
  SI_T=0: value         ord=1
  ...

Suffixes normaux de "count" :
  SI_T=0: count         ord=2
  ...

NOUVEAUX suffixes cross-token (profondeur 1 — traverse 1 séparateur) :
  "rag3db_value"   → ord=0, SI_T=0, depth=1, sep_at=6, next_ord=1
  "ag3db_value"    → ord=0, SI_T=1, depth=1, sep_at=5, next_ord=1
  "g3db_value"     → ord=0, SI_T=2, depth=1, ...
  "3db_value"      → ord=0, SI_T=3, depth=1, ...
  "db_value"       → ord=0, SI_T=4, depth=1, ...
  "b_value"        → ord=0, SI_T=5, depth=1, ...
  "_value"         → ord=0, SI_T=6 (= sep start), depth=1, ...
  
  "value_count"    → ord=1, SI_T=0, depth=1, sep_at=5, next_ord=2
  "alue_count"     → ord=1, SI_T=1, depth=1, ...
  ...

NOUVEAUX suffixes cross-token (profondeur 2 — traverse 2 séparateurs) :
  "rag3db_value_count"  → ord=0, SI_T=0, depth=2
  "ag3db_value_count"   → ord=0, SI_T=1, depth=2
  ...
  "db_value_count"      → ord=0, SI_T=4, depth=2
  ...
  "_value_count"        → ord=0, SI_T=6, depth=2
```

### Le tri-SI buffering

Pour chaque entrée cross-token, on encode 3 niveaux de SI :

```
"db_value_co" à depth=2 :
  SI_0 = 4   (position dans "rag3db" — le token d'origine)
  SI_1 = 11  (position depuis le début de "rag3db_value_" — inclut 1er cross)
  SI_2 = 17  (position depuis le début de "rag3db_value_count" — inclut 2ème cross)
  
  Ou autrement :
  token_0_ord = 0 ("rag3db"), offset_in_token_0 = 4 ("db...")
  token_1_ord = 1 ("value"), fully_covered = true
  token_2_ord = 2 ("count"), match_len_in_token_2 = 2 ("co")
```

### Avantage majeur

Chercher "db_value_co" fuzzy d=2 :
1. UN SEUL FST walk avec Levenshtein DFA
2. Le DFA traverse "db_value_co" en continu — séparateurs inclus
3. Pas de sibling table, pas de falling walk iteratif
4. Le résultat donne directement : tokens [0, 1, 2], positions, match ranges

C'est **O(1) FST walk** quelque soit le nombre de tokens traversés (jusqu'à depth max).

### Inconvénient

**Explosion de l'index** :
- Pour N tokens de taille moyenne L :
  - Suffixes normaux : N × L entrées (comme aujourd'hui)
  - Cross depth=1 : (N-1) × L entrées supplémentaires
  - Cross depth=2 : (N-2) × L entrées supplémentaires
  - Total : ~3NL entrées au lieu de NL → **index ×3**

Avec un max depth=2 (tri-token), c'est ×3. Avec depth=1 (bi-token), c'est ×2.

### Encoding

Le u64 actuel ne suffit pas pour 3 SI + 3 ordinals. Options :
1. **OutputTable** systématique pour les cross-token entries (multi_flag=1, pointer vers table)
2. **Compact encoding** : SI_0 + depth + premier ordinal inline, le reste dans OutputTable
3. **Nouveau format output** : augmenter à u128 (mais FST ne le supporte pas nativement)

---

## Comparaison directe

```
Query : "db_value_co" fuzzy d=2

Vision A (extended token) :
  1. Trigrams sur "db_value_co" (11 chars) → 9 trigrams
  2. Chaque trigram : FST walk + falling walk
  3. "db_" matche "rag3db_" SI=4 → split → sibling → "value_"
  4. "value_" matche complet → split → sibling → "count"  
  5. "co" matche "count" SI=0
  → 3 falling walks séquentiels + 2 sibling lookups par trigram
  → Mieux qu'aujourd'hui (gap=0, plus simple) mais toujours itératif

Vision B (multi-SI buffered, depth=2) :
  1. 1 FST walk avec Levenshtein DFA sur "db_value_co"
  2. Match l'entrée "db_value_count" à depth=2, SI_0=4
  3. Done.
  → 0 sibling lookups, 0 falling walks itératifs
```

| Critère | Vision A | Vision B (depth=2) |
|---------|:--------:|:------------------:|
| Fuzzy cross-token speed | ×5-10 vs actuel | **×50-100 vs actuel** |
| Index size | ×1.05 | **×2-3** |
| Impl complexity | Low | Medium-High |
| BM25 compat | Perfect | Perfect (tokens BM25 inchangés) |
| Max cross-token depth | Unlimited (itératif) | Fixed (depth=2) |
| Sibling table needed | Oui (pour depth>0) | Non (pour depth ≤ max) |
| GapMap needed | Non (gap=0) | Non |

---

## Vision C — Hybride pragmatique

Combiner les deux : extended tokens (Vision A) + cross-token entries pré-calculées à depth=1 seulement.

```
Token SFX : "rag3db_" (extended, 7 bytes)
  Suffixes normaux : rag3db_, ag3db_, g3db_, 3db_, db_, b_, _

Cross-token depth=1 : "rag3db_value_" (13 bytes)
  Suffixes ajoutés : rag3db_value_, ag3db_value_, ..., db_value_, b_value_, _value_
  → Avec : ord=0, SI=position, cross_ord=1, sep_at=6
```

- Depth=1 couvre 80%+ des queries fuzzy (2 mots)
- Pour 3+ mots, fallback sur falling walk itératif (rapide car gap=0)
- Index ×1.5-2 (pas ×3)
- Encoding : 1 cross_ord + sep_at dans les bits libres ou OutputTable

---

## Ce que Lucie propose en plus

### "On enregistre 3 SI"

L'idée de 3 SI par entrée :
- **SI_token** : offset dans le token immédiat
- **SI_prev** : offset par rapport au début du token précédent (incluant son contenu + sep)
- **SI_prev2** : offset par rapport au token encore avant

Ça permet au falling walk de savoir instantanément :
- "Je suis à SI_token=4 dans token ord=1, mais aussi à SI_prev=11 depuis le début de ord=0"
- Pas besoin de recalculer les offsets cross-token à runtime

### Comment l'encoder

Option : dans l'OutputTable (pas inline u64) :
```
struct CrossTokenEntry {
    token_ord: u32,
    si_token: u16,       // offset in this token
    si_from_prev: u16,   // offset from start of previous token (includes sep)
    si_from_prev2: u16,  // offset from start of token-2 (includes both seps)
    prev_ord: u32,
    prev2_ord: u32,
    token_content_len: u16,   // sans trailing sep
    token_extended_len: u16,  // avec trailing sep
}
```

Pour les entrées single-token (pas cross), SI_prev et SI_prev2 sont absents (ou 0xFFFF = N/A).

---

## Recommandation finale

**Vision C (hybride)** semble le meilleur compromis :

1. **Extended tokens** (trailing sep) → tous les gaps deviennent 0, séparateurs dans le FST
2. **Cross-token depth=1 entries** → couvre la majorité des queries fuzzy cross-token en 1 FST walk
3. **Falling walk itératif** pour depth>1 → rapide car gap=0
4. **Dual SI** (SI_token + SI_from_prev) pour les cross-token entries → falling walk sait où il est

Index size : ×1.5-2 (acceptable).
Fuzzy speed : ×20-50 pour 2-token queries, ×5-10 pour 3+ tokens.
BM25 : parfaitement préservé.
Backward compat : reindex nécessaire, mais le format est une extension naturelle.



décision lucie:

VISION A MAIS:

en fait moi je verrais bien vision A, mais avec: taille max token plutot     
  qu'arbitraire camelCase, (genre 5), + nouveaux type de sibling               
  sibling_next_token (prochain token a cause de tokenizatin arbitraire ou      
  taille max), sibling nextWord (prochain vrai mot (jusqu'a un range de        
  separateurs + les tokens du range de séparateurs), sibling nextSeparator ?   
  si utile vraiment prochain range séparateurs rencontré