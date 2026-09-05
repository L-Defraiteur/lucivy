"""Bring-your-own-storage: an index whose files live in a Python blob store.

The store is a plain Python object (``load`` / ``save`` / ``delete`` /
``exists`` / ``list``, optionally ``blob_len`` / ``load_range``). Its methods
are called from lucivy's own threads, so every binding method that may reach
the store releases the GIL while it waits — the tests below would hang, not
fail, without that.
"""

import faulthandler
import os
import shutil
import sqlite3
import tempfile
import threading

import pytest

import lucivy


FIELDS = [
    {"name": "title", "type": "text", "stored": True},
    {"name": "body", "type": "text", "stored": True},
]

DOCS = [
    {"doc_id": 1, "title": "Introduction to Python", "body": "Python is a versatile programming language used for web development and data science."},
    {"doc_id": 2, "title": "Rust Programming", "body": "Rust provides memory safety without garbage collection through its ownership system."},
    {"doc_id": 3, "title": "JavaScript Basics", "body": "JavaScript is the language of the web, running in every browser natively."},
    {"doc_id": 4, "title": "Machine Learning with Python", "body": "Deep learning frameworks like PyTorch and TensorFlow make neural networks accessible."},
    {"doc_id": 5, "title": "Database Design", "body": "Relational databases use SQL for querying structured data efficiently."},
    {"doc_id": 6, "title": "Graph Databases", "body": "Graph databases like Neo4j store relationships natively, making traversals fast."},
    {"doc_id": 7, "title": "Full-Text Search", "body": "Inverted indexes power full-text search engines like Lucene, Tantivy and Elasticsearch."},
    {"doc_id": 8, "title": "Web Development", "body": "Modern web development combines frontend frameworks with backend APIs and microservices."},
]

QUERIES = [
    "python",
    {"type": "contains", "field": "body", "value": "garbage collection"},
    {"type": "contains", "field": "body", "value": "memry", "distance": 1},
    {"type": "contains", "field": "body", "value": "program.*language", "regex": True},
    {"type": "startsWith", "field": "title", "value": "data"},
    {"type": "parse", "value": "web AND development", "fields": ["title", "body"]},
]


def hits(idx, query):
    return [(r.doc_id, round(r.score, 4)) for r in idx.search(query, limit=20)]


# ─── Stores ──────────────────────────────────────────────────────────────────


class DictBlobStore:
    """The smallest store there is: ``(index_name, file_name) -> bytes``.

    Records every call with the thread it ran on, so a test can prove the
    calls came from lucivy's threads and not from the test's own.
    """

    def __init__(self):
        self.blobs = {}
        self.lock = threading.Lock()
        self.calls = []  # (method, index_name, file_name, thread ident)

    def _note(self, method, index_name, file_name=""):
        self.calls.append((method, index_name, file_name, threading.get_ident()))

    def load(self, index_name, file_name):
        self._note("load", index_name, file_name)
        with self.lock:
            try:
                return self.blobs[(index_name, file_name)]
            except KeyError:
                raise FileNotFoundError(f"{index_name}/{file_name}")

    def save(self, index_name, file_name, data):
        self._note("save", index_name, file_name)
        with self.lock:
            self.blobs[(index_name, file_name)] = bytes(data)

    def delete(self, index_name, file_name):
        self._note("delete", index_name, file_name)
        with self.lock:
            self.blobs.pop((index_name, file_name), None)

    def exists(self, index_name, file_name):
        self._note("exists", index_name, file_name)
        with self.lock:
            return (index_name, file_name) in self.blobs

    def list(self, index_name):
        self._note("list", index_name)
        with self.lock:
            return [f for (i, f) in self.blobs if i == index_name]

    def namespaces(self):
        with self.lock:
            return sorted({i for (i, _) in self.blobs})


class LazyDictBlobStore(DictBlobStore):
    """DictBlobStore plus the optional pair lazy loading needs, counted."""

    def blob_len(self, index_name, file_name):
        self._note("blob_len", index_name, file_name)
        with self.lock:
            data = self.blobs.get((index_name, file_name))
        return None if data is None else len(data)

    def load_range(self, index_name, file_name, offset, length):
        self._note("load_range", index_name, file_name)
        with self.lock:
            data = self.blobs.get((index_name, file_name))
        return None if data is None else data[offset:offset + length]

    def count(self, method):
        return sum(1 for c in self.calls if c[0] == method)

    def files(self, method):
        return {(c[1], c[2]) for c in self.calls if c[0] == method}


class SqliteBlobStore:
    """Blobs in a SQLite table; ``save`` and ``delete`` are transactions.

    One connection shared by every thread, serialized by a lock:
    ``check_same_thread=False`` because the calls come from lucivy's threads.
    """

    def __init__(self, path):
        self.lock = threading.Lock()
        self.conn = sqlite3.connect(path, check_same_thread=False)
        with self.conn:
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS blobs ("
                " index_name TEXT NOT NULL, file_name TEXT NOT NULL, data BLOB NOT NULL,"
                " PRIMARY KEY (index_name, file_name))"
            )

    def load(self, index_name, file_name):
        with self.lock:
            row = self.conn.execute(
                "SELECT data FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name),
            ).fetchone()
        if row is None:
            raise FileNotFoundError(f"{index_name}/{file_name}")
        return row[0]

    def save(self, index_name, file_name, data):
        with self.lock, self.conn:
            self.conn.execute(
                "INSERT OR REPLACE INTO blobs (index_name, file_name, data) VALUES (?, ?, ?)",
                (index_name, file_name, sqlite3.Binary(data)),
            )

    def delete(self, index_name, file_name):
        with self.lock, self.conn:
            self.conn.execute(
                "DELETE FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name),
            )

    def exists(self, index_name, file_name):
        with self.lock:
            row = self.conn.execute(
                "SELECT 1 FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name),
            ).fetchone()
        return row is not None

    def list(self, index_name):
        with self.lock:
            rows = self.conn.execute(
                "SELECT file_name FROM blobs WHERE index_name = ?", (index_name,)
            ).fetchall()
        return [r[0] for r in rows]

    # Optional: lets lazy=True size files and probe them without a download.
    def blob_len(self, index_name, file_name):
        with self.lock:
            row = self.conn.execute(
                "SELECT length(data) FROM blobs WHERE index_name = ? AND file_name = ?",
                (index_name, file_name),
            ).fetchone()
        return None if row is None else row[0]

    def load_range(self, index_name, file_name, offset, length):
        with self.lock:
            row = self.conn.execute(
                "SELECT substr(data, ?, ?) FROM blobs WHERE index_name = ? AND file_name = ?",
                (offset + 1, length, index_name, file_name),
            ).fetchone()
        return None if row is None else row[0]

    def close(self):
        self.conn.close()


class FailingBlobStore(DictBlobStore):
    """Works until ``arm()``; then every ``save`` raises."""

    def __init__(self):
        super().__init__()
        self.armed = False

    def arm(self):
        self.armed = True

    def save(self, index_name, file_name, data):
        if self.armed:
            raise RuntimeError("disk on fire")
        super().save(index_name, file_name, data)


# ─── Fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp(prefix="lucivy_blob_")
    yield d
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def cache_dir(tmp_dir):
    return os.path.join(tmp_dir, "cache")


# ─── Basics ──────────────────────────────────────────────────────────────────


class TestDictStore:
    def test_create_commit_search(self, cache_dir):
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(store, "basic", FIELDS, cache_dir=cache_dir)
        idx.add_many(DOCS)
        idx.commit()
        assert idx.num_docs == len(DOCS)
        assert idx.path is None
        assert idx.blob_index_name == "basic"
        assert "<blob:basic>" in repr(idx)
        results = idx.search("ownership", highlights=True, fields=True)
        assert [r.doc_id for r in results] == [2]
        assert "body" in results[0].highlights
        assert results[0].fields["title"] == "Rust Programming"
        idx.close()
        # Shard blobs live under the prefixed namespace, root files under the bare name.
        assert store.namespaces() == ["Lucivy_basic/shard_0", "basic"]
        assert "_shard_config.json" in store.list("basic")
        assert "meta.json" in store.list("Lucivy_basic/shard_0")

    def test_reopen_with_fresh_cache_gives_same_answers(self, tmp_dir):
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(
            store, "reopen", FIELDS, shards=2, cache_dir=os.path.join(tmp_dir, "cache_a"))
        idx.add_many(DOCS)
        idx.commit()
        expected = {i: hits(idx, q) for i, q in enumerate(QUERIES)}
        idx.close()
        del idx
        shutil.rmtree(os.path.join(tmp_dir, "cache_a"), ignore_errors=True)

        # Another machine: same store, nothing on disk.
        again = lucivy.Index.open_with_blob_store(
            store, "reopen", cache_dir=os.path.join(tmp_dir, "cache_b"))
        assert again.num_shards == 2
        assert again.num_docs == len(DOCS)
        for i, q in enumerate(QUERIES):
            assert hits(again, q) == expected[i], f"query {q!r} differs after reopen"
        # And it keeps writing.
        again.add(doc_id=100, title="Fresh", body="A fresh document after reopen")
        again.commit()
        assert [r.doc_id for r in again.search("fresh")] == [100]
        again.close()

    def test_default_cache_dir(self):
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(store, "defcache", FIELDS)
        idx.add_many(DOCS[:2])
        idx.commit()
        assert idx.num_docs == 2
        idx.close()

    def test_exports_are_refused(self, cache_dir):
        """Snapshot and delta export read from a directory: a blob-backed index has none."""
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(store, "noexport", FIELDS, cache_dir=cache_dir)
        idx.add_many(DOCS[:2])
        idx.commit()
        with pytest.raises(ValueError, match="blob store"):
            idx.export_snapshot()
        with pytest.raises(ValueError, match="blob store"):
            idx.export_sharded_delta(idx.shard_versions)
        with pytest.raises(ValueError, match="blob store"):
            idx.apply_sharded_delta(b"LUCIDS")
        idx.close()

    def test_bytearray_and_memoryview_are_accepted(self, cache_dir):
        class ViewStore(DictBlobStore):
            def load(self, index_name, file_name):
                data = super().load(index_name, file_name)
                return memoryview(data) if file_name.endswith(".json") else bytearray(data)

        store = ViewStore()
        idx = lucivy.Index.create_with_blob_store(store, "views", FIELDS, cache_dir=cache_dir)
        idx.add_many(DOCS)
        idx.commit()
        idx.close()
        again = lucivy.Index.open_with_blob_store(store, "views", cache_dir=cache_dir)
        assert again.num_docs == len(DOCS)
        assert [r.doc_id for r in again.search("ownership")] == [2]
        again.close()

    def test_missing_method_is_rejected(self, cache_dir):
        class Incomplete:
            def load(self, i, f): ...
            def save(self, i, f, d): ...
            def delete(self, i, f): ...
            def exists(self, i, f): ...

        with pytest.raises(TypeError, match="list"):
            lucivy.Index.create_with_blob_store(Incomplete(), "bad", FIELDS, cache_dir=cache_dir)

    def test_open_missing_index_raises(self, cache_dir):
        """A KeyError / FileNotFoundError from the store is 'not found', reported, not a hang."""
        with pytest.raises(ValueError, match="_shard_config.json"):
            lucivy.Index.open_with_blob_store(DictBlobStore(), "nothing", cache_dir=cache_dir)


# ─── SQLite: the transactional point ─────────────────────────────────────────


class TestSqliteStore:
    def test_second_open_from_the_same_database(self, tmp_dir):
        db = os.path.join(tmp_dir, "blobs.sqlite")
        writer = SqliteBlobStore(db)
        idx = lucivy.Index.create_with_blob_store(
            writer, "catalog", FIELDS, shards=2, cache_dir=os.path.join(tmp_dir, "cache_w"))
        idx.add_many(DOCS)
        idx.commit()
        expected = {i: hits(idx, q) for i, q in enumerate(QUERIES)}

        # A second process's view: its own connection to the same file.
        reader = SqliteBlobStore(db)
        other = lucivy.Index.open_with_blob_store(
            reader, "catalog", cache_dir=os.path.join(tmp_dir, "cache_r"))
        assert other.num_docs == len(DOCS)
        for i, q in enumerate(QUERIES):
            assert hits(other, q) == expected[i], f"query {q!r} differs in the second index"
        other.close()
        idx.close()

        rows = reader.conn.execute("SELECT count(*) FROM blobs").fetchone()[0]
        assert rows > 0
        reader.close()
        writer.close()

    def test_lazy_open_with_sqlite(self, tmp_dir):
        db = os.path.join(tmp_dir, "lazy.sqlite")
        store = SqliteBlobStore(db)
        idx = lucivy.Index.create_with_blob_store(
            store, "lazy_sql", FIELDS, cache_dir=os.path.join(tmp_dir, "cache_e"))
        idx.add_many(DOCS)
        idx.commit()
        expected = hits(idx, QUERIES[1])
        idx.close()

        lazy = lucivy.Index.open_with_blob_store(
            store, "lazy_sql", cache_dir=os.path.join(tmp_dir, "cache_l"), lazy=True)
        assert hits(lazy, QUERIES[1]) == expected
        lazy.close()
        store.close()


# ─── Lazy loading ────────────────────────────────────────────────────────────


class TestLazy:
    def test_lazy_open_downloads_only_metadata(self, tmp_dir):
        """lazy=True with blob_len + load_range. What the engine guarantees
        (same as the core test): an open pulls well under half of the index
        — metadata files, sized by blob_len — and the large index structures
        (the suffix FSTs) are only ever probed through load_range, never
        downloaded whole with load, not even by a query that uses them."""
        store = LazyDictBlobStore()
        # The per-segment layout: the contract above is stated for it. A shard
        # dictionary's files (`dict-*.sfx`, `.termtexts`) are read whole when
        # the index opens, so a lazy open of a dictionary index pulls them at
        # open and stays lazy on the segments only.
        idx = lucivy.Index.create_with_blob_store(
            store, "lazy", FIELDS, cache_dir=os.path.join(tmp_dir, "cache_e"), shared_dictionary=False)
        idx.add_many(DOCS)
        idx.commit()
        expected = hits(idx, QUERIES[1])
        idx.close()
        del idx

        sizes = {k: len(v) for k, v in store.blobs.items()}
        total = sum(sizes.values())
        large = {k for k, n in sizes.items() if n >= 4096}
        assert large, "the fixture must produce at least one large file"
        assert all(f.endswith((".sfx", ".bytemap")) for (_, f) in large), sorted(large)

        store.calls.clear()
        lazy = lucivy.Index.open_with_blob_store(
            store, "lazy", cache_dir=os.path.join(tmp_dir, "cache_l"), lazy=True)
        assert store.count("blob_len") > 0, "lazy open sizes files through blob_len"
        pulled = sum(sizes[k] for k in store.files("load"))
        assert pulled < total / 2, f"lazy open pulled {pulled} of {total} bytes"
        assert not (store.files("load") & large), "lazy open downloaded a large file"
        # Opening a segment probes headers and footers: small ranged reads.
        assert store.count("load_range") > 0

        store.calls.clear()
        assert hits(lazy, QUERIES[1]) == expected
        assert store.count("load_range") > 0, "probes of the index structures go through load_range"
        assert not (store.files("load") & large), "a query downloaded a suffix FST whole"
        all_files = {k for k in store.blobs if not k[1].endswith(".lock")}
        assert store.files("load") < all_files, "a query must not download the whole index"
        lazy.close()

    def test_lazy_needs_blob_len(self, tmp_dir):
        """lazy=True is refused up front on a store without blob_len: the
        engine cannot size the files, and its unknown-size path deadlocks
        on the first segment open (lock taken twice) instead of degrading."""
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(
            store, "lazy_plain", FIELDS, cache_dir=os.path.join(tmp_dir, "cache_e"))
        idx.add_many(DOCS)
        idx.commit()
        idx.close()
        with pytest.raises(ValueError, match="blob_len"):
            lucivy.Index.open_with_blob_store(
                store, "lazy_plain", cache_dir=os.path.join(tmp_dir, "cache_l"), lazy=True)
        with pytest.raises(ValueError, match="blob_len"):
            lucivy.Index.create_with_blob_store(
                store, "lazy_plain2", FIELDS, cache_dir=os.path.join(tmp_dir, "cache_l"), lazy=True)
        # Without lazy the same store is fine.
        again = lucivy.Index.open_with_blob_store(
            store, "lazy_plain", cache_dir=os.path.join(tmp_dir, "cache_l"))
        assert again.num_docs == len(DOCS)
        again.close()


# ─── Threads and the GIL ─────────────────────────────────────────────────────


class TestThreading:
    def test_store_is_called_from_lucivy_threads_while_the_caller_waits(self, cache_dir):
        """Many commits through a Python store. Each commit waits for lucivy's
        threads, and those threads need the GIL to call the store: this test
        cannot complete unless the binding releases the GIL while waiting.
        A regression shows up as a hang, so a watchdog aborts the process
        with a traceback of every thread instead of blocking the suite."""
        faulthandler.dump_traceback_later(120, exit=True)
        try:
            store = DictBlobStore()
            idx = lucivy.Index.create_with_blob_store(store, "threads", FIELDS, shards=2, cache_dir=cache_dir)
            for i in range(30):
                idx.add(doc_id=i, title=f"Commit {i}", body=f"document number {i} written by commit {i}")
                idx.commit()
                if i % 5 == 4:
                    idx.delete(i - 1)
                    idx.commit()
            idx.wait_merges_quiet()
            idx.compact()
            assert idx.num_docs == 30 - 6
            assert len(idx.search("document")) >= 10
            idx.close()
        finally:
            faulthandler.cancel_dump_traceback_later()

        main = threading.get_ident()
        threads = {c[3] for c in store.calls}
        assert threads - {main}, "the store was never called from a lucivy thread"
        saves = [c for c in store.calls if c[0] == "save"]
        assert any(c[3] != main for c in saves)

    def test_store_exception_surfaces_from_commit(self, cache_dir):
        faulthandler.dump_traceback_later(120, exit=True)
        try:
            store = FailingBlobStore()
            idx = lucivy.Index.create_with_blob_store(store, "failing", FIELDS, cache_dir=cache_dir)
            idx.add_many(DOCS[:3])
            idx.commit()
            store.arm()
            idx.add(doc_id=50, title="Doomed", body="This commit cannot be saved")
            with pytest.raises(ValueError, match="disk on fire"):
                idx.commit()
            # The index is not wedged: the error is reported again, not swallowed.
            idx.add(doc_id=51, title="Still doomed", body="Still cannot be saved")
            with pytest.raises(ValueError, match="disk on fire"):
                idx.commit()
            store.armed = False
        finally:
            faulthandler.cancel_dump_traceback_later()


# ─── drop_index ──────────────────────────────────────────────────────────────


class TestDropIndex:
    def test_drop_deletes_every_blob_through_the_store(self, cache_dir):
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(store, "doomed", FIELDS, shards=2, cache_dir=cache_dir)
        idx.add_many(DOCS)
        idx.commit()
        assert store.list("Lucivy_doomed/shard_0")
        assert store.list("Lucivy_doomed/shard_1")
        assert store.list("doomed")
        idx.drop_index()
        for ns in ["Lucivy_doomed/shard_0", "Lucivy_doomed/shard_1", "doomed"]:
            assert store.list(ns) == [], f"{ns} not empty after drop_index"
        assert store.namespaces() == []
        deletes = [c for c in store.calls if c[0] == "delete"]
        assert deletes, "blobs must go through the store's delete"
        assert repr(idx) == "Index(dropped)"
        with pytest.raises(ValueError, match="dropped"):
            idx.search("python")

    def test_recreate_after_drop(self, cache_dir):
        store = DictBlobStore()
        idx = lucivy.Index.create_with_blob_store(store, "reborn", FIELDS, cache_dir=cache_dir)
        idx.add_many(DOCS)
        idx.commit()
        idx.drop_index()
        again = lucivy.Index.create_with_blob_store(store, "reborn", FIELDS, cache_dir=cache_dir)
        again.add(doc_id=1, title="Fresh", body="A fresh index under the same name")
        again.commit()
        assert again.num_docs == 1
        assert [r.doc_id for r in again.search("fresh")] == [1]
        again.close()
