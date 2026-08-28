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

  /** Wait for a quiet index: commit, then every background merge running or about to start. */
  _lucivy_drain_merges(ctx: LucivyCtx): CStringPtr;

  /** Start a background compaction to segments of at most max_docs documents. */
  _lucivy_compact_async(ctx: LucivyCtx, max_docs: number): number;

  // ── Memory (3.0.0) ─────────────────────────────────────────────────

  /**
   * JSON: {"index_bytes", "in_memory", "num_docs", "warnings": [...], "shards": [...]}.
   * A browser addresses at most 4 GB; above LUCIVY_RAM_INDEX_MAX (3 GB by default)
   * the index is streamed from storage instead of held, and `warnings` says so.
   */
  _lucivy_memory_status(ctx: LucivyCtx): CStringPtr;

  /**
   * Wait for background merges to be quiet, then read the whole index into memory
   * when it fits. JSON: {"bytes", "files", "ms", "skipped"}.
   */
  _lucivy_preload(ctx: LucivyCtx): CStringPtr;

  /** Honest warnings for a query, without running it. JSON array of strings. */
  _lucivy_query_warnings(ctx: LucivyCtx, query_json: CStringPtr): CStringPtr;

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

  /** Merge BM25 stats from multiple nodes into global stats (for distributed search).
   *  stats_json_array: JSON array of ExportableStats strings: '["{\\"total_num_docs\\":...}", ...]'
   *  Returns merged JSON string ready for _lucivy_search_with_global_stats(). */
  _lucivy_merge_stats(stats_json_array: CStringPtr): CStringPtr;

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

  /** Pointer to the log ring buffer (for direct HEAPU8 access). */
  _lucivy_log_ring_ptr(): number;

  /** Size of the log ring buffer in bytes. */
  _lucivy_log_ring_size(): number;
}

/** Create and initialize the Lucivy WASM module. */
export default function createLucivy(
  moduleArg?: Partial<EmscriptenModule>,
): Promise<LucivyModule>;


// ── High-level API (js/lucivy.js, runs the module in a Web Worker) ──────

/** Startup options: each maps to a module argument read before the engine starts. */
export interface LucivyOptions {
  /** In-memory filesystem: the index lives for the session only. */
  noOpfs?: boolean;
  /** Engine diagnostics (LUCIVY_VERBOSE, V3_PROFILE). */
  verbose?: boolean;
  /** Whole-file cache budget in MB; pins it when set (default: the index size). */
  fileCacheMb?: number;
  /** Largest index held in memory, MB; above it the index is streamed (default 3072). */
  ramIndexMaxMb?: number;
  /** luciole scheduler threads (default min(cores, 8)). */
  schedulerThreads?: number;
  /** Indexer threads (default 1; the writer heap follows). */
  writerThreads?: number;
  /** Largest segment a background merge may produce (default 800). */
  maxMergedDocs?: number;
  /** Segment builds allowed at once (default 2). */
  maxBuilds?: number;
}

export interface FieldDef {
  name: string;
  type: 'text' | 'u64' | 'i64' | 'f64' | 'bool' | 'date' | 'bytes' | string;
  stored?: boolean;
  fast?: boolean;
  indexed?: boolean;
}

export interface IndexConfig {
  fields: FieldDef[];
  shards?: number;
  sfx_version?: number;
  [key: string]: unknown;
}

export interface SearchOptions {
  limit?: number;
  /** Include byte spans per field (default true). */
  highlights?: boolean;
  /** Include stored field values (default true). */
  fields?: boolean;
}

export interface SearchResult {
  docId: number;
  score: number;
  highlights?: Record<string, [number, number][]>;
  fields?: Record<string, unknown>;
}

export interface MemoryStatus {
  index_bytes: number;
  in_memory: boolean;
  num_docs: number;
  /** Empty when the index is held in memory; a sentence for the user otherwise. */
  warnings: string[];
  shards: { shard: number; bytes: number; opened: number; listed: number }[];
}

export interface PreloadResult {
  bytes: number;
  files: number;
  ms: number;
  /** True when the index is streamed and nothing was loaded. */
  skipped: boolean;
}

export class Lucivy {
  constructor(workerUrl: string | URL, options?: LucivyOptions);
  /** Resolves once the worker and the WASM module are ready. */
  readonly ready: Promise<void>;
  create(path: string, fieldsOrConfig: FieldDef[] | IndexConfig): Promise<LucivyIndex>;
  open(path: string): Promise<LucivyIndex>;
  /** Open an index persisted in OPFS in place (no copy). */
  openDirect(path: string): Promise<LucivyIndex>;
  importSnapshot(data: Uint8Array, path: string): Promise<LucivyIndex>;
  /** Terminate the worker and free every byte of WASM memory. */
  terminate(): void;
}

export class LucivyIndex {
  readonly path: string;
  add(docId: number, fields: Record<string, unknown>): Promise<void>;
  addMany(docs: Array<{ docId: number } & Record<string, unknown>>): Promise<void>;
  remove(docId: number): Promise<void>;
  update(docId: number, fields: Record<string, unknown>): Promise<void>;
  commit(options?: { sync?: boolean }): Promise<void>;
  /** Not supported on a sharded index: rejects with the reason. */
  rollback(): Promise<never>;
  /** Merge down to segments of at most maxDocs documents. Not for the browser's own indexes: small segments are what fills the threads. */
  compact(maxDocs?: number): Promise<number>;
  /** Commit, then wait until no background merge is running or about to start. */
  drainMerges(): Promise<void>;
  /** Read the whole index into memory when it fits (after merges are quiet). */
  preload(): Promise<PreloadResult>;
  memoryStatus(): Promise<MemoryStatus>;
  search(query: string | object, options?: SearchOptions): Promise<SearchResult[]>;
  searchFiltered(query: string | object, allowedIds: number[], options?: SearchOptions): Promise<SearchResult[]>;
  numDocs(): Promise<number>;
  schema(): Promise<IndexConfig>;
  exportSnapshot(): Promise<Uint8Array>;
  /** What this index holds per shard — hand it to whoever exports the delta. */
  shardVersions(): Promise<ShardVersion[]>;
  /** A LUCIDS delta holding only the shards that moved since `clientVersions`. */
  exportShardedDelta(clientVersions?: ShardVersion[]): Promise<Uint8Array>;
  /** Apply a LUCIDS delta onto this index, in place. */
  applyShardedDelta(data: Uint8Array | ArrayBuffer): Promise<boolean>;
  close(): Promise<void>;
  destroy(): Promise<void>;
}

/** One shard's identity and the version of it a client holds. */
export interface ShardVersion {
  shard_id: number;
  version: string;
  /** The segments that version is made of — what the exporter diffs against. */
  segment_ids: string[];
}
