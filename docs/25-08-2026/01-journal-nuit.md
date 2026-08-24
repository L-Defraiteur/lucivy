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
