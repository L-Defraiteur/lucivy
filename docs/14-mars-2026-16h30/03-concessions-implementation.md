# Concessions et limitations de l'implémentation actuelle

Date : 14 mars 2026 — 16h30

## 1. min_suffix_len = 3

Les suffixes de 1-2 chars ne sont pas indexés dans le .sfx. Un token de 5 chars
génère 3 suffixes (5, 4, 3 chars) au lieu de 5.

**Impact** : les queries contains de 1-2 chars ("ab", "x") ne passent pas par le .sfx.

**Mitigation** : fallback sur prefix walk du FST ._raw pour ces cas. Les queries
< 3 chars sont rares en pratique (code search = identifiants de 3+ chars).

**Alternative rejetée** : indexer tous les suffixes ≥ 1 char. Augmente la taille
du FST et génère beaucoup de multi-parents pour les suffixes courts ("s", "e", "a"
sont suffixes de centaines de tokens).

## 2. POSITION_GAP = 1 en dur

La conversion Ti → gap_index pour le multi-value suppose que le POSITION_GAP
entre values est exactement 1 (constante dans `postings_writer.rs:18`).

**Impact** : si POSITION_GAP change, la GapMap multi-value retournera des
séparateurs décalés.

**Mitigation** : POSITION_GAP est une constante privée qui n'a pas changé
depuis le fork. Si elle change, les tests multi-value casseront immédiatement
(test_multi_value_separator, test_multi_value_ti_to_seq).

**Fix futur** : stocker POSITION_GAP dans le header du .sfx ou de la GapMap
pour le rendre explicite.

## 3. Position phantom Ti dans le multi-value

Pour un doc avec 2 values (tokens Ti=0,1 puis Ti=3,4), la position Ti=2
est un "fantôme" — elle n'existe dans aucun posting list mais la GapMap
y a une entrée (le suffix de la value 0).

`read_separator(doc, 1, 2)` retourne `Some(b"")` (suffix vide de value 0)
au lieu de `None` ou `VALUE_BOUNDARY`.

**Impact** : aucun en pratique. Ti=2 n'apparaît jamais dans un posting list,
donc `read_separator(doc, 1, 2)` n'est jamais appelé lors d'une vraie recherche.
Le test `test_multi_value_separators_isolated` documente ce comportement.

**Fix futur** : ajouter un check `ti_belongs_to_any_value(ti)` qui vérifie
que le Ti existe réellement. Pas prioritaire.

## 4. Pas de fuzzy (d>0) encore

Le Levenshtein DFA n'est pas branché sur le suffix FST. Seules les queries
exact (d=0) sont supportées via prefix walk.

**Impact** : contains fuzzy continue d'utiliser le path existant (stored text).

**Plan** : Phase 5 du doc 07. Le DFA Levenshtein de startsWith
(`FuzzyTermQuery::new_prefix`) s'applique directement au suffix FST.
Même code, FST différent.

## 5. Multi-token search non implémenté

Le placeholder `suffix_contains_multi_token` existe mais retourne `Vec::new()`.
Les queries multi-token ("rag3db core") continuent d'utiliser le path existant.

**Impact** : seul le single-token contains bénéficie du .sfx pour l'instant.

**Plan** : Phase 4 du doc 07. La logique est :
- Premier token : .sfx exact, tout SI
- Milieu : ._raw exact, SI=0
- Dernier : .sfx prefix walk, SI=0
- Intersection curseurs triés + GapMap validation

## 6. Ordinals .sfx ↔ ._raw non vérifiés à runtime

Les ordinals du suffix FST sont calculés indépendamment (BTreeSet trié dans
SfxCollector) et supposés identiques aux ordinals du FST ._raw (même ordre
alphabétique). Aucune vérification runtime ne confirme cette correspondance.

**Impact** : si le tokenizer ._raw produit des tokens dans un ordre différent
de notre BTreeSet (ex: normalisation Unicode différente), les ordinals seront
décalés et les résultats faux, silencieusement.

**Mitigation** : les deux utilisent le même tokenizer (lowercase). Le BTreeSet
et le FST sont tous deux triés par ordre lexicographique des bytes. La
correspondance est garantie par construction.

**Fix futur** : ajouter une assertion debug dans les tests d'intégration qui
vérifie que `sfx_ordinal[i]` correspond au terme `raw_fst.ord_to_term(i)`.

## 7. Stored text toujours écrit

Le stored text (LZ4 compressé) continue d'être écrit à l'indexation pour
tous les champs, même si le .sfx le remplace sur le hot path de recherche.

**Impact** : taille d'index pas réduite. Le stored text est toujours utilisé
pour l'affichage du document complet et le regex cross-token.

**Fix futur** : Phase 6 du doc 07, optionnel. Pourrait être désactivé via
une option de schema `stored: false` pour les champs qui n'ont besoin que
de la recherche.

## 8. Un .sfx par champ ._raw

Chaque champ texte qui a un ._raw produit son propre fichier .sfx
(`{segment_uuid}.{field_id}.sfx`). Pour un schéma avec 5 champs texte,
c'est 5 fichiers .sfx par segment.

**Impact** : plus de fichiers sur disque. Négligeable en pratique (les segments
ont déjà 9+ fichiers chacun).

**Alternative rejetée** : un seul .sfx multi-champ. Plus compact en nombre de
fichiers mais complexifie le format et le reader. Pas de gain de performance.
