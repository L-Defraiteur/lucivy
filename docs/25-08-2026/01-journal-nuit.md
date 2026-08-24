# Journal de nuit — 25 août 2026

Suite directe du 24 (`docs/24-08-2026/06-recap-progression-et-a-faire.md`,
doc 41 rag3weaver). Branche `wip/publication-3.0.0`. Priorités données avant
de dormir : (1) finir le débogage WASM/parité, (2) la fusion v3 en arènes.
Entrées horodatées, les plus récentes en bas.

## 00:05 — état au départ

- Navigateur : 15 440 fichiers indexés en ~15 min (build debug), 413
  segments, 24 fusions une à une, tas au plus haut 2,3 Go, 5,5 Go de
  sidecars écrits dans OPFS. Commit `1fb67ec` (sans Asyncify, permis de
  fusion, lectures paresseuses).
- Panel de parité (21 requêtes) : échecs `read sfx: No such file (os error
  44)` — un searcher tient des segments fusionnés puis supprimés ; en natif
  c'est masqué par mmap. Corrigé dans l'arbre (pas encore commité) : les
  handles paresseux **épinglent** les octets d'un fichier supprimé tant
  qu'ils vivent (sémantique unlink), test natif
  `lucivy_core/tests/test_lazy_directory.rs` (3 verts).
- Rejouer le panel sans réindexer : `?open=user_index` (ouverture directe
  de l'index OPFS). Bloqué : au rechargement, `OPFS mount failed (ret=-20)`
  (ENOTDIR de `wasmfs_create_directory("/opfs")`), reproductible deux fois,
  alors que le montage réussissait au chargement précédent. En cours.

## 00:25 — parité navigateur / natif : acquise

- Le montage OPFS a réussi au 3e chargement (`?nodemo`) ; le panel a tourné
  **depuis le worker** (`playground/parity_worker.js` : `lucivy_open` sur
  l'index OPFS + `lucivy_search`), sans dépendre de l'état de la page.
- **19/21 requêtes identiques** au natif (comptes, top-10 ordonné, scores à
  1e-4, nombre de spans) ; les 2 autres : mêmes docs, mêmes scores, mêmes
  spans, ordre des ex æquo différent (disposition des segments différente).
  Tous les modes couverts. Résultats : `/tmp/parity_native.json`,
  `/tmp/parity_wasm.json`, diff `/tmp/parity_diff.txt`.
- Temps en build debug : 4-14 s par requête (natif 35-670 ms) ; un « sans
  résultat » coûte 4,6 s → coût fixe par requête (ouverture des readers de
  ~100 segments × 4 shards, asserts). À remesurer en release.
- Commit `5190663` (épinglage unlink, `?open=`, runners).
- Le montage OPFS qui échoue (`ret=-20`) juste après un rechargement reste
  à comprendre : deux échecs consécutifs puis un succès, sans changement
  de code. Hypothèse : access handles OPFS de l'ancien worker pas encore
  libérés (course), à confirmer avec un délai/retry au montage.

## 00:30 — fusion v3 en arènes : écrite, en test

`merge_segments_v3` : arène de textes `(start, len)`, table d'intern en
adressage ouvert (FxHash de la forme + texte, comparaison contre l'arène,
zéro allocation sur un hit, dimensionnée 2× la somme des termes), postings
chunk et mot dans deux vecteurs plats étiquetés par ordinal, un seul tri
puis découpe par ordinal ; lecteurs `for_each_entry` sans allocation dans
`sfxpost_v2` et `word_sfxpost`. Type de sortie inchangé
(`SfxCollectorDataV3`) — l'étape suivante serait de le passer en arène
aussi. Tests de merge et bench 2 000 docs en cours.

## 00:50 — build release : même parité, mêmes temps → l'I/O, pas le CPU

- Build release (5,2 Mo) sur l'index OPFS : 20/21 identiques + 1 ex æquo
  (filtre `path`, 382 docs au même score, fenêtre top-10 arbitraire).
- **Temps identiques au debug** : 4-14 s par requête, 4,3 s pour une
  requête sans résultat. Donc pas les asserts : une requête `content`
  matérialise tous les sidecars de tous les segments (~100 segments
  vivants, ~4 Go) et le cache de fichiers (768 Mo) se vide à chaque
  requête → ~4 Go relus depuis OPFS par requête. Le natif vit sur mmap +
  page cache. Test en cours : `?cache=3000` (nouveau `--file-cache-mb`).
- Conséquence structurelle : pour 15-20 k docs dans le navigateur, il faut
  des lectures **par plage** dans les sidecars côté requête (FST résident,
  postings/termtexts lus à l'ordinal), et/ou des sidecars plus petits.
- Fusion en arènes (natif i686, 14 segments, 650 k tokens) : boucle
  436 → 195-223 ms ; assemblage 172 → 250-290 ms à cause d'un tri global,
  remplacé par un bucketing stable par ordinal (les buckets arrivent triés
  par construction, vérifié en debug) — bench en cours.

## 01:00 — la taille de l'index est le vrai plafond du navigateur

Mesuré dans OPFS après le run complet (15 440 fichiers, ~400 Mo de texte) :
**5 904 Mo** vivants — par shard ~1 450 Mo dont `.sfx` 610-680 Mo,
`word_sfxpost` ~190, `bytemap` ~165, `sfxpost` ~155, `termtexts` ~90,
`sibling_v3` ~80 ; 1 500 à 2 500 fichiers par shard (≈ 100-150 segments,
les fusions n'ont consolidé que 24 groupes de 14). Une requête `content`
touche le `.sfx` de chaque segment : 2,5 Go, hors de portée d'un tas wasm
de 4 Go → le cache de 768 Mo se vide à chaque requête, d'où les 4-14 s.
Un `?cache=3000` ne suffira pas non plus (2,5 Go rien que les FST).

Leviers, par ordre de rendement probable :
1. **Compaction** : fusionner beaucoup plus large en fin d'indexation (le
   FST partage les suffixes entre documents ; 400 petits segments = 400 FST
   redondants). À mesurer en natif : même corpus, fusion forcée jusqu'à
   ~10 k docs/segment, taille des sidecars avant/après.
2. **Format** : 15× le texte est le problème de fond (natif compris : 50 k
   fichiers kernel → ~20 Go). Pistes : `bytemap` et `word_sfxpost` sont
   des dérivés recalculables, `sibling_v3`/`sfxpost` compressibles.
3. **Lectures par plage côté requête** : postings/termtexts lus à
   l'ordinal ; le FST lui-même a besoin d'un `&[u8]` contigu (fork
   BurntSushi), donc résident — d'où l'importance de 1 et 2.

## 01:15 — fusion en arènes : mesurée, commitée

Natif i686 release, fusion `content` de 14 segments (~650 k tokens,
~10 M postings), moyenne sur 3 shards :

| version | boucle segments (intern+postings) | assemblage | total |
|---|---|---|---|
| avant (String par clé, Vec par ordinal) | 471 ms (436) | 172 ms | 643 ms |
| arène + table d'intern + postings plats + tri global | 249 (223) | 262 | 511 |
| + remap de docs en table dense | 216 (195) | 249 | 465 |
| + bucketing stable par ordinal (plus de tri) | **180 (145)** | **226** | **406** |

−37 % ; l'assemblage reste dominé par les 650 k `String` de sortie
(`token_texts` + `tokens`) et les 650 k `Vec` de `content_postings` que
le type `SfxCollectorDataV3` impose — prochaine étape si on continue :
passer ce type en arène (touche le collector et les nœuds du DAG).
Tests : lib merge 71, pipeline v3, merge_contains, deux champs — verts.

Ajouté `ShardedHandle::compact(max_docs)` : regroupe les segments de
chaque shard sous `max_docs` et les fusionne (`merge_many`), commit avant
et après. Mesure en cours sur les 15 440 fichiers (natif) : tailles des
sidecars avant/après compaction à 10 k docs/segment.
