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

## À froid

Mesuré le 13 septembre avec `posix_fadvise(POSIX_FADV_DONTNEED)` sur tous les
fichiers de l'index — le cache de page se vide ainsi **sans droits root**, ce qui
rend la mesure rejouable (`cold.py` du scratchpad). Les deux index du banc, panel
réduit, une passe :

| disposition | requête | recherche à chaud | à froid | rapport | relecture des documents |
|---|---|---|---|---|---|
| dictionnaire | `mutex_lock` | 16,6 ms | 185,6 ms | ×11 | 123 → 128 ms |
| dictionnaire | `sched` | 12,1 ms | 20,7 ms | ×1,7 | 131 → 128 ms |
| dictionnaire | `schdule` fz1 | 49,2 ms | 71,0 ms | ×1,4 | 17 → 17 ms |
| dictionnaire | `spin_lock_[a-z]+` | 220,8 ms | 239,9 ms | ×1,1 | 55 → 55 ms |
| sans positions | `mutex_lock` | 26,7 ms | 58,4 ms | ×2,2 | 116 → 119 ms |
| sans positions | `sched` | 42,3 ms | 43,4 ms | ×1,0 | 125 → 127 ms |
| sans positions | `schdule` fz1 | 184,3 ms | 194,0 ms | ×1,1 | 17 → 17 ms |
| sans positions | `spin_lock_[a-z]+` | 11,6 ms | 19,5 ms | ×1,7 | 56 → 52 ms |

**Ce que ces chiffres disent, et ce qu'ils ne disent pas.**

- Le protocole évince le cache **entre les runs**, pas entre les requêtes : la
  première requête d'un run paie la montée de la FST et des sidecars (×11 sur
  `mutex_lock` en dictionnaire), les suivantes trouvent les pages déjà chaudes.
  C'est donc une mesure d'**ouverture à froid**, pas de « chaque requête à froid ».
- **L'index sans positions souffre nettement moins** : 2,4 Gio à monter au lieu de
  4,9, et il en lit moins — ×2,2 contre ×11 sur la même requête.
- **La relecture des documents ne bouge pas** (≈125 ms dans les deux états) : le
  document store était déjà hors cache à chaud. C'est une limite du protocole, pas
  un résultat ; ne pas en tirer que « la relecture est gratuite à froid ».
- Elasticsearch n'est pas comparable ici : ses données vivent dans son conteneur et
  dans sa JVM, qu'on ne refroidit pas de la même façon. Son chiffre à froid reste
  celui de sa première exécution après indexation, étiqueté comme tel dans le
  rapport.

## Pas d'option publique

L'interface existait déjà : `highlights: false` est le défaut de tous les
bindings. Elle devient simplement honnête — on ne paie plus pour ce qu'on ne
demande pas. `V3_SPANS=0` ne reste qu'un levier de mesure.
