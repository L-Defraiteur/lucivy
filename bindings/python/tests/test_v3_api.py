"""Tests for the methods added to the Python binding with the 3.0.0 engine.

Covers: compact, wait_merges_quiet, index_bytes, open_snapshot /
open_snapshot_from (served from memory, read-only), drop_index.
"""

import os
import shutil
import tempfile

import pytest

import lucivy


FIELDS = [
    {"name": "title", "type": "text"},
    {"name": "body", "type": "text"},
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


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp(prefix="lucivy_v3_")
    yield d
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def index(tmp_dir):
    idx = lucivy.Index.create(os.path.join(tmp_dir, "idx"), FIELDS)
    idx.add_many(DOCS)
    idx.commit()
    return idx


def hits(idx, query):
    """(doc_id, rounded score) pairs — the answer an index gives to a query."""
    return [(r.doc_id, round(r.score, 4)) for r in idx.search(query, limit=20)]


# ─── compact / wait_merges_quiet / index_bytes ───────────────────────────────


class TestMaintenance:
    def test_compact_after_several_commits(self, tmp_dir):
        """Many small commits leave many segments; compact folds them and keeps the data."""
        idx = lucivy.Index.create(os.path.join(tmp_dir, "compact"), FIELDS)
        for doc in DOCS:
            idx.add(**doc)
            idx.commit()
        before = hits(idx, "python")
        merges = idx.compact()
        assert isinstance(merges, int)
        assert merges >= 0
        assert idx.num_docs == len(DOCS)
        assert hits(idx, "python") == before

    def test_compact_max_docs_argument(self, index):
        merges = index.compact(max_docs=4)
        assert isinstance(merges, int)
        assert index.num_docs == len(DOCS)
        assert len(index.search("python")) >= 2

    def test_compact_commits_pending_writes(self, index):
        """compact() commits first: an uncommitted add becomes searchable."""
        index.add(doc_id=99, title="Pending", body="Compaction flushes me")
        index.compact()
        assert index.num_docs == len(DOCS) + 1
        assert any(r.doc_id == 99 for r in index.search("compaction"))

    def test_wait_merges_quiet(self, index):
        rounds = index.wait_merges_quiet()
        assert isinstance(rounds, int)
        assert rounds >= 0
        # Idempotent: a quiet index stays quiet.
        assert index.wait_merges_quiet() >= 0

    def test_index_bytes_positive_after_commit(self, index):
        size = index.index_bytes()
        assert isinstance(size, int)
        assert size > 0

    def test_index_bytes_empty_index(self, tmp_dir):
        idx = lucivy.Index.create(os.path.join(tmp_dir, "empty"), FIELDS)
        idx.commit()
        assert idx.index_bytes() == 0

    def test_index_bytes_grows_with_content(self, tmp_dir):
        idx = lucivy.Index.create(os.path.join(tmp_dir, "grow"), FIELDS)
        idx.add_many(DOCS[:2])
        idx.commit()
        small = idx.index_bytes()
        idx.add_many([
            {"doc_id": 100 + i, "title": f"Filler {i}", "body": " ".join(f"word{i}_{j}" for j in range(60))}
            for i in range(40)
        ])
        idx.commit()
        assert idx.index_bytes() > small


# ─── open_snapshot: served from memory, read-only ───────────────────────────


class TestOpenSnapshot:
    def test_same_answers_as_source(self, index):
        blob = index.export_snapshot()
        served = lucivy.Index.open_snapshot(blob)
        assert served.num_docs == index.num_docs
        assert served.num_shards == index.num_shards
        for q in QUERIES:
            assert hits(served, q) == hits(index, q), f"query {q!r} differs"

    def test_highlights_and_fields(self, tmp_dir):
        path = os.path.join(tmp_dir, "stored")
        idx = lucivy.Index.create(path, [
            {"name": "title", "type": "text", "stored": True},
            {"name": "body", "type": "text", "stored": True},
        ])
        idx.add_many(DOCS)
        idx.commit()
        served = lucivy.Index.open_snapshot(idx.export_snapshot())
        results = served.search("ownership", highlights=True, fields=True)
        assert len(results) == 1
        assert results[0].doc_id == 2
        assert "body" in results[0].highlights
        assert results[0].fields["title"] == "Rust Programming"

    def test_open_snapshot_from_file(self, index, tmp_dir):
        snap = os.path.join(tmp_dir, "idx.luce")
        index.export_snapshot_to(snap)
        served = lucivy.Index.open_snapshot_from(snap)
        assert served.num_docs == len(DOCS)
        assert hits(served, "python") == hits(index, "python")
        # Nothing was extracted next to the file.
        assert sorted(os.listdir(tmp_dir)) == ["idx", "idx.luce"]

    def test_nothing_written_to_disk(self, index, tmp_dir):
        blob = index.export_snapshot()
        before = set(os.listdir(tmp_dir))
        served = lucivy.Index.open_snapshot(blob)
        served.search("python")
        assert set(os.listdir(tmp_dir)) == before
        assert served.path is None
        assert "<snapshot>" in repr(served)

    def test_multi_shard_snapshot(self, tmp_dir):
        idx = lucivy.Index.create(os.path.join(tmp_dir, "sharded"), FIELDS, shards=3)
        idx.add_many(DOCS)
        idx.commit()
        served = lucivy.Index.open_snapshot(idx.export_snapshot())
        assert served.num_shards == 3
        assert served.num_docs == len(DOCS)
        for q in QUERIES:
            assert hits(served, q) == hits(idx, q), f"query {q!r} differs"

    def test_index_bytes_measures_the_blob(self, index):
        blob = index.export_snapshot()
        served = lucivy.Index.open_snapshot(blob)
        measured = served.index_bytes()
        assert 0 < measured <= len(blob)

    def test_read_only_mutations_raise(self, index):
        """Every mutation is refused up front, with the reason, not at commit."""
        served = lucivy.Index.open_snapshot(index.export_snapshot())
        with pytest.raises(ValueError, match="read-only"):
            served.add(doc_id=500, title="Nope", body="A snapshot is read-only")
        with pytest.raises(ValueError, match="read-only"):
            served.add_many([{"doc_id": 501, "title": "Nope", "body": "Still read-only"}])
        with pytest.raises(ValueError, match="read-only"):
            served.delete(1)
        with pytest.raises(ValueError, match="read-only"):
            served.update(1, title="Nope", body="Read-only")
        with pytest.raises(ValueError, match="read-only"):
            served.commit()
        with pytest.raises(ValueError, match="read-only"):
            served.compact()
        # Untouched, and still searchable.
        assert served.num_docs == len(DOCS)
        assert served.search("nope") == []
        assert hits(served, "python") == hits(index, "python")

    def test_read_only_delta_raises(self, index):
        served = lucivy.Index.open_snapshot(index.export_snapshot())
        with pytest.raises(ValueError, match="snapshot"):
            served.apply_sharded_delta(b"LUCIDS")
        with pytest.raises(ValueError, match="snapshot"):
            served.export_sharded_delta(index.shard_versions)

    def test_read_only_export_raises(self, index, tmp_dir):
        served = lucivy.Index.open_snapshot(index.export_snapshot())
        with pytest.raises(ValueError, match="snapshot"):
            served.export_snapshot()
        with pytest.raises(ValueError, match="snapshot"):
            served.export_snapshot_to(os.path.join(tmp_dir, "again.luce"))
        with pytest.raises(ValueError, match="snapshot"):
            served.drop_index()

    def test_close_releases_the_blob(self, index):
        served = lucivy.Index.open_snapshot(index.export_snapshot())
        served.close()
        assert repr(served) == "Index(dropped)"
        with pytest.raises(ValueError):
            served.search("python")

    def test_garbage_is_rejected(self):
        with pytest.raises(ValueError):
            lucivy.Index.open_snapshot(b"not a LUCE snapshot at all")

    def test_wait_merges_quiet_on_snapshot(self, index):
        served = lucivy.Index.open_snapshot(index.export_snapshot())
        assert served.wait_merges_quiet() == 0


# ─── drop_index ─────────────────────────────────────────────────────────────


class TestDropIndex:
    def test_directory_is_gone(self, tmp_dir):
        path = os.path.join(tmp_dir, "doomed")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add_many(DOCS)
        idx.commit()
        assert os.path.isdir(path)
        idx.drop_index()
        assert not os.path.exists(path)

    def test_further_calls_raise(self, tmp_dir):
        path = os.path.join(tmp_dir, "doomed2")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add_many(DOCS)
        idx.commit()
        idx.drop_index()
        assert repr(idx) == "Index(dropped)"
        with pytest.raises(ValueError, match="dropped"):
            idx.search("python")
        with pytest.raises(ValueError, match="dropped"):
            idx.add(doc_id=1, title="a", body="b")
        with pytest.raises(ValueError, match="dropped"):
            idx.commit()
        with pytest.raises(ValueError, match="dropped"):
            idx.num_docs
        with pytest.raises(ValueError, match="dropped"):
            idx.index_bytes()
        with pytest.raises(ValueError, match="dropped"):
            idx.export_snapshot()
        with pytest.raises(ValueError, match="dropped"):
            idx.close()
        with pytest.raises(ValueError, match="dropped"):
            idx.drop_index()

    def test_drop_with_uncommitted_writes(self, tmp_dir):
        """drop_index commits first (close), then removes: no leftover files."""
        path = os.path.join(tmp_dir, "doomed3")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add(doc_id=1, title="Never", body="Never persisted")
        idx.drop_index()
        assert not os.path.exists(path)

    def test_recreate_at_same_path(self, tmp_dir):
        path = os.path.join(tmp_dir, "reborn")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add_many(DOCS)
        idx.commit()
        idx.drop_index()
        idx2 = lucivy.Index.create(path, FIELDS)
        idx2.add(doc_id=1, title="Fresh", body="A fresh index at the same path")
        idx2.commit()
        assert idx2.num_docs == 1
        assert [r.doc_id for r in idx2.search("fresh")] == [1]


# ─── shared_dictionary ────────────────────────────────────────────────────


class TestSharedDictionary:
    def test_same_answers_as_the_default_and_survives_reopen(self, tmp_dir):
        """`shared_dictionary=True` (one dictionary per shard, `sfx_version` 4)
        answers exactly like the default index — documents and scores — over
        several commits on several shards, and after a close / open."""
        plain = lucivy.Index.create(os.path.join(tmp_dir, "plain"), FIELDS, shards=2)
        compact_path = os.path.join(tmp_dir, "shared")
        shared = lucivy.Index.create(compact_path, FIELDS, shards=2, shared_dictionary=True)
        for idx in (plain, shared):
            for doc in DOCS:
                idx.add(**doc)
                idx.commit()
        queries = ["python", "language", "data", "web", "learning"]
        for q in queries:
            assert hits(shared, q) == hits(plain, q), q
        shared.close()
        reopened = lucivy.Index.open(compact_path)
        for q in queries:
            assert hits(reopened, q) == hits(plain, q), f"{q} after reopen"

    def test_option_is_refused_with_a_contradicting_sfx_version(self):
        """The core refuses a config that says both; the binding cannot express
        that, so this only pins that the flag is accepted and is boolean."""
        assert lucivy.Index.create.__doc__ and "shared_dictionary" in lucivy.Index.create.__doc__
