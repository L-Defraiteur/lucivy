# Rapport de session — 12 au 13 septembre 2026

*De la fin du chantier « index sans positions » à la publication de 4.1.0, puis
au comparatif refait sur un corpus épinglé. Autonome : tout ce qui est affirmé
ici a sa commande de reproduction dans `05-knowledge-dump.md`.*

## 1. Ce qui a été publié

**4.1.0, le 13 septembre 2026**, tag `v4.1.0` sur `main` = `0a41f24`.

| registre | paquets | vérifié |
|---|---|---|
| PyPI | `lucivy` | 4.1.0 |
| npm | `lucivy`, `lucivy-wasm` et les 5 paquets de plateforme | 4.1.0 |
| crates.io | `luciole`, `lucistore`, `ld-lucivy`, `lucivy-core`, `sparse-vector` | 4.1.0, publiés dans cet ordre entre 09:45:36 et 09:47:14 |
| GitHub | release `v4.1.0`, 12 artefacts | — |

Le contenu : l'index sans positions et ses interfaces, le correctif « une
occurrence, un span », la CI en trois fichiers, le README de PyPI réparé.

## 2. Les chantiers, dans l'ordre

### 2.1 Rendre `positions: false` utilisable (commits `6fa26ab`, `ca896d2`)

- **Validation corrigée** : l'option refusait un champ texte sans `"stored": true`
  explicite, alors que le handle stocke par défaut (`stored.unwrap_or(true)`).
  Seul `"stored": false` est refusé désormais.
- **Node** : `Index.create(path, fields, { positions: false, shards, … })`
  (`IndexOptions`, typé ; mélanger objet et arguments est refusé).
- **Documentation** : « What's new in 4.1 » dans les quatre README de bindings,
  README principal, `lucivy_core/README.md`, `ARCHITECTURE.md`, CHANGELOG.
- **Playground `?nopos`** et WASM rebâti.

**Vérifié dans Chrome**, ce qui n'avait jamais été fait pour cette option :
2 000 fichiers du noyau 268 → 174 Mo ; **10 000 fichiers 1 052 → 638 Mo**, 43 →
37 s, pic mémoire inchangé (1,5 Go), **fusions au-delà de 2 000 documents sans
problème** (le palier que Lucie signalait). Panel de parité de 21 requêtes
identique entre les deux dispositions, aux ex æquo près.

### 2.2 Un défaut du moteur publié, trouvé par cette comparaison (`66381bc`)

En relâché, une aiguille qui termine un mot découpé en morceaux (`lock` dans
`superblock` = `super` + `block`) **sortait deux fois** de la phase littérale :
par l'entrée mot (position du premier morceau) et par le dernier morceau, aux
mêmes octets. La déduplication avait pour clé `(doc, position, byte_from)` :
les deux survivaient, le span était rendu en double et **comptait deux fois dans
le tf**, donc dans le score et dans l'ordre.

- **Ampleur** : 542 doublons pour `lock` et 68 pour `init` sur 10 000 fichiers ;
  585 documents et 3 050 doublons sur `contains_split "spin lock init"`.
- **Depuis quand** : le moteur v3, 4.0.2 comprise.
- **Pourquoi invisible** : la vérité terrain comparait les spans en `HashSet`.
- **Correctif** : `orchestrator::dedup_occurrences`, clé `(doc, byte_from,
  byte_to)` ; le harnais compte maintenant les doublons comme spans en trop.
- **Preuve** : rouge `extra=542` avant, exact après ; test
  `test_relaxed_duplicate_spans` (échoue sur l'ancien code), panel 10/10 dans
  les trois dispositions ; `de` +4 % (26 → 27 ms).

### 2.3 La CI refaite en trois fichiers (`ca0d901`)

`v4.1` comptait 21 commits qu'aucune CI n'avait vus : `ci.yml` ne se déclenchait
que sur `main`.

| fichier | rôle | déclencheurs |
|---|---|---|
| `ci.yml` | le code est juste | push `main` et `v*`, PR vers `main`, appelé par `release.yml` |
| `build.yml` | ça se construit partout (5 plateformes, sdist, WASM) | PR vers `main`, push `main` touchant aux bindings, à la main, appelé |
| `release.yml` | publier | tag `v*`, ou à la main |

`release.yml` **appelle** les deux au lieu d'en garder une copie (`checks`,
supprimé) : le feu vert d'une publication est désormais la CI de tous les jours.
`ci.yml` a gagné la suite `lucivy-core` complète, `lucivy-cpp`, pytest et les six
fichiers de tests Node, qui ne tournaient qu'à la main.

### 2.4 La publication (PR #16 → `main` → tag)

Chemin : branche de travail → PR → fusion en **avance rapide** (la tête de `main`
est exactement le commit que la CI a validé) → tag. Deux aléas :

- un **`startup_failure`** de GitHub sur la CI de `main`, relancé sans
  modification et reparti (ni notre fichier, ni un incident déclaré) ;
- **`test_snapshot_served`** comparait un top-10 d'ex æquo complet : sur
  `contains kmalloc`, dix documents au même score `0.00033313446`, ordonnés
  selon la disposition des segments, qu'un snapshot servi en place ne reproduit
  pas. Corrigé comme le roundtrip LUCE l'avait été le 6 septembre : comparer
  tous les résultats triés par score puis identifiant.

### 2.5 « 70 short » : c'était notre motif, pas Elasticsearch (`1408bff`)

Question de Mark Harwood (ex-Elastic) sur LinkedIn. Vérification faite sur les
**octets qu'Elasticsearch stocke lui-même** (93 983 valeurs balayées) :

| calcul | documents |
|---|---|
| regex `spin_lock_[a-z]+`, sensible à la casse | 5 440 |
| la même, insensible à la casse | 5 510 |
| ce que rend Elasticsearch avec `case_insensitive: true` | 5 440 |
| ce qu'il rend écrit `[a-zA-Z]+` | **5 510** |

**Cause** : `case_insensitive` de Lucene replie les **littéraux** d'un motif, pas
ses **classes de caractères** — `spin_lock_` attrapait `SPIN_LOCK_`, `[a-z]+`
refusait `UNLOCKED`. Reproduit sur un champ `keyword` : ce n'est pas le type
`wildcard`. Corrigé dans les cinq README, la page, l'article, le rapport et le
banc ; note et reproduction en quatre lignes : `docs/12-09-2026/01`.

### 2.6 Le banc rendu reproductible (`745ac2e`)

- **Corpus épinglé par commit** : `torvalds/linux` v7.2, `8d3ae59288f1`. Le
  script clone s'il manque, remet sur ce commit s'il a dérivé, écrit l'empreinte
  dans le dossier de travail et **jette les index en cache quand elle change**.
- **Elasticsearch réutilise** un index portant la même empreinte (marquée dans
  `_meta` du mapping, compte de documents en garde-fou) : les reprises de la
  journée n'ont coûté que les requêtes au lieu de deux minutes de trigrammes.
- **Quatrième disposition** `dict-nopos` dans le banc.

### 2.7 Le comparatif refait (`9a15bf3`, `3a215ff`, `1657d0c`, `9c6b7f6`)

Sur les mêmes octets (101 373 fichiers, 899 Mo) :

| moteur | index | × texte | indexation |
|---|---|---|---|
| **lucivy 4.1, dictionnaire + `positions: false`** | **2 478 Mo** | **×2,8** | **94 s** |
| Elasticsearch 8.19, trigrammes + `wildcard` | 3 050 Mo | ×3,4 | 118 s |
| lucivy, dictionnaire + `derived_in_ram` | 3 392 Mo | ×3,8 | 108 s |
| lucivy, dictionnaire partagé | 5 044 Mo | ×5,6 | 112 s |
| tantivy 0.25, `NgramTokenizer` | 735 Mo | ×0,8 | 5 s |

**L'index qui répond exactement à tout est plus petit et plus rapide à bâtir que
celui d'Elasticsearch qui répond parfois faux en silence.**

Et le banc compare enfin **la même question des deux côtés** : chaque ligne porte
le `took` d'Elasticsearch (documents) *et* ce que `highlight` lui coûte pour
marquer les 200 premiers — 28 à 187 ms selon la requête, **673 ms sur la regex**,
quand nous rendons *tous* les spans en 12 à 237 ms.

Les quatre dispositions passent le panel de vérité terrain **10/10 sur le noyau
entier**, doublons comptés : c'est la vérification à pleine échelle d'après
correctif.

### 2.8 Les spans seulement si on les demande (`10dadfe`)

Le prescan matérialisait toujours un triplet par match et le mettait en cache,
alors que sans collecteur il n'était jamais lu (`de` : 7,9 M triplets pour rien).
Un drapeau `want_spans`, déduit de la présence du collecteur, coupe ce vecteur
dans le chemin littéral et dans `flatten` du chemin sans positions.

| disposition | requête | avec spans | sans spans | gain |
|---|---|---|---|---|
| dictionnaire | `de` | 574,6 ms | 441,9 ms | −23 % |
| sans positions | `de` | 336,6 ms | **189,9 ms** | **−44 %** |
| dictionnaire | `mutex_lock` | 13,1 ms | 11,8 ms | −10 % |
| sans positions | `mutex_lock` | 24,7 ms | 22,5 ms | −9 % |

Comptes de documents identiques partout. **Aucune option publique** :
`highlights: false` était déjà le défaut de tous les bindings, il devient
seulement honnête. Le fuzzy et la regex gardent leurs spans (leur tf s'en
déduit). Détail : `02-spans-a-la-demande.md`.

### 2.9 La mesure à froid (`6cd681d`)

`posix_fadvise(DONTNEED)` vide le cache de page **sans droits root**
(`benches/cold_cache.py`). L'ouverture à froid coûte ×11 sur la première requête
en dictionnaire contre **×2,2 sans positions** (2,4 Gio à monter au lieu de 4,9) ;
les requêtes suivantes retombent à ×1,0-1,7. Ce que le protocole ne dit pas est
écrit dans la note : l'éviction a lieu entre les runs, pas entre les requêtes.

## 3. Décisions de Lucie

- Publier : fusionner d'abord dans `main`, taguer ensuite ; autorisation donnée
  de poser et pousser le tag une fois la CI verte.
- Compte `gh` sur `L-Defraiteur` (personnel) avant toute action sur le dépôt.
- `main` n'a pas été touchée avant la fusion : les corrections du 12 y sont
  arrivées avec 4.1.0.
- L'article n'est pas soumis à Hacker News : « on va encore bosser jusqu'à avoir
  la meilleure des libs ».
- Deux notes de vision demandées et écrites (`01-index-a-la-carte.md`).

## 4. Erreurs de méthode, et ce qu'elles ont appris

- **Comparer deux corpus** : le « 70 short » et, plus tôt, une comparaison
  faussée par un clone du noyau reclôné le 11. D'où l'épinglage par commit.
- **Cinq pushes d'affilée** : `cancel-in-progress` a annulé la CI de quatre
  commits successifs ; seul le dernier a eu un verdict. Regrouper avant de
  pousser, ou restreindre l'annulation aux pull requests (proposé, non fait).
- **Deux scripts de mesure faux** : `V3_QUERIES` oublié (le harnais a tourné son
  panel par défaut), puis un filtre de sortie trop étroit qui a rendu un fichier
  vide qu'on pouvait prendre pour un échec du moteur. Vérifier qu'une mesure
  produit des lignes avant d'en tirer une conclusion.
- **Une prédiction fausse, corrigée** : j'avais annoncé que le chemin sans
  positions ne gagnerait rien à couper les spans. C'est là qu'il gagne le plus.

## 5. Ce qui reste ouvert

1. **Profiler l'indexation** : 94 à 112 s contre 5 s pour tantivy, jamais regardé
   depuis le repli différé du dictionnaire.
2. **Le document store** : 123 ms pour aller chercher les documents contre 16 ms
   de recherche sur `mutex_lock` — piste principale des requêtes « documents
   seuls ». C'est le prochain sujet.
3. **Restreindre `cancel-in-progress` aux pull requests.**
4. **L'index à la carte** et la conversion de nos propres index (`01`).
5. **Soumettre l'article**, à jour avec les chiffres du 13.
