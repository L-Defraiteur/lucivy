# Les spans seulement si on les demande

*13 septembre 2026, après la publication de 4.1.*

## Ce qui se passait

Le prescan construisait **toujours** un triplet `(document, début, fin)` par
match et le rangeait dans son cache. Le collecteur de highlights (`HighlightSink`)
ne décidait que de leur **émission** : sans lui, `emit_highlights` sortait
immédiatement et le vecteur était jeté sans avoir jamais été lu. Sur `de`, au
noyau, cela fait **7,9 millions de triplets** bâtis pour rien dès que l'appelant
ne voulait que des documents.

## Ce qui a changé

Un drapeau `want_spans`, déduit de la présence du collecteur, coupe la
construction du vecteur dans le chemin littéral (`run_sfx_v3_prescan`) et dans
`flatten` du chemin sans positions. La vérification, elle, ne bouge pas : c'est
elle qui prouve le match, et le tf se compte sur les matches, pas sur les spans.

Le fuzzy et la regex gardent leurs spans : leur tf **se déduit** du vecteur
(`for &(doc_id, _, _) in &highlights`). Les couper là demande de remonter le
comptage dans l'orchestrateur — à faire seulement si la mesure le justifie, et
leurs volumes se comptent en dizaines de milliers, pas en millions.

## La mesure

Noyau épinglé (Linux v7.2 à `8d3ae59288f1`, 101 373 fichiers), meilleur de deux
passes, index du banc :

| disposition | requête | documents | avec spans | sans spans | gain |
|---|---|---|---|---|---|
| dictionnaire | `de` (7,9 M spans) | 100 166 | 574,6 ms | 441,9 ms | −23 % |
| sans positions | `de` | 100 166 | 336,6 ms | **189,9 ms** | **−44 %** |
| dictionnaire | `ude` (478 k) | 74 500 | 87,4 ms | 77,1 ms | −12 % |
| sans positions | `ude` | 74 500 | 108,4 ms | 99,5 ms | −8 % |
| dictionnaire | `mutex_lock` (21 k) | 5 202 | 13,1 ms | 11,8 ms | −10 % |
| sans positions | `mutex_lock` | 5 202 | 24,7 ms | 22,5 ms | −9 % |

**Les comptes de documents sont identiques** dans les six lignes, et égaux à la
vérité. Reproduire : `V3_SPANS=0` avec le harnais (`spans-ab.sh` du scratchpad),
qui passe toujours un collecteur, donc signale des spans manquants — c'est voulu,
seuls les comptes et les temps sont lus.

**Correction d'une prédiction.** J'avais annoncé que le chemin sans positions ne
gagnerait rien, les spans y tombant de la vérification. C'est faux : à gros
volume, c'est là que le gain est le plus fort (−44 %), parce que le vecteur
aplati pèse des centaines de mégaoctets.

## Pas d'option publique

L'interface existait déjà : `highlights: false` est le défaut de tous les
bindings. Elle devient simplement honnête — on ne paie plus pour ce qu'on ne
demande pas. `V3_SPANS=0` ne reste qu'un levier de mesure.
