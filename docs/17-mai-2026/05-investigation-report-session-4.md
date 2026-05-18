# Rapport d'investigation — Session 4 (17-18 mai 2026)

## Objectif

Fixer les faux positifs (FP) et faux négatifs (FN) du ground truth SFX v3
sur 500 fichiers C++ (clone rag3db).

## État initial (début de session)

Ground truth : 6 pass, 9 fail sur 15 queries.
Principaux problèmes :
- function strict: 62→22 (40 FN)
- uint64_t strict: 11→0 (11 FN)
- include strict: 29→21 (8 FN)
- std::unique_ptr strict: 8→7 (1 FN)

## Tentative 1 : Désactiver le content_len filter

**Changement** : commenter `matches.retain(|m| byte_to - byte_from >= query_content_len)`
**Résultat** : corrige certains FN (struct 70→71, rag3db 49→51, uint64_t 0→11) mais crée des FP (include 21→33, uint64_t relax 15→33)
**Conclusion** : le filtre est nécessaire mais trop agressif

## Tentative 2 : Content_len filter span=1 only

**Changement** : `matches.retain(|m| m.span > 1 || byte_span >= query_content_len)`
**Résultat** : les chains (span>1) passent le filtre. Améliore les FN sans créer de FP sur les single-token.
**Conclusion** : gardé dans le code final

## Tentative 3 : Content-prefix ordinals

**Changement** : content key = `extract_content_prefix(text)` (scan leading alphanum chars) au lieu de `text[..own_len]`
**Résultat** :
- function strict: 22→39 (+17), include strict: 21→29 ✓, uint64_t strict: 0→9
- Mais FP sur include (+4), uint64_t relax (+10)
**Pourquoi** : les ordinals ignorent les seps → "ion " et "ion\n-" partagent un ordinal → le chain trouve tous les docs. Mais aussi : les overlaps sont mélangés → FP
**Conclusion** : améliore les FN mais crée des FP d'overlap

## Tentative 4 : Vec<Vec<u64>> chain alternatives

**Changement** : `TokenChainV3.ordinals: Vec<u64>` → `Vec<Vec<u64>>`. Chaque position stocke les ordinals alternatifs. Le resolve fait l'union des postings.
**Résultat** : function strict: 39→62 ✓ (avec content-prefix). Tous les FN cross-token résolus.
**Problème** : la première tentative (forking exponentiel) a crashé Ubuntu. Résolu avec les alternatives par position.
**Conclusion** : gardé dans le code final

## Tentative 5 : anchor_start=true pour remainder

**Changement** : `fst_candidates(remainder, anchor_start=true)` dans cross_token_chain
**Résultat** : réduit les FP en empêchant les matches SI>0 au dernier step (ex: "t" matchant "state" à SI=1)
**Conclusion** : gardé dans le code final

## Tentative 6 : Word map global (ChunkWordMap + NextWordMap)

**Changement** : ordinal → [(word_id, chunk_index, total_chunks)] + word_id → [next_words]
**Résultat** : 0 rejects. La word map dit "ordinal X peut être chunk 1 de mot Y" — c'est vrai globalement pour TOUTES les variantes → ne filtre pas
**Conclusion** : insuffisant seul, la vérif doit être per-doc

## Tentative 7 : WordPosMap per-doc

**Changement** : (doc_id, position) → word_id_within_doc. Vérifie same_word.
**Résultat** : intra-word = toujours valide (chunks contigus). Inter-word = aussi valide (frontière de mot = bytes contigus). Ne filtre rien.
**Conclusion** : le problème n'est PAS la structure mot — c'est le CONTENU des overlaps

## Tentative 8 : TermTexts verification (post-filtre bytes)

**Changement** : pour chaque chain match, vérifier text[sti..] == query via TermTexts
**Résultat** : réduit certains FP (struct 83→73). Mais casse std::unique_ptr (8→0) car TermTexts stocke un seul texte par ordinal et le mauvais texte était stocké (word-stripped écrasait le chunk)
**Bug trouvé** : TermTexts dans AssembleV3Node n'excluait pas is_word_stripped → le texte word-stripped écrasait le texte chunk. Fix: `if meta.is_word_stripped { continue; }`
**Conclusion** : TermTexts verification pas fiable (un seul texte par ordinal = arbitraire)

## Tentative 9 : Ordinals text[..own_len] (retour v2-style)

**Changement** : content key = `text[..own_len]` (content+sep, sans overlap)
**Résultat** : élimine les FP de mélange de seps. Avec Vec<Vec<u64>> alternatives, pas de FN.
- return strict: 463/463 ✓ (was 464)
- uint64_t strict: 11/11 ✓
- std::unique_ptr strict: 8/8 ✓
- struct strict: 78/71 (7 FP restants)
**Conclusion** : meilleur compromis. Les 7 FP restants viennent du mélange d'OVERLAPS

## Tentative 10 : Ordinals extended-text (texte complet avec overlap)

**Changement** : content key = texte complet (content+sep+overlap)
**Résultat** : identique à text[..own_len] car les word-stripped doublons persistent
**Problème** : word-stripped FST key pointe vers UN SEUL overlap variant → les autres docs ne sont pas trouvés en relaxed
**Conclusion** : casse le relaxed mode

## Tentative 11 : Word-stripped postings séparés

**Changement** : word-stripped dans content_key_map avec leur propre ordinal
**Résultat** : les word-stripped ajoutent des postings au même (doc, pos) que les chunks → doublons → FP
**Conclusion** : les word-stripped NE DOIVENT PAS avoir leurs propres postings

## Tentative 12 : Word-stripped mappés au premier chunk (état actuel)

**Changement** : word-stripped EXCLUS de content_key_map. `intern_to_final[ws] = intern_to_final[first_chunk]`. Pas de posting word-stripped. Builder 0x02 utilise `first_chunk_intern_ord`.
**Résultat** : 125/126 unit tests pass (1 fail: include_vs_inclusive, connu). Ground truth strict: 7/8 queries parfaites. struct: 7 FP restants, function: 1 FP, rag3db: 11 FP.
**Problème restant** : les FP viennent ENCORE de ordinals fantômes au même (doc, pos). 5 ordinals distincts trouvés pour un même (doc, pos) dans le raw trace. Source inconnue.

## Problème non résolu : ordinals fantômes

Le raw match trace montre 5+ ordinals distincts avec des postings au même (doc, pos).
Pourtant :
- Il n'y a qu'un seul `token_postings[].push()` dans le code (ligne 257)
- Les word-stripped postings sont supprimés
- content_key_map exclut les word-stripped

**Hypothèses pour la prochaine session** :
1. Les ordinals viennent de content_key_map qui groupe par text[..own_len]. Si un token apparaît dans plusieurs docs avec le même text mais own_len différent (UTF-8 snap?), ils pourraient créer des entries séparées
2. Le même intern_id apparaît dans content_key_map sous des clés différentes à cause d'un bug de calcul own_len
3. Il y a un autre code path qui ajoute des postings (dans le merge ou ailleurs)
4. C'est les chunks intra-mot avec overlaps différents qui ont le même text[..own_len] → même ordinal → mais le falling walk matche sur une variante d'overlap et le posting vient d'une autre. C'est le problème d'overlap mixing avec text[..own_len] ordinals.

**Prochaine étape recommandée** : vérifier si les 5 ordinals correspondent à 5 content keys différentes. Si oui, les doublons ne viennent PAS des postings mais du fait que la falling walk + fst_candidates produit des chain matches pour 5 splits différents, chacun avec un ordinal différent, et par coïncidence chaque ordinal a un posting au même (doc, pos) pour un AUTRE token.

Autrement dit : le match ord=13275 a un posting au (doc=2, pos=1969) mais c'est pour un TOKEN DIFFÉRENT de celui au pos=1969. Ce token (avec ord=13275) est à (doc=X, pos=1969) dans un AUTRE doc, et le sfxpost a cette entrée sous ordinal 13275. Sauf que chaque (doc, pos) n'existe que sous UN ordinal...

Ou bien : 5 chains différentes produisent des matches pour le même doc avec des bytes overlapping. Le dedup par (doc_id, position) ne les supprime pas car les ordinals diffèrent (dedup key = (doc_id, position), pas ordinal).

## Code changes actuel (à commiter)

### Gardés :
- `Vec<Vec<u64>>` pour TokenChainV3.ordinals
- `last_ordinal` dans MatchV3 
- Content key = text[..own_len] (pas extended, pas content-prefix)
- Word-stripped exclus de content_key_map, mappés au premier chunk
- Pas de posting pour word-stripped/tail
- anchor_start=true pour remainder dans chain builder
- TermTexts exclut word-stripped dans AssembleV3Node
- resolve_suffix cherche aussi partition 0x02
- word_map.rs module (ChunkWordMap + NextWordMap + verify_chain_adjacency)
- word_pos_map.rs module (WordPosMapWriter/Reader)
- Registered dans index_registry
- Debug trace conditionnel (V3_DEBUG_QUERY env var)
- Ground truth test debug_struct_fp
- 3 design docs + 1 investigation report

### À investiguer prochaine session :
1. Pourquoi 5 ordinals au même (doc, pos) dans le raw trace ?
2. Les FP "struct" restants : overlap mixing ou autre cause ?
3. Relaxed mode FP : la partition 0x02 a ses propres problèmes (beaucoup de FP)
4. Le test diag_include_vs_inclusive qui fail
