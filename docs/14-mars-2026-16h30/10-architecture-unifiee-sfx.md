# Architecture unifiée : le .sfx comme index unique

Date : 14 mars 2026

Statut : design validé, prêt à implémenter

## Constat

Aujourd'hui, chaque champ texte génère trois index :
- `._raw` FST (TermDictionary) : exact, prefix, fuzzy, regex
- `._ngram` : trigrams pour contains (candidats rapides)
- `.sfx` : suffix FST pour contains v2 (preuve directe)

Le .sfx est un **superset** du ._raw FST. Les entrées SI=0 du .sfx sont
exactement les termes du ._raw. Donc le .sfx peut remplacer le ._raw FST
et éliminer le ._ngram entièrement.

## Architecture cible

```
AVANT (3 index par champ) :
  ._raw FST  →  TermInfoStore  →  posting lists
  ._ngram    →  trigram posting lists
  .sfx       →  OutputTable    →  TermInfoStore  →  posting lists

APRÈS (1 index par champ) :
  .sfx       →  OutputTable    →  TermInfoStore  →  posting lists
              + GapMap (séparateurs)
```

Le .sfx FST devient l'unique point d'entrée pour TOUTES les recherches
sur le champ.

## Pourquoi ça marche sans pénalité

### Prefix scan (startsWith)

```
._raw FST :  range ge("im") lt("in") → trouve "import"
.sfx FST  :  même range              → trouve "import" (SI=0)
```

Les suffixes "mport", "port" ne commencent PAS par "im" — ils sont ailleurs
dans l'arbre FST et ne sont jamais visités. Le scan est identique.

### Exact lookup

```
._raw FST :  get("import") → ordinal=5
.sfx FST  :  get("import") → decode → SI=0, ordinal=5
```

Même résultat. Pour 95% des termes (single-parent SI=0), le décodage est
direct dans le u64 (pas d'OutputTable). Le coût est identique.

### Cas multi-parent

"import" est aussi suffix de "autreimport" → multi-parent dans l'OutputTable.

```
get("import") → OutputTable → [(ord=3, SI=0), (ord=7, SI=5)]
```

Pour exact/prefix : on filtre SI=0, on ignore le reste.
Coût du filtrage : itérer 2-5 entries de quelques bytes en mémoire.
Nanosecondes. Invisible comparé au coût du scan FST ou de la posting list.

## Optimisation : SI=0 en premier dans l'OutputTable

Trier les entries par SI croissant dans `encode_parent_entries` :

```
Avant :  [(ord=7, SI=5), (ord=3, SI=0), (ord=12, SI=3)]
Après :  [(ord=3, SI=0), (ord=7, SI=3), (ord=12, SI=5)]
              ↑ early exit pour exact/prefix
```

Pour un exact/prefix lookup : lire la première entry. SI=0 → on s'arrête.
Pour un contains : lire tout le tableau (on veut tous les SI).

Implémentation : une ligne dans `encode_parent_entries()` — `parents.sort_by_key(|p| p.si)`.

## Toutes les query types sur le .sfx unifié

| Query | Walk | SI filter | Source séparateurs |
|-------|------|-----------|-------------------|
| **exact** | `get(term)` | SI=0 only | — |
| **prefix/startsWith** | `range(ge, lt)` | SI=0 only | — |
| **fuzzy** | `search(Levenshtein DFA)` | SI=0 only | — |
| **contains** | `prefix_walk(query)` | any SI | GapMap |
| **contains fuzzy** | `fuzzy_walk(query, d)` | any SI | GapMap + fuzzy sep |
| **regex** | Walk DFA (voir ci-dessous) | selon contexte | GapMap |

## Regex : trois modes sur le .sfx unifié

### Le problème

Un regex peut traverser plusieurs tokens et séparateurs :
```
regex: "imp[a-z]+\s+rag3db.*core"
texte: "import rag3db_core"
         ^^^^^^ ^^^^^^^^^
         token1  token2 + sep + token3
```

### Principe commun : continuation d'état DFA

L'état DFA est un `u32`. Après le walk FST sur le premier token, on a
l'état final. On feed les bytes du séparateur (GapMap), puis les bytes
du token suivant, etc. **Jamais de restart du regex.**

```
Walk FST "import" → state_42
Feed GapMap " "   → state_43     ← pas de restart, continuation
Feed "rag3db"     → state_57     ← continuation
Feed GapMap "_"   → state_58
Feed "core"       → state_final  ← is_match? ✓
```

### Mode 1 : regex contains

Le premier token peut être un suffix (any SI). Les littéraux suivants
sont cherchés via contains (.sfx any SI) pour pré-filtrer les candidats.

```
1. PREMIER TOKEN — walk regex DFA sur .sfx, any SI
   Trouve tous les tokens (et leurs suffixes) qui matchent le début
   du regex. Ex: "imp[a-z]+" matche "import" (SI=0) et aussi
   "import" en tant que suffix de "autreimport" (SI=5).

2. LITTÉRAUX SUIVANTS — contains via .sfx (any SI)
   Extraire les littéraux du reste du regex : ["rag3db", "core"]
   Chercher chaque littéral via .sfx contains (prefix walk, any SI)
   → posting lists. Intersection des Ti consécutifs dans les mêmes docs.

3. VALIDATION — continuation DFA
   Pour chaque candidat (doc, Ti_start) :
   - Reprendre l'état DFA du premier token
   - Feed séparateur (GapMap) + tokens suivants (posting lists)
   - is_match(state_final) ? → match confirmé
```

### Mode 2 : regex startsWith

Le premier token doit matcher au début du token document (SI=0).
Les littéraux suivants sont toujours cherchés via contains (any SI).

```
1. PREMIER TOKEN — walk regex DFA sur .sfx, SI=0 only
   Ne trouve que les tokens complets qui commencent par le pattern.
   Ex: "imp[a-z]+" matche "import" (SI=0) mais PAS "import"
   en tant que suffix de "autreimport".

2. LITTÉRAUX SUIVANTS — contains via .sfx (any SI)
   Même chose que mode 1 : les tokens suivants peuvent être
   des substrings des tokens du document.

3. VALIDATION — continuation DFA (identique au mode 1)
```

Pourquoi les littéraux suivants sont toujours en mode contains : le regex
après le premier token peut matcher des substrings. Ex: `start[a-z]+port`
doit trouver "port" comme suffix de "import" au deuxième token.

### Mode 3 : strict_regex

Pas d'extraction de littéraux, pas de pré-filtrage par intersection.
Pure continuation DFA du début à la fin. Le plus simple et le plus
correct, potentiellement plus rapide pour les regex complexes où
l'extraction de littéraux est pauvre.

```
1. PREMIER TOKEN — walk regex DFA sur .sfx (SI selon startsWith/contains)
   Collect les matches avec leur état DFA final.

2. POUR CHAQUE CANDIDAT — continuation directe
   Pour chaque (doc_id, Ti, dfa_state) du premier token :
     a. Lire le séparateur GapMap(doc, Ti, Ti+1)
     b. Feed chaque byte du séparateur dans le DFA
     c. Lire le token suivant (posting list Ti+1)
     d. Feed chaque byte du token dans le DFA
     e. Si can_match(state) == false → abandon (pruning)
     f. Répéter (a-e) pour les Ti suivants
     g. Si is_match(state) à n'importe quel point → match

   Pas d'extraction de littéraux. Pas d'intersection.
   Le DFA lui-même fait le pruning via can_match().
```

Le `can_match()` est crucial : dès que le DFA entre dans un état dead
(aucun suffixe ne peut plus matcher), on abandonne ce candidat. C'est
l'équivalent du pruning du walk FST, mais sur le flux token+séparateur.

### Comparaison des trois modes

| | regex contains | regex startsWith | strict_regex |
|---|---|---|---|
| Premier token | walk DFA any SI | walk DFA SI=0 | walk DFA (selon mode) |
| Tokens suivants | littéraux contains | littéraux contains | continuation DFA directe |
| Pré-filtrage | oui (intersection) | oui (intersection) | non (DFA pruning) |
| Avantage | moins de candidats | moins de candidats | pas d'extraction littéraux |
| Quand l'utiliser | regex avec bons littéraux | prefix + regex | regex pur, peu de littéraux |

### Pourquoi c'est optimal

- L'état DFA est un `u32`. Coût mémoire nul.
- Le walk DFA sur le premier token est le plus discriminant.
- La continuation ne fait que feeder des bytes — pas de recompilation regex.
- `can_match()` prune les candidats impossibles immédiatement.
- Zéro décompression LZ4 de stored text à aucun moment.
- Les données viennent de trois sources (FST, GapMap, posting lists)
  mais le DFA ne voit qu'un flux de bytes continu.

### Comparaison avec l'existant

```
AVANT :
  1. Extraire littéraux du regex
  2. Lookup trigrams ._ngram → candidats
  3. Pour chaque candidat : décompresser stored text (LZ4)
  4. Exécuter le regex sur le texte décompressé
  → Bottleneck : décompression LZ4 par candidat

APRÈS :
  1. Walk DFA sur .sfx (premier token)
  2. Pré-filtrage littéraux via .sfx (ou continuation directe)
  3. Validation : feed bytes (tokens + GapMap) dans le DFA
  → Bottleneck : walk DFA (même coût qu'un prefix scan)
  → Zéro stored text, zéro LZ4
```

## Gains attendus

### Espace disque/mémoire

- **Éliminé** : ._raw FST (~10-15% de la taille du segment)
- **Éliminé** : ._ngram posting lists (~20-30% de la taille du segment)
- **Conservé** : .sfx FST (plus gros que ._raw, mais remplace les deux)
- **Conservé** : TermInfoStore, posting lists, positions, offsets, GapMap
- **Estimation** : -15 à -25% de taille segment nette

### Performance recherche

| Query | Avant | Après | Gain |
|-------|-------|-------|------|
| exact | FST lookup | FST lookup + decode SI=0 | ~identique |
| prefix | FST range scan | FST range scan + decode SI=0 | ~identique |
| fuzzy | FST Levenshtein walk | FST Levenshtein walk + decode SI=0 | ~identique |
| contains d=0 | trigrams + LZ4 + verify | .sfx prefix walk | **10-500x** |
| contains fuzzy | trigrams + LZ4 + fuzzy verify | .sfx fuzzy walk | **10-500x** |
| regex | trigrams + LZ4 + regex | .sfx DFA walk + GapMap continuation | **10-100x** |

Les gains massifs sont sur contains et regex — les deux qui lisaient du
stored text. Les autres types sont inchangés en performance.

## Plan d'implémentation

### Phase U1 — Tri SI=0 en premier

Modifier `encode_parent_entries()` : trier par SI croissant.
Ajouter `decode_first_si0()` : retourne le premier SI=0 sans lire tout.
~10 lignes, zéro risque.

### Phase U2 — TermDictionary sur .sfx

Créer un `SfxTermDictionary` qui wrappe le .sfx FST et expose la même
API que `TermDictionary` :
- `get(key) → Option<TermInfo>` : lookup .sfx → filter SI=0 → TermInfoStore
- `term_ord(key) → Option<TermOrdinal>` : lookup .sfx → filter SI=0
- `term_info_from_ord(ord) → TermInfo` : direct TermInfoStore
- `stream() / search(automaton)` : walk .sfx → filter SI=0

Le SegmentReader utilise `SfxTermDictionary` si le .sfx existe,
sinon fallback sur le `TermDictionary` standard.

### Phase U3 — Regex avec continuation DFA

Implémenter le walk regex premier token + continuation état :
- `SfxRegexWalker` : walk DFA sur .sfx, capture l'état final par match
- `continue_dfa(state, bytes) → state` : feed séparateur/token bytes
- Intégrer dans `SuffixContainsQuery` pour le mode regex

### Phase U4 — Supprimer ._ngram

Une fois le .sfx validé pour tous les query types :
- Retirer la génération du champ ._ngram dans les bindings
- Retirer le `NgramContainsQuery`
- Retirer le `NgramFilter` tokenizer
- Gain : ~20-30% espace, moins de code à maintenir

### Phase U5 — Supprimer ._raw FST

Une fois le `SfxTermDictionary` validé :
- Le `TermDictionary` standard n'est plus nécessaire pour les champs texte
- Conserver le `TermInfoStore` et les posting lists (inchangés)
- Gain : ~10-15% espace supplémentaire

## Dépendances

```
U1 (tri SI=0)      ← aucune, peut être fait maintenant
  ↓
U2 (SfxTermDict)   ← U1
  ↓
U3 (regex DFA)     ← U2
  ↓
U4 (rm ._ngram)    ← U2 + validation benchmark
  ↓
U5 (rm ._raw FST)  ← U2 + validation benchmark
```

## Questions ouvertes

### Merger de segments

Le merger doit fusionner les .sfx de plusieurs segments. C'est PHASE-8.
Avec l'architecture unifiée, le .sfx devient critique — le merger doit
le supporter nativement (pas juste le reconstruire from scratch).

### Stored text

Le stored text (Store/LZ4) reste nécessaire pour :
- Retourner le document original à l'utilisateur
- Les snippets/extraits

Il n'est plus nécessaire pour la recherche (contains, regex).
On pourrait le rendre optionnel à terme (flag par champ).
