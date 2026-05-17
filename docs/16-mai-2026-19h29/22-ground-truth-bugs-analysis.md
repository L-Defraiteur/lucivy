# Ground Truth Bugs — Analyse v3 sur repo rag3db

**Date** : 17 mai 2026  
**Dataset** : 500 fichiers de `L-Defraiteur/rag3db`  
**Résultats** : 6/10 pass, 4 fail

---

## Résumé

| Query | Grep | V3 | Verdict | Cause |
|-------|:----:|:--:|---------|-------|
| `function` | 62 | 64 | +2 faux positifs | cross-token boundary incorrect |
| `return` | 463 | 463 | OK | |
| `include` | 29 | 29 | OK | |
| `struct` | 71 | 71 | OK | |
| `void` | 18 | 18 | OK | |
| `uint64_t` | 11 | 24 | +13 faux positifs | cross-token boundary incorrect |
| `std::unique_ptr` | 8 | 5 | -3 faux négatifs | à investiguer |
| `ku_dynamic_cast` | 0 | 0 | OK | |
| `TableFunction` | 0 | 4 | +4 faux positifs | cross-token boundary incorrect |
| `rag3db` | 51 | 51 | OK | |

---

## Bug 1 : Faux positifs cross-token (function, uint64_t, TableFunction)

### Symptôme

La query `"function"` (8 bytes) matche dans `"WriteTransaction\n-STATEMENT"`. Le highlight montre `>>ction\n-S<<` — 8 bytes qui traversent un newline.

La query `"uint64_t"` matche dans `"Uint64ToInt64OutOfRange"` qui contient `"uint64t"` (sans underscore entre 64 et oint). Le `_` de la query matche un boundary de token qui n'est pas un underscore réel dans le texte.

La query `"TableFunction"` matche dans `"TABLE_FUNCTION_ENTRY"` et `"table function"` (avec espace/underscore entre les mots).

### Cause probable

Le falling walk cross-token (dans `cross_token_chain_v3`) résout des chaînes de tokens adjacents sans vérifier que les bytes entre les tokens correspondent bien aux bytes de la query. Le chain vérifie l'adjacence par **position** (pos+1) mais pas par **contenu exact** des bytes entre les tokens.

Concrètement : le token `"Transac"` suivi de `"tion\n-S"` matche la chaîne `"function"` parce que :
1. Falling walk trouve `"func"` dans un token (substring match)
2. Le remainder `"tion"` est cherché dans le token suivant via `fst_candidates`
3. `"tion"` est trouvé au début du token suivant
4. L'adjacence `pos+1` est vérifiée → match

Mais le texte réel entre les deux positions est `"Transac"` + `"tion\n-S"`, pas `"func"` + `"tion"`. Le `"func"` n'est PAS à la fin du premier token, et le `"tion"` n'est PAS au début du second dans le bon contexte.

### Analyse

Le problème est dans `resolve_chains_v3` : l'adjacence par position seule ne suffit pas. Il faut vérifier que :
1. Le **dernier token de la chaîne** contient le suffix attendu (overlap validated)
2. Le **premier byte du token suivant** correspond au remainder de la query
3. Les bytes entre les tokens (seps absorbés) correspondent bien à la query

En v2, le sibling table et le gapmap géraient ça. En v3, l'overlap de 2 bytes est censé couvrir ce cas, mais la vérification n'est pas assez stricte.

### Piste de fix

Option A : dans `resolve_chains_v3`, après la vérification d'adjacence par position, vérifier que `byte_to` du token précédent == `byte_from` du token suivant (continuité byte). Le texte doit être continu dans le document original.

Option B : dans la chaîne, valider que les bytes d'overlap du token précédent correspondent aux premiers bytes du token suivant. L'overlap de 2 bytes donne un "proof" que les tokens sont bien adjacents dans le texte.

Option C : dans le falling walk, quand on split au own_len, vérifier que l'overlap bytes matche le début de la query remainder. Si l'overlap est `"ti"` et le remainder commence par `"ti"` → OK. Si l'overlap est `"ti"` et le remainder commence par `"on"` → rejet.

**Option C semble la plus propre** : c'est exactement le rôle de l'overlap. Le falling walk v3 détecte déjà `overlap_validated` (nombre de bytes d'overlap consommés). Si overlap_validated == 0 et overlap_len > 0, c'est suspect — le chain a splitté mais n'a pas vérifié la continuité.

---

## Bug 2 : Faux négatifs std::unique_ptr (3 docs manqués)

### Symptôme

3 fichiers contiennent `"std::unique_ptr"` mais v3 ne les trouve pas. Les fichiers sont :
- `test/api/result_value_test.cpp` : `std::vector<std::unique_ptr<rag3db::common::Value>>`
- `test/api/api_test.cpp` : `std::unordered_map<std::string, std::unique_ptr<Value>>`
- `test/storage/node_insertion_deletion_test.cpp` : `std::unique_ptr<Connection>`

### Cause probable

Le texte `"std::unique_ptr"` est tokenisé comme :
- `"std"` (content) + `"::"` (sep) → token `"std::"` (5 bytes)
- `"unique"` (content) + `"_"` (sep) → token `"unique_"` (7 bytes)
- `"ptr"` (content) → token `"ptr"` (3 bytes)

La query `"std::unique_ptr"` doit traverser 3 tokens. Le falling walk chain doit :
1. Trouver `"std::"` → split → remainder `"unique_ptr"`
2. Trouver `"unique_"` → split → remainder `"ptr"`
3. Trouver `"ptr"` → match

Mais le `"std::unique_ptr"` est suivi de `"<"` dans le texte (ex: `"std::unique_ptr<Connection>"`). Le token `"ptr"` aurait un overlap de `"<C"` ou `"<c"`. Et le `max_depth` du chain est 8, donc 3 tokens devraient passer.

Le problème pourrait être que ces 3 fichiers spécifiques ont une tokenisation différente. Par exemple, si `"std::unique_ptr"` est dans un contexte plus long qui fait que les chunks sont découpés différemment.

### Piste d'investigation

1. Tracer la tokenisation de `"std::unique_ptr<Connection>"` avec EqualChunkTokenizer
2. Vérifier que le falling walk chain produit les bons splits
3. Vérifier que les 3 tokens ont des postings dans les mêmes docs
4. Vérifier que l'adjacence pos+1 est correcte pour ces 3 docs

---

## Bug 3 : Highlights trop larges

### Observation

Certains highlights couvrent des milliers de bytes (ex: `[2578..21945]` pour `"TableFunction"` dans `show_functions.csv`). C'est parce que le cross-token chain produit un `byte_from` au début du premier token et un `byte_to` à la fin du dernier token de la chaîne. Si la chaîne traverse beaucoup de tokens, le highlight couvre tout entre les deux.

### Cause

Le `resolve_chains_v3` stocke `byte_from_first` (du premier token) et `byte_to` (du dernier token du chain). Si le chain a un mauvais match (faux positif), le span peut être énorme.

Ce n'est pas un bug en soi si le match est correct — un match légitime sur 3 tokens devrait avoir un highlight de ~20 bytes. Mais les highlights de 19K bytes indiquent un faux positif (Bug 1).

---

## Prochaines étapes

1. **Fixer Bug 1** (faux positifs cross-token) — priorité haute, c'est le plus impactant
2. **Investiguer Bug 2** (faux négatifs std::unique_ptr) — tracer la tokenisation
3. **Le Bug 3** se résoudra quand Bug 1 sera fixé (les highlights géants sont des conséquences des faux positifs)

Le rapport complet est dans `/tmp/v3_ground_truth_report.txt`.
