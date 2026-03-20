# Doc 08 — Investigation : SFX search diffs vs ground truth

Date : 20 mars 2026

## Constat

Bench 5K (linux kernel, LUCIVY_VERIFY=1) — SFX search vs ground truth substring :

| Terme | SFX hits | Ground truth | Diff | Status |
|-------|----------|-------------|------|--------|
| mutex | 610 | 610 | 0 | MATCH |
| printk | 178 | 178 | 0 | MATCH |
| lock | 2454 | 2455 | 1 | DIFF |
| sched | 420 | 424 | 4 | DIFF |
| function | 1285 | 1305 | 20 | DIFF |

Ces diffs sont **stables et reproductibles** — pas des race conditions.
C'est le même résultat avant et après les fixes SegmentComponent/merge.

## Hypothèses

### H1 : Le tokenizer ne produit pas le bon token

Le RAW_TOKENIZER fait : `SimpleTokenizer → CamelCaseSplitFilter → LowerCaser`

- SimpleTokenizer split sur les non-alphanumériques
- CamelCaseSplitFilter split camelCase et merge les chunks < 4 chars
- LowerCaser met en minuscules

Cas où "function" dans le texte brut ne produit pas le token "function" :
- **Underscore prefix** : `_function` → SimpleTokenizer split sur `_` → token "function" ✓ (devrait marcher)
- **CamelCase** : `myFunction` → CamelCaseSplit → "my", "Function" → lower → "my", "function" ✓
- **ALL CAPS** : `FUNCTION` → lower → "function" ✓
- **Joined** : `dysfunction` → SimpleTokenizer ne split pas → token "dysfunction" → SFX devrait trouver "function" comme substring... sauf si le SFX search cherche par token, pas par substring dans le token ?

### H2 : Le SFX search ne fait pas un vrai substring match

Le SFX search fait `prefix_walk` sur le suffix FST. Pour chaque terme "dysfunction",
les suffixes sont : "dysfunction"(SI=0), "ysfunction"(SI=1), ..., "function"(SI=7), ...

Donc "function" comme substring de "dysfunction" DEVRAIT être trouvé par le prefix_walk
sur le suffixe "function" (SI=7).

SAUF SI le SFX search ne regarde que SI=0 (début de mot) et pas SI>0 (substring).
À vérifier dans le code.

### H3 : Le ground truth fait un contains case-sensitive différent

Le ground truth fait `text.contains(term)`. Si le texte a "FUNCTION" (majuscules),
`contains("function")` ne matche PAS (case-sensitive). Donc le ground truth
devrait trouver MOINS de docs, pas plus.

SAUF SI le ground truth fait un `to_lowercase().contains()`. À vérifier.

### H4 : Multi-value / multi-token boundary

Le texte d'un doc peut avoir "function" splitté sur deux tokens par le tokenizer :
ex: "func" + "tion" si le CamelCaseSplitFilter coupe mal. Peu probable pour "function"
mais possible pour d'autres cas.

## Plan d'investigation

1. Trouver les docs manquants : pour chaque doc dans le ground truth mais PAS dans le SFX search, extraire le texte brut et le contexte du substring "function"
2. Tokenizer le texte avec RAW_TOKENIZER et vérifier quels tokens sont produits
3. Vérifier si le token "function" est dans le term dict du segment
4. Vérifier si le suffix FST a les bons suffixes pour ce token
5. Si le token n'est pas "function" (ex: "dysfunction"), vérifier que le SFX search fait bien SI>0

## Fichiers clés

- `lucivy_core/benches/bench_sharding.rs` — bench + ground truth
- `lucivy_core/src/diagnostics.rs` — inspect_term, inspect_sfx, compare_postings_vs_sfxpost
- `src/suffix_fst/collector.rs` — SfxCollector (construit le FST)
- `src/query/phrase_query/suffix_contains_query.rs` — SFX search query
- `src/tokenizer/simple_tokenizer.rs` — SimpleTokenizer
- `src/tokenizer/camel_case.rs` — CamelCaseSplitFilter
