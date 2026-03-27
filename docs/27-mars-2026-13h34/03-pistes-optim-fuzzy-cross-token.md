# Doc 03 — Pistes optimisation fuzzy cross-token

Date : 27 mars 2026

## Problème

`fuzzy_falling_walk` est correct mais lent (~10s sur 862 docs pour d=1).
Le DFA Levenshtein fait un DFS sur tout le SFX FST (100K+ suffixes).

## Pistes (à tester dans l'ordre)

### Piste 1 : Filtrer les candidats — garder le DFS, réduire les resolves

Le DFS explore tout le FST mais la plupart des candidats sont inutiles.
On ne garde que ceux qui :
- ont un sibling contiguë (cross-token viable) OU
- consomment toute la query (`fst_depth >= query.len() - d`, single-token match)

Le DFS reste le même, mais les resolve/adjacency ne sont faits que sur les
candidats utiles. Si le bottleneck est le resolve et pas le DFS, ça suffit.

**À tester d'abord — pas de conclusion hâtive sur ce qui est lent.**

### Piste 2 : Fuzzy walk sur le term dict au lieu du SFX

Le term dict (tokens uniques) est ~10× plus petit que le SFX (tous les suffixes).
Un Levenshtein DFA walk sur le term dict est donc ~10× plus rapide.

Pour le cross-token, le premier token est SI=0 → il est dans le term dict.
On fuzzy-walk le term dict pour trouver les tokens proches, puis sibling chain.

Limitation : ne couvre pas SI>0 (contains qui commence mid-token avec fuzzy).
Mais SI>0 fuzzy est un cas rare.

### Piste 3 : Fuzzy walk sur la partition SI=0 du SFX seulement

Comme piste 2 mais sans utiliser le term dict directement. Le SFX a une
partition \x00 (SI=0) qui ne contient que les tokens complets. Le DFA walk
sur cette partition est ~10× plus rapide que sur tout le SFX.

On peut faire : SI=0 en fuzzy + SI>0 en exact (falling_walk). Ça couvre
tous les cas sauf SI>0 fuzzy (rare).

### Piste 4 : Sibling-first (itération des tokens avec siblings)

Itérer les ~300 tokens qui ont des siblings, Levenshtein check CPU pur.
~30K ops. Très rapide mais ne couvre que SI=0 et les tokens avec siblings.

Limitation : ne trouve pas les tokens sans siblings (single-token fuzzy)
ni les starts mid-token (SI>0).

### Piste 5 : Early termination dans le DFS

Le DFS du fuzzy_falling_walk explore tout. On pourrait :
- Limiter le nombre de candidats trouvés (top N)
- Limiter la profondeur du DFS (fst_depth > query.len() + d → stop)
- Skip les branches qui ne mènent pas à des noeuds finals proches

Limitation : pourrait rater des candidats valides.

### Piste 6 : Trigrams virtuels via les SI du SFX

Idée de Lucie. Le SFX a déjà des suffixes à SI=0,1,2,... qui sont des
N-grams naturels. Au lieu de DFS avec un DFA, on extrait les trigrams de
la query et on les lookup dans le SFX (O(1) par lookup).

1. Extraire trigrams de la query : "rak" "ak3" "k3w" "3we" "wea" "eav" "ave" "ver"
2. Lookup chaque trigram dans le SFX → set d'ordinals par trigram
3. Intersecter les ordinals → candidats qui contiennent la plupart des trigrams
4. Vérifier Levenshtein seulement sur les candidats (peu nombreux)

C'est l'approche PostgreSQL (trigram filter + verify). Ultra rapide car
O(trigrams × log FST) au lieu de O(FST_size × DFA_states).

Pour d=1, on tolère 1 trigram manquant. Pour d=2, 2 trigrams manquants.
Les candidats faux positifs sont éliminés par le verify Levenshtein.

**Potentiellement la meilleure piste** : O(query_len) lookups au lieu de
O(FST_size) DFS. Et on a déjà les trigrams dans le SFX gratuitement.

## Plan

Piste 1 testée : le filtre sibling réduit 700→24 candidats. DFS = 2ms/segment.
Total natif = 20ms. En WASM ~100-200ms. À tester.

Si encore trop lent en WASM : piste 6 (trigrams) ou piste 3 (SI=0 only).
