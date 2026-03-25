# Doc 01 — Rétrospective tokenization + multi-token search

Date : 25 mars 2026

## Contexte

Le RAW_TOKENIZER indexe le texte : `SimpleTokenizer → CamelCaseSplitFilter → LowerCaser`.
Le SFX stocke tous les suffixes de chaque token pour le substring search (contains/startsWith).
Le multi-token search (queries avec espaces) fait un walk SFX par token puis intersecte par position.

## Les cas problèmes rencontrés

### 1. "rag3weaver" ne trouve pas

**Cause** : `tokenize_query` utilisait `SimpleTokenizer + LowerCaser` (pas de CamelCaseSplit).
"rag3weaver" → 1 token "rag3weaver". Mais l'index a ["rag3", "weaver"]. Aucun token indexé ne contient "rag3weaver".

**Tentative** : ajouter CamelCaseSplitFilter dans `tokenize_query` (commit `70d993c`).
"rag3weaver" → ["rag3", "weaver"] → multi-token → match ✓

### 2. "rag3wea" ne trouve pas

**Cause** : backward merge dans CamelCaseSplit. "rag3wea" → ["rag3", "wea"] → backward merge "wea"(3<4) → ["rag3wea"] = 1 token.

**Tentative** : retirer le backward merge (commit `8dbb4eb`).
"rag3wea" → ["rag3", "wea"] → multi-token → match ✓

### 3. "ag3weaver" ne trouve pas

**Cause** : forward merge trop agressif. "ag3weaver" → ["ag", "3", "weaver"] → "ag"(2<4) merge → "ag3"(3<4) merge → "ag3weaver" = 1 token.

**Tentative** : MAX_MERGED_CHUNKS=2 (commit `9545324`).
"ag3weaver" → ["ag3", "weaver"] → match ✓

### 4. "ingleQuery" highlight montre "ddSingleQuery"

**Cause** : le multi-token highlight calculait le SI du premier token via un lookup approximatif d'ordinals qui matchait le mauvais parent.

**Fix** : résoudre les postings de chaque parent et vérifier (doc_id, token_index) exact (commit `cd2ea43`). ✓

### 5. "ingleQuery" ne trouve pas (après lowercase fix)

**Cause** : `build_contains_query` faisait `value.to_lowercase()` AVANT la tokenisation.
"ingleQuery" → "inglequery" → CamelCaseSplit ne détecte plus le boundary lower→Upper.

**Fix** : passer la casse originale, laisser `tokenize_query` lowerer après le split (commit `cd2ea43`). ✓

### 6. "gleQuery" ne trouve pas

**Cause** : "gle" = 3 chars < MIN_CHUNK_CHARS=4 → merge avec "Query" → 1 token "glequery".

**Tentative** : baisser MIN_CHUNK_CHARS à 3. Fonctionnel mais change l'indexation.

**Autre tentative** : tokenize_query sans merge (split brut via `find_boundaries`).
"gleQuery" → ["gle", "query"] → multi-token → match ✓
Mais ça casse "rag3db" (voir #7).

### 7. "rag3db" ne trouve pas (avec query sans merge)

**Cause** : query sans merge → ["rag", "3", "db"] = 3 tokens. Index = ["rag3", "db"] = 2 positions.
Le multi-token attend 3 positions consécutives mais l'index n'en a que 2.
"rag" et "3" sont dans le MÊME token indexé "rag3".

**Tentatives** :
- **Chain flexible** (same position matching) : accepter position+1 OU même position.
  Problème : non-first tokens avec SI=0 ne trouvent pas "3" dans "rag3" (SI=3).
- **use_si0 = false pour tous** : tous les tokens cherchent tous les SIs.
  Fonctionne mais rend "eQuery" très lent (voir #8).
- **prefix_walk pour tous** : au lieu de resolve_suffix exact.
  Fonctionne mais même problème de perf.
- **On-demand all-SI resolve** : fallback dans le chain building.
  Problème de logique (position du backward walk).

### 8. "eQuery" très lent (24s sur 5k docs)

**Cause** : "eQuery" → ["e", "query"]. Avec `use_si0 = false`, "e" fait un prefix_walk
sur TOUS les suffixes commençant par "e" dans le SFX = presque chaque mot de l'index.
Le walk FST lui-même est le bottleneck.

**Non résolu** — conflit fondamental entre :
- Avoir tous les SIs (nécessaire pour same-position matching)
- Avoir des walks rapides (nécessite SI=0 pour non-first)

## Résumé des tensions

| Besoin | Solution | Conflit |
|--------|----------|---------|
| Query tokens matchent quand CamelCaseSplit diffère | CamelCaseSplit dans query | Tokens query ≠ tokens index si merge différent |
| Query split plus fin que index | Chain flexible (same position) | Non-first tokens ont besoin de all-SI → walk lent |
| Tokens courts dans la query | Pivot-first filtre les postings | Mais le walk FST lui-même est lent pour tokens courts |
| Performance | SI=0 pour non-first, resolve_suffix exact | Incompatible avec same-position matching |

## Le dilemme central

Le CamelCaseSplit crée une **asymétrie** entre les tokens indexés et les tokens d'une query partielle :

- Index "addSingleQuery" → ["addSingle", "Query"] (merge "add"+"Single")
- Query "ingleQuery" → doit matcher, mais "ingle" n'est pas un token indexé, c'est un **suffixe** du token "addsingle"

Le SFX gère les suffixes intra-token. Le multi-token gère les séquences inter-token.
Le problème c'est quand la query **chevauche** les deux : un token query est à la fois
un suffixe d'un token indexé ET doit être suivi d'un token indexé adjacent.

## Options à explorer

### A. Retirer CamelCaseSplit complètement

- Pas de split → tokens entiers (longs)
- Le SFX gère les substrings naturellement
- Tokens plus longs = plus de suffixes SFX = index plus gros
- Plus de multi-token pour les queries camelCase (tout est single-token SFX)
- Simple, pas d'asymétrie
- **Question** : impact taille index + perf SFX sur longs tokens ?

### B. Garder CamelCaseSplit, même tokenizer query/index

- Le tokenizer query applique les mêmes règles de merge
- Certaines queries partielles ne matchent pas ("gleQuery" → 1 token)
- Pas de same-position matching nécessaire
- Simple, rapide, prévisible
- **Question** : acceptable de ne pas trouver "gleQuery" ?

### C. Garder CamelCaseSplit, query split maximal + chain flexible + on-demand SI

- Le plus complet fonctionnellement
- Complexe à implémenter correctement
- Risque de perf sur tokens courts (walk SFX massif)
- Le on-demand all-SI dans le chain building est dur à faire correctement
- **Question** : la complexité vaut-elle le gain ?

### D. Garder CamelCaseSplit, query split au dernier boundary seulement

- Toujours max 2 tokens : head + tail
- Head = tout avant le dernier boundary (SFX suffix match)
- Tail = tout après (prefix match sur dernier token)
- Si head < 3 chars → pas de split (single token)
- Simple, prévisible, rapide
- **Question** : couvre-t-il tous les cas ? "rag3db" → head="rag3", tail="db" → 2 tokens ✓.
  "gleQuery" → head="gle", tail="query" ✓. "ag3weaver" → dernier boundary = 3|weaver →
  head="ag3", tail="weaver" ✓. Mais "eQuery" → head="e" (1<3) → single token "equery" → pas trouvé.

### E. Hybride : essayer merged d'abord, fallback split si 0 résultats

- Première passe : tokenizer identique à l'index (rapide, exact)
- Si 0 résultats ET query a des boundaries : retry avec split maximal + chain flexible
- Correct et rapide pour le cas commun
- Le fallback est rare et peut être plus lent
- **Question** : le double-pass est-il acceptable en latence ?

### F. CamelCaseSplit dans l'index, PAS dans la query

- Index : ["addSingle", "Query"]
- Query : "ingleQuery" → 1 token "inglequery" → SFX cherche "inglequery" comme substring
- "inglequery" est un substring de... rien ! "addsingle" + "query" sont des tokens séparés.
- **Ne fonctionne pas** pour les queries cross-token.

## État actuel du code (non committé)

- Pivot-first multi-token ✓ (walk all → resolve pivot → filter others)
- strict_separators = false ✓
- Chain building : strict position (revert du flexible)
- use_si0 : `prefix_only || !is_first` (revert)
- Walk : resolve_suffix pour non-last, prefix_walk pour last (revert)
- tokenize_query : split maximal via find_boundaries (PAS encore reverté)
- CamelCaseSplit indexation : MAX_MERGED_CHUNKS=2, no backward merge

## À décider

1. Garder ou retirer CamelCaseSplit ?
2. Si garder : quelle stratégie query ? (B, C, D, ou E)
3. Impact perf/taille à mesurer avant de décider
