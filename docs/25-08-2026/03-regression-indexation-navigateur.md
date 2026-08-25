# Régression : l'indexation navigateur ne passe plus

Rapport demandé le 25 août 2026 en fin d'après-midi. Question : à quel moment
l'indexation dans le navigateur fonctionnait-elle, et à quel commit.

## 1. Dernière fois où ça a marché

**Nuit du 24 au 25 août, entre ≈23:50 et 00:05, HEAD à `1fb67ec`**
(*« wasm: no ASYNCIFY, one merge at a time, lazy directory reads — the browser
passes its first commit »*, 24/08 23:42), avec dans l'arbre de travail ce qui
est devenu `5190663` (00:05).

Ce qui a réellement été fait alors : le navigateur a indexé **15 440 fichiers
du kernel** (~13,5 min en release) dans OPFS, puis le panel de 21 requêtes a
tourné dessus.

Preuves, par horodatage des artefacts :

| heure | artefact | ce que ça prouve |
|---|---|---|
| 24/08 23:42 | commit `1fb67ec` | HEAD au moment de l'indexation |
| 25/08 00:05 | commit `5190663` | ouverture d'un index OPFS en place + runner de parité (nécessaires au panel) |
| 25/08 **00:07** | `/tmp/parity_wasm.json` | **le panel a tourné sur l'index navigateur** — donc l'index existait |
| 25/08 00:13 | `/tmp/parity_wasm_release.json` | second passage |
| 25/08 00:53 | `/tmp/parity_wasm_compact.json` | compactage navigateur réussi (HEAD `ca3c57f`/`8b58881`) |
| 25/08 01:23 | `/tmp/parity_wasm_compact2.json` | second compactage |

L'index de 15 440 documents ainsi construit est celui rouvert toute la journée
sous `user_index` — 117 segments, 5,4 Go.

**Depuis 00:05, aucune indexation navigateur n'a été relancée** jusqu'à
aujourd'hui. Tout ce qui a été mesuré dans le navigateur entre-temps
(recherches, compactage, résidence) portait sur cet index déjà construit. La
fenêtre de régression est donc large et n'a été refermée par aucun test.

## 2. Ce qui se passe maintenant

Deux tentatives aujourd'hui, toutes deux échouées au **premier commit de
2 000 documents** :

- 14:30, HEAD `bb8985d`
- 15:12, HEAD `76a7e1e` (build avec symboles)

```
memory allocation of 402653184 bytes failed   (384 Mio, exactement)
Aborted()
```

Les threads d'indexation avortent, le flush n'aboutit jamais, et le graphe
d'attente reste bloqué sur `indexer_flush_finalize` (observé jusqu'à 1 415 s).

**Site exact**, obtenu en branchant l'anneau de diagnostic sur la page (il
était lu puis jeté, d'où « allocation failed » sans pile jusqu'ici) :

```
[alloc] realloc failed: 402653184 bytes, align 8
  <alloc::raw_vec::RawVecInner>::finish_grow
  <alloc::raw_vec::RawVec<…>>::grow_one
  <ld_lucivy::indexer::sfx_dag_v3::BuildFstV3Node as luciole::node::Node>::execute
  luciole::runtime::execute_dag
```

C'est un `Vec` du **constructeur de FST v3** qui double de 192 à 384 Mio,
pendant la construction du segment initial — pas pendant une fusion.

Le corpus n'est pas en cause : les 2 000 premiers documents sont **identiques**
à ceux de la nuit (même liste, `md5sum` des 2 000 premières lignes identique
entre `corpus_indexed.list` et `corpus_10k.list`).

## 3. Cause la plus probable : la taille des segments a quadruplé

Mesure au moment de l'échec (`[finalize]` dans `playground/diag.log`) :

```
finalize() 488 docs / 500 docs / 503 docs / 509 docs
[finalize] field 2: sfx build 22058ms, write 8 files / 132719KB
```

Soit **~500 documents par segment**, et 132 Mo de fichiers écrits pour un seul
segment d'un seul champ.

Or l'index construit la nuit compte **117 segments pour 15 440 documents**
(20 + 45 + 7 + 45 sur les quatre shards), soit **132 documents par segment**.

**Les segments sont donc ~3,8× plus gros qu'à la nuit, pour le même corpus et
le même seuil de commit (2 000 documents).**

Le mécanisme tient en une phrase : `ba48e60` (*« v3 indexes record term
frequencies only: positions and offsets were read by v2 scorers alone »*,
25/08 09:06) a **réduit** ce que l'index inversé stocke par document. Le tas
de l'écrivain (`WRITER_HEAP_SIZE`, 15 Mo en WASM) se remplit donc moins vite,
il retient plus de documents avant de vider, les segments grossissent — et le
pic du constructeur de FST, qui est proportionnel au nombre de tokens du
segment et **non** borné par ce tas, grossit d'autant.

Autrement dit : une économie de mémoire à un endroit a agrandi l'unité de
travail à un autre. Le tas de l'écrivain ne borne plus le vrai pic.

Note : cette explication est **cohérente avec toutes les mesures** mais n'est
pas encore prouvée par une bissection. Voir §5.

## 4. Fenêtre de suspicion

Commits entre le dernier succès et le premier échec, par ordre :

| commit | heure | touche l'indexation ? |
|---|---|---|
| `5190663` | 00:05 | handles épinglés, ouverture en place — non |
| `ab441ad` | 00:17 | **fusion v3 en arènes** — oui (fusion, pas build initial) |
| `ca3c57f` | 00:36 | compactage planifié dans l'acteur — non |
| `8b58881` | 00:59 | compactage — non |
| `ba48e60` | 09:06 | **`WithFreqs` : moins stocké par document** — oui, suspect nº 1 |
| `40ee55f` | 09:48 | recherche par lots — non |
| `b567506` | 09:51 | runtime vivant — non |
| `b316520` → `045e2ef` | 10:00-10:53 | lecture d'en-tête, cache d'en-tête, LUCE — non |
| `9ca89fc` | 13:46 | **WSP3** (writer) — oui |
| `e4bf1f0` | 13:54 | **SIB2** (writer) — oui |
| `7697d52` | 14:24 | résidence — non |
| `ee3f4a0` | 14:50 | **SFP3** (writer) — oui |
| `bb8985d` | 14:58 | scratch des writers — correction, pas cause |

`bb8985d` corrigeait un vrai défaut (un `Vec` par ordinal dans les writers)
mais **n'a rien changé au symptôme** : l'échec revient à l'octet près, ce qui
confirme que la cause est ailleurs — et la pile le dit : dans le constructeur
de FST, pas dans les writers.

## 5. Comment trancher

Deux voies, la première est bien moins chère :

1. **Vérifier l'hypothèse directement** : forcer des segments petits
   (`LUCIVY_WRITER_HEAP` plus bas, ou un plafond en documents par segment) et
   relancer. Si l'indexation passe, la taille des segments est la cause et le
   correctif est un plafond explicite — le tas de l'écrivain ne borne pas le
   pic du constructeur de FST, il ne l'a jamais fait, ça ne se voyait pas.
2. **Bissection** : `ba48e60` d'abord (suspect nº 1), puis `ab441ad`. Chaque
   essai coûte ~3 min de build et ~5 min avant l'abandon, l'échec étant
   précoce et reproductible.

## 6. Ce que ça dit de l'architecture proposée

L'idée de séparer **indexer** (OPFS, mémoire bornée) de **servir** (un paquet
LUCE résident) n'est pas une optimisation : si le pic d'indexation n'est pas
borné, c'est la seule voie qui tienne. Et le plafond par segment de §5.1 est
exactement ce qui rend la phase 1 bornée — indépendamment du LUCE.
