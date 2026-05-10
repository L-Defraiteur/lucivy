# Diagnostic : startsWith rate des docs vs ground truth

## Symptôme

```
contains DAG:      1952 vs gt   1952  MATCH
startsWith DAG:     780 vs gt    801  FAIL (< gt!)
```

Même index, mêmes segments, même terme. `contains` = résultat exact, `startsWith` rate ~3-27%.

## Cause racine : mismatch de tokenisation dans le ground truth

### Tokenizer de l'index

Le `raw_code` tokenizer (handle.rs:477) :
```
SimpleTokenizer → CamelCaseSplitFilter → LowerCaser
```

- `SimpleTokenizer` : split sur `!c.is_alphanumeric()` — **`_` est un séparateur**
- `CamelCaseSplitFilter` : split supplémentaire sur camelCase
- Donc `sched_setaffinity` → `["sched", "setaffinity"]`
- Et `printk_ratelimited` → `["printk", "ratelimited"]`

### Tokenizer du ground truth (bench_sharding.rs:667)

```rust
text.split(|c: char| !c.is_alphanumeric() && c != '_')
    .any(|tok| tok.starts_with(&prefix_lower))
```

**`_` est gardé dans les tokens** → `sched_setaffinity` reste UN token.
`"sched_setaffinity".starts_with("sched")` → true → comptabilisé.

### Le delta

Le ground truth compte des docs où le terme apparaît dans un token qui contient `_` :
- Ground truth : `sched_setaffinity` starts with `sched` → true (token entier)
- Index : tokens `["sched", "setaffinity"]` — `sched` est bien au SI=0, **mais le SFX startsWith cherche des tokens complets qui commencent par le préfixe**

Le problème c'est plus subtil : le moteur SFX en mode `anchor_start` fait un `prefix_walk_si0`, qui cherche dans la partition SI=0 du SFX. Ça retourne les docs qui ont un token **commençant par** le préfixe. `sched` est bien trouvé (c'est un token complet commençant par `sched`). Mais `sched_setaffinity` n'existe pas comme token dans l'index — il a été splitté en `sched` + `setaffinity`.

Donc le moteur devrait trouver `sched` comme match SI=0. Le delta vient d'ailleurs : le ground truth matche `sched_setaffinity` comme un seul token, mais le moteur ne voit pas ce token du tout — il voit `sched` et `setaffinity` séparément, et `sched` matche bien startsWith `sched`.

### Hypothèse révisée

La cause est probablement que certains docs ont des tokens compound (ex: `__sched`) qui sont splittés différemment par SimpleTokenizer vs le ground truth :
- Ground truth : `__sched` → split sur `!alphanum && != _` → token `__sched` → starts_with `sched` = **false** (commence par `_`)
- Index : `__sched` → SimpleTokenizer split sur `!alphanum` → `_` est séparateur → token `sched` → SI=0 match

Attendez — dans ce cas le moteur trouverait PLUS que le ground truth, pas moins. C'est l'inverse du symptôme.

### Scénario qui donne startsWith < ground truth

```
Token dans le texte : "scheduler_init"
Ground truth split : ["scheduler_init"] → starts_with("sched") = true ✓
Index tokenizer    : ["scheduler", "init"] → "scheduler" starts with "sched" = true ✓
```
→ Les deux matchent, pas de delta ici.

```
Token dans le texte : "CONFIG_SCHED_DEBUG"
Ground truth split : ["CONFIG_SCHED_DEBUG"] → starts_with("sched") = false (commence par C)
Index tokenizer    : ["config", "sched", "debug"] → "sched" starts with "sched" = true
```
→ Moteur trouve PLUS, ground truth moins. Ça donne startsWith > gt, pas l'inverse.

### Le vrai delta : cas où ground truth > startsWith

Il faut chercher un cas où :
- Ground truth split garde un token commençant par le préfixe
- Mais le tokenizer de l'index le split d'une façon qui **perd** le préfixe en début de token

Possible avec CamelCaseSplitFilter :
```
Token : "lockDep" 
Ground truth : ["lockDep"] → starts_with("lock") = true ✓
Index CamelCase : ["lock", "dep"] → "lock" SI=0 match ✓
```
→ Toujours OK.

```
Token : "unlockDevice"
Ground truth : ["unlockDevice"] → starts_with("lock") = false
Index CamelCase : ["unlock", "device"] → pas de match
```
→ Cohérent.

## Plan de diagnostic pour la prochaine session

### 1. Identifier les docs manquants

Modifier `t02_ground_truth_exhaustive` pour logger les doc_ids que le ground truth trouve mais pas startsWith :

```rust
// Pour chaque terme, collecter les doc_ids de chaque méthode
let gt_docs: HashSet<(usize, DocId)> = ...;  // ground truth
let sw_docs: HashSet<(usize, DocId)> = ...;  // startsWith search

let missing = gt_docs.difference(&sw_docs);
for (shard, doc) in missing.take(5) {
    // Lire le doc, afficher le texte autour du match ground truth
    // Afficher la tokenisation réelle via index tokenizer
}
```

### 2. Comparer tokenisation

Pour les docs manquants, comparer :
- Le texte brut
- Les tokens produits par le ground truth split
- Les tokens produits par le raw_code tokenizer
- Le contenu du SFX (SI=0 entries pour ce segment)

### 3. Vérifier le SFX reader

Vérifier que `prefix_walk_si0` sur le segment du doc manquant retourne bien le term ordinal du token attendu.

### 4. Fix probable

Aligner le ground truth sur le vrai tokenizer :
```rust
fn ground_truth_starts_with(...) {
    // Au lieu de split naïf, utiliser le même tokenizer que l'index
    let tokenizer = index.tokenizers().get("raw_code").unwrap();
    let mut stream = tokenizer.token_stream(text);
    while stream.advance() {
        if stream.token().text.starts_with(&prefix_lower) {
            count += 1;
            break;
        }
    }
}
```

Ou plus simple : changer le split du ground truth pour matcher SimpleTokenizer :
```rust
.split(|c: char| !c.is_alphanumeric())  // retirer le `&& c != '_'`
```
