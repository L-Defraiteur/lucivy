# Architecture du projet — état au 24 août 2026

Rappel global, à jour des changements récents. Complète (ne remplace pas) le
knowledge dump algorithmique `docs/22-aout-2026-19h47/09` qui détaille les
algorithmes de requête.

## Les couches

```
bindings : Python (pyo3) · Node (napi) · C++ (cxx) · bridge rag3db (cxx) · emscripten (extern C)
   │              tous : JSON QueryConfig ⇄ ShardedHandle
lucivy_core : ShardedHandle · LucivyHandle · query (compat layer) · warnings
              search_dag (prescan parallèle) · blob_directory · sync (LUCE/LUCID/LUCIDS)
   │
ld-lucivy : le moteur — index/segments/merge · SFX v3 (suffix FST) · query v3
            (contains/fuzzy/regex) · BM25 · docstore/fastfields (héritage tantivy)
   │
lucistore : infra partagée (« and friends » = sparse_vector) — trait BlobStore
            · blob_cache · version/snapshot/delta/delta_sharded · sync_server
luciole : acteurs + scheduler global (pool persistant) + DAG. WASM-safe, crate séparé.
```

## Le moteur SFX v3 (défaut depuis le 23 août)

Par champ texte et par segment, 8 sidecars : `.sfx` (FST : partitions 0x00
débuts de chunk / 0x01 suffixes / 0x02 word-stripped ; listes multi-parents
u32), `.sfxpost`, `.termtexts` (TTX3 + section STATS = plus long mot),
`.posmap`, `.bytemap`, `.word_sfxpost` (WSP2), `.word_pos_map`, `.sibling_v3`.
Un `meta.json` sans `sfx_version` = index v2 (le champ est toujours écrit
maintenant) ; index mixtes v2/v3 supportés, détection par magic, signalés par
`query_warnings`.

Requêtes : contains strict/relaxed (chaînes de chunks + pipeline word ;
`verify_literal` revérifie contre le texte ; `verify_boundaries` pour
anchor/exact), fuzzy (trigrammes → régions → fenêtre → `fuzzy_spans`), regex
(littéraux prouvés + fenêtre bornée sinon document entier). Spans = highlights,
exactes à l'octet (vérités terrain).

## Handles et cycle de vie

- `LucivyHandle` : un index. `create/open(dir)`, writer tantivy-style, policy
  de merge plafonnée à 10k docs/segment (entrée ET sortie). `close()` =
  commit + `drain_merges` + libération.
- `ShardedHandle` : N shards + acteurs (readers → routeur → shards) sur le
  scheduler global. `add_document(doc, id)` **estampille `_node_id`** ;
  `add_document_json(id, fields)` champs par nom ; `search`,
  `search_filtered(allowed_ids)`, `search_with_docs` (highlights par champ),
  `delete_by_node_id`, `commit`, `query_warnings`, `export/apply_sharded_delta`,
  `export_stats`/`search_with_global_stats` (distribué). **`close()` rend le
  handle inerte** (drain + arrêt des pools d'acteurs) ; `drop_index()` détruit
  tout (shards + fichiers racine).
- `SchemaConfig` : clés inconnues refusées (serde nomme les valides),
  `validate()` sur les valeurs ; `from_stored_json` tolérant pour rouvrir les
  configs écrites par d'autres versions.

## Persistance — trois topologies

| Topologie | Storage | Usage |
|---|---|---|
| Disque | `FsShardStorage` (MmapDirectory) | défaut natif |
| RAM | `RamShardStorage` | tests |
| **ACID** | `BlobShardStorage<S: BlobStore>` → `BlobDirectory` | blobs (DB/S3) = source de vérité, cache local mmap jetable |

ACID : Eager (défaut, tout à l'ouverture) ou **Lazy**
(`.with_load_mode(BlobLoadMode::Lazy)`) — rien à l'ouverture, sondes de footer
servies par `BlobStore::load_range` (≤ 64 Ko), fichier matérialisé au 4e accès
distant. `blob_len`/`load_range` : méthodes à défaut (`None` = repli complet).
`impl BlobStore for Arc<T>` permet `Arc<dyn BlobStore>` direct.

Sync incrémental (lucistore) : LUCE (snapshot), LUCID (delta 1 index), LUCIDS
(N shards), `SyncServer` (historique de versions). Réparés le 23 août : ids
normalisés (tirets), fichiers `.del` transportés, writer recréé après apply —
delta 293 Ko au lieu de 379 Mo. **Topologie distincte** de l'ACID : copie
locale durable tenue à jour par deltas (le futur WASM offline).

## Règles WASM (inchangées, critiques)

Jamais de `thread::spawn` (tout via luciole) ; I/O différée (`FsWriter` RAM
jusqu'au terminate) ; pas d'I/O dans un handler d'acteur ; heap writer 15 Mo ;
`MAXIMUM_MEMORY=4GB`. Le binding emscripten builde (24 août) mais n'a pas
encore tourné ; rag3weaver compile `lucivy_core` en Rust dans son propre wasm
(chemin recommandé).

## Consommateur principal : rag3weaver

Migration FTS en cours chez eux (branche `fts-lucivy-v3`, submodule épinglé) :
`CALL *_LUCIVY_INDEX` (extension C++) → `ShardedHandle` direct sur
`BlobShardStorage` + leur `CypherBlobStore`. Dialogue par docs dans
`rag3db/extension/rag3weaver/docs/23-aout-2026-20h33/` (04→10).
