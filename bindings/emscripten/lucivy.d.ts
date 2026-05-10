/**
 * Lucivy WASM (Emscripten) — TypeScript declarations.
 *
 * Usage:
 *   const Module = await createLucivy();
 *   const ctx = Module.ccall("lucivy_create", "number", ["string", "string", "number"],
 *     ["/index", JSON.stringify({fields:[{name:"body",type:"text",stored:true}]}), 1]);
 */

// ── Query JSON format ──────────────────────────────────────────────
//
// All queries are passed as JSON strings to lucivy_search / lucivy_search_filtered.
//
// Query types (all substring queries are cross-token):
//
//   {"type":"contains","field":"body","value":"lock"}
//     Substring match. Finds "lock" inside "unlock", "locking", etc.
//
//   {"type":"contains","field":"body","value":"lock","distance":1}
//     Fuzzy substring (Levenshtein). Finds "lock", "look", "lack", etc.
//
//   {"type":"contains","field":"body","value":"lock.*init","regex":true}
//     Regex substring. Cross-token regex matching.
//
//   {"type":"startsWith","field":"body","value":"lock"}
//     Token prefix. Finds tokens starting with "lock" (lock, locks, locking...).
//
//   {"type":"contains_split","field":"body","value":"struct device"}
//     Split on whitespace, each word as contains, combined with boolean OR.
//
//   {"type":"term","field":"body","value":"lock"}
//     Exact whole-token match.
//
//   {"type":"fuzzy","field":"body","value":"schdule","distance":1}
//     Alias for contains + distance.
//
//   {"type":"phrase","field":"body","value":"mutex lock"}
//     Adjacent tokens in order.
//
//   {"type":"regex","field":"body","pattern":"sched[a-z]+"}
//     Regex on individual tokens.
//
//   {"type":"boolean","must":[...],"should":[...],"must_not":[...]}
//     Boolean combination of sub-queries.
//
//   {"type":"disjunction_max","queries":[...],"tie_breaker":0.1}
//     Best-score from sub-queries with tie-breaker.
//
//   {"type":"more_like_this","field":"body","value":"sample text",
//    "min_doc_frequency":1,"min_term_frequency":1,"min_word_length":3}
//     TF-IDF similarity search.
//
// Filtering (in query JSON):
//   "filters": [
//     {"field":"category","op":"eq","value":"kernel"},
//     {"field":"score","op":"gte","value":0.5},
//     {"field":"status","op":"in","value":["active","review"]}
//   ]
//   Ops: eq, ne, lt, lte, gt, gte, in, not_in, between, starts_with, contains
//   Composite: must, should, must_not with nested "clauses"

/** Opaque pointer to a LucivyContext (WASM heap address). */
type LucivyCtx = number;

/** Returned C string pointer — read with Module.UTF8ToString(ptr). */
type CStringPtr = number;

export interface LucivyModule extends EmscriptenModule {
  // ── Lifecycle ──────────────────────────────────────────────────────

  /** Create a new index. Returns context pointer. */
  _lucivy_create(
    path: CStringPtr,
    config_json: CStringPtr,
    shards: number,
  ): LucivyCtx;

  /** Open an existing index. Returns context pointer. */
  _lucivy_open(path: CStringPtr): LucivyCtx;

  /** Streaming open: begin (creates context, no shards loaded yet). */
  _lucivy_open_begin(path: CStringPtr): LucivyCtx;

  /** Streaming open: import a .luce snapshot file into the context. */
  _lucivy_import_file(
    ctx: LucivyCtx,
    filename: CStringPtr,
    data: number,
    len: number,
  ): CStringPtr;

  /** Streaming open: finalize after all files imported. Returns final ctx. */
  _lucivy_open_finish(ctx: LucivyCtx): LucivyCtx;

  /** Close the index (flush + release locks). */
  _lucivy_close(ctx: LucivyCtx): CStringPtr;

  /** Destroy the context and free memory. */
  _lucivy_destroy(ctx: LucivyCtx): void;

  // ── Document operations ────────────────────────────────────────────

  /** Add a document. fields_json: {"body":"text","score":3.14} */
  _lucivy_add(
    ctx: LucivyCtx,
    doc_id_lo: number,
    doc_id_hi: number,
    fields_json: CStringPtr,
  ): CStringPtr;

  /** Add multiple documents. docs_json: [{"_node_id":1,"body":"..."},..] */
  _lucivy_add_many(ctx: LucivyCtx, docs_json: CStringPtr): CStringPtr;

  /** Delete a document by _node_id. */
  _lucivy_remove(ctx: LucivyCtx, doc_id: number): CStringPtr;

  /** Update a document (delete + re-add). */
  _lucivy_update(
    ctx: LucivyCtx,
    doc_id_lo: number,
    doc_id_hi: number,
    fields_json: CStringPtr,
  ): CStringPtr;

  // ── Transaction ────────────────────────────────────────────────────

  /** Commit pending writes (synchronous). */
  _lucivy_commit(ctx: LucivyCtx): CStringPtr;

  /** Start async commit (returns immediately). */
  _lucivy_commit_async(ctx: LucivyCtx): number;

  /** Check async commit status. Returns 1 if done. */
  _lucivy_commit_status_ptr(ctx: LucivyCtx): number;

  /** Finish async commit (blocks until done). */
  _lucivy_commit_finish(ctx: LucivyCtx): CStringPtr;

  /** Drain background merges. */
  _lucivy_drain_merges(ctx: LucivyCtx): CStringPtr;

  // ── Search ─────────────────────────────────────────────────────────

  /**
   * Search the index. Returns JSON array of results.
   * @param query_json - Query JSON string (see query types above).
   * @param limit - Max results.
   * @param highlights - 1 to include highlight byte offsets, 0 to skip.
   * @param include_fields - 1 to include stored field values, 0 to skip.
   */
  _lucivy_search(
    ctx: LucivyCtx,
    query_json: CStringPtr,
    limit: number,
    highlights: number,
    include_fields: number,
  ): CStringPtr;

  /**
   * Search with pre-filter by _node_id.
   * @param allowed_ids_json - JSON array of allowed _node_id values: "[1,2,3]"
   */
  _lucivy_search_filtered(
    ctx: LucivyCtx,
    query_json: CStringPtr,
    limit: number,
    highlights: number,
    include_fields: number,
    allowed_ids_json: CStringPtr,
  ): CStringPtr;

  /** Search with pre-computed global BM25 stats (for distributed search). */
  _lucivy_search_with_global_stats(
    ctx: LucivyCtx,
    query_json: CStringPtr,
    limit: number,
    stats_json: CStringPtr,
  ): CStringPtr;

  // ── Info ────────────────────────────────────────────────────────────

  /** Number of documents in the index. */
  _lucivy_num_docs(ctx: LucivyCtx): number;

  /** Schema as JSON string. */
  _lucivy_schema_json(ctx: LucivyCtx): CStringPtr;

  /** Shard versions as JSON. */
  _lucivy_shard_versions(ctx: LucivyCtx): CStringPtr;

  // ── Snapshot / Delta ───────────────────────────────────────────────

  /** Export full snapshot (.luce). Returns JSON with file list. */
  _lucivy_export_snapshot(ctx: LucivyCtx, out_dir: CStringPtr): CStringPtr;

  /** Import snapshot from directory. */
  _lucivy_import_snapshot(ctx: LucivyCtx, snapshot_dir: CStringPtr): CStringPtr;

  /** Export sharded delta (.lucids). */
  _lucivy_export_sharded_delta(
    ctx: LucivyCtx,
    out_dir: CStringPtr,
    base_versions_json: CStringPtr,
  ): CStringPtr;

  /** Apply sharded delta. */
  _lucivy_apply_sharded_delta(
    ctx: LucivyCtx,
    delta_dir: CStringPtr,
  ): CStringPtr;

  /** Export BM25 stats for distributed search. */
  _lucivy_export_stats(ctx: LucivyCtx, query_json: CStringPtr): CStringPtr;

  // ── Diagnostics ────────────────────────────────────────────────────

  /** Read ring buffer logs. */
  _lucivy_read_logs(): CStringPtr;

  /** Configure logging. config_json: {"log_level":"debug"} */
  _lucivy_configure(config_json: CStringPtr): CStringPtr;

  /** Dump scheduler DAG as Mermaid. */
  _lucivy_dump_mermaid(ctx: LucivyCtx): CStringPtr;

  /** Dump scheduler state. */
  _lucivy_dump_state(ctx: LucivyCtx): CStringPtr;

  /** Dump wait graph as Mermaid. */
  _lucivy_dump_wait_graph(ctx: LucivyCtx): CStringPtr;

  /** Dump wait graph as text. */
  _lucivy_dump_wait_graph_text(ctx: LucivyCtx): CStringPtr;
}

/** Create and initialize the Lucivy WASM module. */
export default function createLucivy(
  moduleArg?: Partial<EmscriptenModule>,
): Promise<LucivyModule>;
