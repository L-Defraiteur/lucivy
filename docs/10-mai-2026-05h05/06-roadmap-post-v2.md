# Roadmap post-v2

## v2.1 — Auto doc_id

- Allocateur BTree de ranges libres (design dans `05-design-auto-doc-id.md`)
- `add()` retourne l'ID, doc_id optionnel dans tous les bindings
- Persisté dans `_id_alloc.json`, compatible snapshot/delta
- Distribué : pour plus tard (préfixe shard_id ou ranges pré-assignés)

## v2.2 — SFX separators + strict_separators propre

- Stocker les séparateurs réels dans le SFX (actuellement perdus à l'indexation)
- Chaque entrée SFX sait quel séparateur la précède (`_`, `-`, `.`, espace, etc.)
- Permet des queries `strict_separators=true` fiables (actuellement approximatif via GapMap)
- Use case : `mutex_lock` matche `mutex_lock` mais pas `mutexclock`

## v2.3 — Fuzzy exact via separators

- Avec les séparateurs dans le SFX, le fuzzy peut distinguer :
  - `"lok"` d=1 → `"lock"` (substring dans un token) ✓
  - `"lok"` d=1 → `"lo_k"` (cross-token avec séparateur) → plus précis
- Le pigeonhole threshold peut être affiné : trigrammes cross-séparateur comptés différemment
- Scoring fuzzy plus précis (pénalité pour match cross-séparateur)

## v2.4 — Tokenizer longueurs arbitraires

- Plus besoin du CamelCaseSplitFilter obligatoire
- Le SFX gère nativement les tokens de longueur arbitraire
- Tokenizer configurable par field (pas juste `raw_code`)
- Support de tokenizers custom (stemming, lemmatisation) dans le SFX
- Permet d'indexer du code avec des identifiants longs sans les splitter

## v3 — Distribué

- 1 shard par machine, coordinateur de recherche
- Auto doc_id distribué (préfixe shard_id ou ranges pré-assignés par nœud)
- `ExportableStats` pour BM25 cross-nœuds (déjà implémenté)
- Delta sync réseau (LUCIDS over HTTP/gRPC)
- Rag3weaver cloud : multi-backend (Postgres, S3) via BlobDirectory

## v3.1 — Normalisation agentique

- `LlmNormalizeNode` dans rag3weaver dataflow (design dans `rag3weaver/docs/10-mai-2026-04h30/`)
- Basé sur Rig (providers Gemini, Anthropic, OpenAI, Ollama)
- Pipeline : xlsx_parse → LlmNormalize → InsertRecord → Chunk → Embed → Flush
- Tool calling pour enrichissement externe (API, DB lookups)
