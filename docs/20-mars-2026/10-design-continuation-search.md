# Doc 10 — Design : continuation cross-token pour contains/startsWith

Date : 20 mars 2026

## Le problème

Le CamelCaseSplitFilter split les mots ALL_CAPS et les tokens longs (>256 bytes).
Quand un query span une frontière de split, le search rate des docs.

Cas confirmés par bench 5K :
- `FUNCTION` → `FUNC` + `TION` → query "function" : DIFF=20 (fixé par expansion uppercase)
- `SCHEDULER` → `SCHE` + `DULER` → query "sched" : DIFF=4 (PAS fixé, splits non-alignés)
- `SCHEDULE` → `SCHE` + `DULE` → query "sched" : idem
- Tokens >256 bytes force-split → query spanning la frontière : idem

L'expansion uppercase ne suffit pas : elle ne marche que quand query et document ont le même split. Pour `SCHEDULER` vs query "sched", les splits sont différents.

## La solution : continuation DFA

Le mécanisme existe déjà dans `regex_continuation_query.rs`. Il :
1. Walk le suffix FST avec un automate (DFA)
2. Quand le DFA n'a pas fini mais qu'on atteint la fin d'un token → candidat
3. Lit le gap via GapMap, avance le DFA à travers le gap
4. Walk le token suivant (SI=0) avec l'état DFA post-gap
5. Boucle jusqu'à MAX_CONTINUATION_DEPTH

C'est exact et complet — gère N splits, n'importe quel alignement.

## Design proposé

### Option sur la query

```rust
SuffixContainsQuery::new(field, "sched".into())
    .with_continuation(true)  // active la continuation cross-token
```

Par défaut : OFF (pas de overhead pour les queries simples).
Quand ON : le search fait d'abord le walk normal, puis la continuation DFA pour les matches partiels.

### Trois niveaux de search

1. **Single-token exact** (actuel) : walk suffix FST, résoudre sfxpost. O(walk).
2. **Expansion uppercase** (ajouté aujourd'hui) : re-tokenise la query en MAJUSCULES, multi-token si split. Couvre les cas où query et doc ont le même split.
3. **Continuation DFA** (à ajouter) : automate qui traverse les frontières de tokens via GapMap. Couvre TOUS les cas.

L'option `with_continuation(true)` active le niveau 3.

### Pour multi-token aussi

Même avec `contains_split "struct device"` (2 tokens), si "struct" est dans un token splitté (ex: `RESTRUCTURE` → `REST` + `RUCT` + `URE`), le multi-token search ne trouvera pas "struct" dans ce token.

Avec continuation activée : chaque token de la query multi-token passe aussi par la continuation DFA. Ça garantit que "struct" est trouvé même s'il span une frontière.

### Le DFA pour contains

Pour contains "sched" :
- Le DFA accepte n'importe quel préfixe, puis "sched", puis n'importe quel suffixe
- C'est un DFA substring : `.*sched.*`
- La construction est triviale : N+1 états (un par char de la query + état acceptant), avec self-loops sur l'état initial

Pour startsWith "sched" :
- DFA : `sched.*`
- Plus simple : pas de préfixe arbitraire, match au début du token

### Optimisation : candidats rapides

Sans continuation : le walk normal donne des candidats directs.
Avec continuation : on a PLUS de candidats (walk normal + continuation).

L'intersection multi-token peut utiliser les candidats du walk normal comme pivot (le plus sélectif), et la continuation comme fallback pour les tokens qui ne matchent pas directement.

```
Stratégie :
1. Walk normal → candidats primaires (rapides, majority of matches)
2. Pour les docs NON trouvés par le walk normal mais présents pour d'autres tokens :
   → continuation DFA sur les tokens manquants
3. Union des résultats
```

Ça évite de lancer la continuation DFA sur tous les docs — seulement sur les cas edge.

### Fuzzy + continuation

La continuation DFA supporte déjà le fuzzy (Levenshtein DFA dans regex_continuation_query).
Avec `with_continuation(true)` + `with_fuzzy(1)` :
- Le DFA est un automate Levenshtein d=1 pour le substring
- La continuation traverse les gaps avec le même automate
- Couvre les fautes de frappe + splits de tokens

## Implémentation

### Étape 1 : extraire la continuation de regex_continuation_query

La logique de continuation dans `continuation_score()` est couplée au RegexContinuationQuery.
Extraire dans un module réutilisable : `continuation.rs`
- `fn continuation_search(automaton, sfx_dict, resolver, sfx_reader, mode) → (BitSet, highlights)`
- Utilisable par SuffixContainsQuery ET RegexContinuationQuery

### Étape 2 : ajouter l'option à SuffixContainsQuery

```rust
pub struct SuffixContainsQuery {
    // ...
    continuation: bool,
}

impl SuffixContainsQuery {
    pub fn with_continuation(mut self, enabled: bool) -> Self {
        self.continuation = enabled;
        self
    }
}
```

### Étape 3 : construire le DFA substring

Pour contains "sched" :
```rust
fn build_substring_dfa(query: &str) -> impl Automaton {
    // État 0: match n'importe quoi (self-loop) ou commence à matcher query
    // États 1..N: progression dans le query
    // État N: acceptant (self-loop pour suffixe arbitraire)
}
```

Ou utiliser un regex : `format!(".*{}.*", regex_escape(query))` → compiler en DFA.

### Étape 4 : brancher dans le scorer

Dans `SuffixContainsWeight::scorer()` :
```rust
if self.continuation {
    // Build substring DFA
    let dfa = build_substring_dfa(&self.query_text);
    // Use continuation_search
    let (bitset, highlights) = continuation_search(dfa, ...);
    // Merge with normal results
}
```

### Étape 5 : bench comparatif

Comparer sur bench 5K :
- Sans continuation : actuel (mutex=610, sched=420, lock=2454)
- Avec continuation : attendu (sched=424, lock=2455)
- Perf : temps de query avec/sans continuation

## Estimation

```
Étape 1 (extraire continuation)  : ~50 lignes déplacées
Étape 2 (option)                 : ~10 lignes
Étape 3 (DFA substring)         : ~30 lignes
Étape 4 (brancher)               : ~20 lignes
Étape 5 (bench)                  : bench existant
Total                            : ~110 lignes
```

## Après : fix CamelCaseSplitFilter

Une fois la continuation en place et validée, fixer le CamelCaseSplitFilter pour ne pas splitter les ALL_CAPS (c'est pas du camelCase). Ça réduit le nombre de cas où la continuation est nécessaire, mais la continuation reste pour les tokens >256 bytes et les edge cases.
