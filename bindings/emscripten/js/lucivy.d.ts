/** Field definition for creating an index. */
export interface FieldDef {
    name: string;
    type: 'text' | 'u64' | 'i64' | 'f64';
}

/** Search query — either a plain string or a structured query object. */
export type SearchQuery = string | {
    type: 'contains' | 'contains_split' | 'term' | 'boolean';
    field?: string;
    value?: string;
    should?: SearchQuery[];
    must?: SearchQuery[];
    must_not?: SearchQuery[];
};

/** Search options. */
export interface SearchOptions {
    limit?: number;
    highlights?: boolean;
}

/** A single search result. */
export interface SearchResult {
    docId: number;
    score: number;
    highlights?: Record<string, [number, number][]>;
}

/** Main-thread Promise API for lucivy-emscripten. */
export declare class Lucivy {
    /** Resolves when the WASM module is loaded and ready. */
    readonly ready: Promise<boolean>;

    constructor(workerUrl: string);

    /** Create a new index at the given path with the specified fields. */
    create(path: string, fields: FieldDef[], stemmer?: string): Promise<LucivyIndex>;

    /** Open an existing index from OPFS. */
    open(path: string): Promise<LucivyIndex>;

    /** Terminate the worker. */
    terminate(): void;
}

/** Handle to an open lucivy index. All operations go through the worker. */
export declare class LucivyIndex {
    readonly path: string;

    /** Add a document with the given ID and field values. */
    add(docId: number, fields: Record<string, string | number>): Promise<boolean>;

    /** Add multiple documents at once. Each doc must have a `docId` key. */
    addMany(docs: Array<Record<string, string | number> & { docId: number }>): Promise<boolean>;

    /** Remove a document by ID. */
    remove(docId: number): Promise<boolean>;

    /** Update a document (remove + add). */
    update(docId: number, fields: Record<string, string | number>): Promise<boolean>;

    /** Commit pending changes and sync to OPFS. */
    commit(): Promise<{ numDocs: number }>;

    /** Rollback uncommitted changes. */
    rollback(): Promise<boolean>;

    /** Search the index. */
    search(query: SearchQuery, options?: SearchOptions): Promise<SearchResult[]>;

    /** Search with an allowed document ID filter. */
    searchFiltered(query: SearchQuery, allowedIds: number[], options?: SearchOptions): Promise<SearchResult[]>;

    /** Get the number of indexed documents. */
    numDocs(): Promise<number>;

    /** Get the index schema. */
    schema(): Promise<FieldDef[] | null>;

    /** Close the index (keep OPFS files). */
    close(): Promise<boolean>;

    /** Close the index and delete OPFS files. */
    destroy(): Promise<boolean>;
}
