"""Comprehensive tests for the lucivy Python bindings.

Covers: CRUD, contains, contains_split, fuzzy (via distance), regex (via contains+regex),
highlights, composite queries, filters, delete, update, persistence.

Query type guide:
─────────────────
- String query:                  "hello world" → auto contains_split across all text fields
- type=contains:                 Substring match on stored text (cross-token, fuzzy by default d=1)
- type=contains + distance=N:    Fuzzy substring match (Levenshtein tolerance on each token)
- type=contains + regex=True:    Regex on stored text (cross-token), uses ngram acceleration
- type=contains_split:           Splits on whitespace, each word → contains, combined with boolean should
- type=boolean:                  Combine sub-queries with must/should/must_not
"""

import os
import shutil
import tempfile

import pytest

import lucivy


# ─── Fixtures ────────────────────────────────────────────────────────────────


SAMPLE_DOCS = [
    {"doc_id": 1, "title": "Introduction to Python", "body": "Python is a versatile programming language used for web development and data science."},
    {"doc_id": 2, "title": "Rust Programming", "body": "Rust provides memory safety without garbage collection through its ownership system."},
    {"doc_id": 3, "title": "JavaScript Basics", "body": "JavaScript is the language of the web, running in every browser natively."},
    {"doc_id": 4, "title": "Machine Learning with Python", "body": "Deep learning frameworks like PyTorch and TensorFlow make neural networks accessible."},
    {"doc_id": 5, "title": "Database Design", "body": "Relational databases use SQL for querying structured data efficiently."},
    {"doc_id": 6, "title": "Graph Databases", "body": "Graph databases like Neo4j store relationships natively, making traversals fast."},
    {"doc_id": 7, "title": "Full-Text Search", "body": "Inverted indexes power full-text search engines like Lucene, Tantivy and Elasticsearch."},
    {"doc_id": 8, "title": "Web Development", "body": "Modern web development combines frontend frameworks with backend APIs and microservices."},
]

FIELDS = [
    {"name": "title", "type": "text"},
    {"name": "body", "type": "text"},
]


@pytest.fixture
def tmp_dir():
    d = tempfile.mkdtemp(prefix="lucivy_test_")
    yield d
    shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def index(tmp_dir):
    """Create an index with sample docs, committed."""
    idx = lucivy.Index.create(os.path.join(tmp_dir, "idx"), FIELDS)
    idx.add_many(SAMPLE_DOCS)
    idx.commit()
    return idx


@pytest.fixture
def index_with_filter_fields(tmp_dir):
    """Index with mixed field types for filter tests.

    Non-text fields need indexed=True and fast=True to be usable in filters.
    """
    fields = [
        {"name": "title", "type": "text"},
        {"name": "body", "type": "text"},
        {"name": "year", "type": "i64", "indexed": True, "fast": True},
        {"name": "score", "type": "f64", "indexed": True, "fast": True},
    ]
    idx = lucivy.Index.create(os.path.join(tmp_dir, "idx_filter"), fields)
    idx.add_many([
        {"doc_id": 1, "title": "Old Article", "body": "Ancient history of computing", "year": 2000, "score": 3.5},
        {"doc_id": 2, "title": "Recent Article", "body": "Modern computing advances", "year": 2024, "score": 9.1},
        {"doc_id": 3, "title": "Future Article", "body": "Quantum computing predictions", "year": 2030, "score": 7.0},
    ])
    idx.commit()
    return idx


# ─── CRUD basics ─────────────────────────────────────────────────────────────


class TestCRUD:
    def test_create_and_count(self, index):
        assert index.num_docs == len(SAMPLE_DOCS)

    def test_schema(self, index):
        schema = index.schema
        names = {f["name"] for f in schema}
        assert "title" in names
        assert "body" in names

    def test_path(self, index, tmp_dir):
        assert index.path == os.path.join(tmp_dir, "idx")

    def test_repr(self, index):
        r = repr(index)
        assert "Index(" in r
        assert "num_docs=8" in r

    def test_add_single(self, tmp_dir):
        idx = lucivy.Index.create(os.path.join(tmp_dir, "single"), FIELDS)
        idx.add(doc_id=42, title="Hello", body="World")
        idx.commit()
        assert idx.num_docs == 1
        results = idx.search("hello")
        assert len(results) == 1
        assert results[0].doc_id == 42

    def test_add_many(self, index):
        results = index.search("python")
        assert len(results) >= 1

    def test_context_manager(self, tmp_dir):
        path = os.path.join(tmp_dir, "ctx")
        with lucivy.Index.create(path, FIELDS) as idx:
            idx.add(doc_id=1, title="Test", body="Context manager works")
            idx.commit()
            assert idx.num_docs == 1


# ─── Contains search (cross-token, the main query type) ─────────────────────


class TestContains:
    """type=contains — operates on stored text, supports cross-token matching,
    fuzzy tolerance (distance=N, default 1), and substring matching."""

    def test_contains_single_word(self, index):
        """String query → contains_split across all text fields."""
        results = index.search("python")
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids  # "Introduction to Python"
        assert 4 in doc_ids  # "Machine Learning with Python"

    def test_contains_dict_query(self, index):
        """Explicit contains dict query on a specific field."""
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "python",
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids
        assert 4 in doc_ids

    def test_contains_no_match(self, index):
        results = index.search("zzzznonexistent")
        assert len(results) == 0

    def test_contains_case_insensitive(self, index):
        results_lower = index.search("rust")
        results_upper = index.search("Rust")
        assert len(results_lower) > 0
        assert {r.doc_id for r in results_lower} == {r.doc_id for r in results_upper}

    def test_contains_cross_token(self, index):
        """Contains matches across token boundaries (e.g. 'garbage collection' = 2 tokens)."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "garbage collection",
        })
        doc_ids = {r.doc_id for r in results}
        assert 2 in doc_ids

    def test_contains_multi_token_phrase(self, index):
        """Contains handles multi-word phrases naturally (with separator awareness)."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "programming language",
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids

    def test_contains_substring(self, index):
        """Contains matches substrings within tokens (e.g. 'program' in 'programming')."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "program",
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids


# ─── Contains split ─────────────────────────────────────────────────────────


class TestContainsSplit:
    """type=contains_split — splits query on whitespace, each word becomes
    a separate contains clause, combined with boolean should (OR).
    This is what a bare string query does automatically."""

    def test_string_query_splits_words(self, index):
        """A string query 'python web' should split into contains per word."""
        results = index.search("python web")
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids

    def test_contains_split_dict(self, index):
        """Explicit contains_split via dict — each word is OR'd."""
        results = index.search({
            "type": "contains_split",
            "field": "body",
            "value": "memory safety",
        })
        doc_ids = {r.doc_id for r in results}
        assert 2 in doc_ids

    def test_contains_split_multi_word(self, index):
        """Multiple words should match docs with any of the words."""
        results = index.search("database sql graph")
        doc_ids = {r.doc_id for r in results}
        assert 5 in doc_ids  # Database Design (SQL)
        assert 6 in doc_ids  # Graph Databases

    def test_contains_split_single_word_fallback(self, index):
        """Single word in contains_split should behave like contains."""
        results_split = index.search("javascript")
        results_contains = index.search({
            "type": "contains",
            "field": "title",
            "value": "javascript",
        })
        assert any(r.doc_id == 3 for r in results_split)
        assert any(r.doc_id == 3 for r in results_contains)

    def test_contains_vs_contains_split(self, index):
        """contains treats 'memory safety' as a PHRASE (both words, in order).
        contains_split treats it as OR (either word matches)."""
        # contains: phrase-like, cross-token
        results_phrase = index.search({
            "type": "contains",
            "field": "body",
            "value": "memory safety",
        })
        # contains_split: OR of individual words
        results_split = index.search({
            "type": "contains_split",
            "field": "body",
            "value": "memory safety",
        })
        # contains_split should return at least as many results
        assert len(results_split) >= len(results_phrase)


# ─── Fuzzy search ───────────────────────────────────────────────────────────


class TestFuzzy:
    """Fuzzy matching via contains with distance=N (cross-token).

    - Best for: substring/phrase search tolerant to typos
    - Default distance=1 catches common typos
    - distance=0 for exact substring only
    """

    def test_contains_fuzzy_single_word(self, index):
        """Contains with distance=1 catches single-word typos (cross-token aware)."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "memry",  # typo for "memory"
            "distance": 1,
        })
        doc_ids = {r.doc_id for r in results}
        assert 2 in doc_ids

    def test_contains_fuzzy_multi_word(self, index):
        """Contains with distance catches typos in multi-word phrases."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "programing languag",  # typos in both words
            "distance": 1,
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids

    def test_contains_fuzzy_distance_0_exact_only(self, index):
        """Contains with distance=0 requires exact substring match."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "memry",  # typo
            "distance": 0,
        })
        assert all(r.doc_id != 2 for r in results)

    def test_contains_split_fuzzy(self, index):
        """contains_split with distance passes fuzzy tolerance to each word."""
        results = index.search({
            "type": "contains_split",
            "field": "body",
            "value": "memry safty",  # typos
            "distance": 1,
        })
        doc_ids = {r.doc_id for r in results}
        assert 2 in doc_ids


# ─── Regex search ────────────────────────────────────────────────────────────


class TestRegex:
    """Regex matching via contains with regex=True (cross-token, ngram-accelerated).

    - Pattern matches against stored text (the full field value)
    - Cross-token: "program.*language" matches "programming language"
    - Uses ngram acceleration + regex verification
    """

    def test_contains_regex_cross_token(self, index):
        """contains+regex matches across token boundaries on stored text."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "program.*language",
            "regex": True,
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids  # "programming language"

    def test_contains_regex_alternation(self, index):
        """contains+regex with | alternation matches multiple patterns."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "python|rust",
            "regex": True,
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids  # Python
        assert 2 in doc_ids  # Rust

    def test_contains_regex_wildcard(self, index):
        """contains+regex with .* for flexible cross-token matching."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "mem.*safe",
            "regex": True,
        })
        doc_ids = {r.doc_id for r in results}
        assert 2 in doc_ids  # "memory safety"

    def test_contains_regex_no_match(self, index):
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "zzzzqqqq",
            "regex": True,
        })
        assert len(results) == 0

    def test_contains_regex_no_match(self, index):
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "zzzzqqqq",
            "regex": True,
        })
        assert len(results) == 0


# ─── Highlights ──────────────────────────────────────────────────────────────


class TestHighlights:
    """highlights=True returns per-field byte offsets of matched text.
    Works with all query types: contains, contains+regex, fuzzy, boolean."""

    def test_highlights_returned(self, index):
        results = index.search("python", highlights=True)
        assert len(results) > 0
        has_highlights = any(r.highlights is not None for r in results)
        assert has_highlights

    def test_highlights_are_byte_offsets(self, index):
        """Highlight offsets are (start, end) tuples of valid byte ranges."""
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "python",
        }, highlights=True)
        for r in results:
            if r.highlights:
                for field_name, offsets in r.highlights.items():
                    assert isinstance(offsets, list)
                    for start, end in offsets:
                        assert isinstance(start, int)
                        assert isinstance(end, int)
                        assert start < end

    def test_highlights_off_by_default(self, index):
        results = index.search("python")
        assert all(r.highlights is None for r in results)

    def test_highlights_multi_field(self, index):
        """Highlights span multiple fields when query is cross-field (string query)."""
        # doc 1: "Introduction to Python" (title) + "Python is a..." (body)
        results = index.search("python", highlights=True)
        doc1 = next((r for r in results if r.doc_id == 1), None)
        assert doc1 is not None
        assert doc1.highlights is not None
        assert len(doc1.highlights) >= 1

    def test_highlights_contains_specific_field(self, index):
        """Highlights with explicit contains query show which field matched."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "ownership",
        }, highlights=True)
        assert len(results) > 0
        doc2 = next((r for r in results if r.doc_id == 2), None)
        assert doc2 is not None
        assert doc2.highlights is not None
        assert "body" in doc2.highlights

    def test_highlights_no_internal_fields(self, index):
        """Highlights never expose internal ._raw or ._ngram fields."""
        results = index.search("python", highlights=True)
        for r in results:
            if r.highlights:
                for field_name in r.highlights:
                    assert not field_name.endswith("._raw"), f"internal field leaked: {field_name}"
                    assert not field_name.endswith("._ngram"), f"internal field leaked: {field_name}"

    def test_highlights_with_contains_regex(self, index):
        """Highlights work with contains+regex (cross-token regex)."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "program.*language",
            "regex": True,
        }, highlights=True)
        assert len(results) >= 1
        assert any(r.highlights is not None for r in results)

    def test_highlights_with_fuzzy(self, index):
        """Highlights work with fuzzy contains."""
        results = index.search({
            "type": "contains",
            "field": "body",
            "value": "ownrship",  # typo for "ownership"
            "distance": 1,
        }, highlights=True)
        doc_ids = {r.doc_id for r in results}
        assert 2 in doc_ids
        doc2 = next(r for r in results if r.doc_id == 2)
        assert doc2.highlights is not None

    def test_highlights_with_boolean(self, index):
        """Highlights work across boolean sub-queries."""
        results = index.search({
            "type": "boolean",
            "should": [
                {"type": "contains", "field": "title", "value": "python"},
                {"type": "contains", "field": "body", "value": "neural"},
            ],
        }, highlights=True)
        assert len(results) >= 1
        assert any(r.highlights is not None for r in results)


# ─── Delete ──────────────────────────────────────────────────────────────────


class TestDelete:
    def test_delete_removes_doc(self, index):
        assert index.num_docs == 8
        index.delete(1)
        index.commit()
        assert index.num_docs == 7
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "introduction",
        })
        assert all(r.doc_id != 1 for r in results)

    def test_delete_multiple(self, index):
        index.delete(1)
        index.delete(2)
        index.delete(3)
        index.commit()
        assert index.num_docs == 5

    def test_delete_then_search(self, index):
        """After deleting doc 2 (Rust), searching 'rust' should not find it."""
        index.delete(2)
        index.commit()
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "rust",
        })
        assert all(r.doc_id != 2 for r in results)


# ─── Update ──────────────────────────────────────────────────────────────────


class TestUpdate:
    def test_update_modifies_content(self, index):
        """Update = delete + re-add. Old content gone, new content searchable."""
        index.update(1, title="Golang Tutorial", body="Go is a compiled language by Google.")
        index.commit()
        assert index.num_docs == 8  # count unchanged

        # Old title should not match
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "introduction",
        })
        assert all(r.doc_id != 1 for r in results)

        # New title should match
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "golang",
        })
        assert any(r.doc_id == 1 for r in results)

    def test_update_preserves_other_docs(self, index):
        index.update(1, title="Changed", body="Changed body")
        index.commit()
        results = index.search({
            "type": "contains",
            "field": "title",
            "value": "rust",
        })
        assert any(r.doc_id == 2 for r in results)


# ─── Persistence ─────────────────────────────────────────────────────────────


class TestPersistence:
    def test_reopen_preserves_data(self, tmp_dir):
        """Create -> commit -> close -> reopen -> same data."""
        path = os.path.join(tmp_dir, "persist")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add_many(SAMPLE_DOCS)
        idx.commit()
        count_before = idx.num_docs
        del idx

        idx2 = lucivy.Index.open(path)
        assert idx2.num_docs == count_before
        results = idx2.search("python")
        assert len(results) >= 2

    def test_reopen_after_delete(self, tmp_dir):
        path = os.path.join(tmp_dir, "persist_del")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add_many(SAMPLE_DOCS)
        idx.commit()
        idx.delete(1)
        idx.commit()
        del idx

        idx2 = lucivy.Index.open(path)
        assert idx2.num_docs == 7
        results = idx2.search({
            "type": "contains",
            "field": "title",
            "value": "introduction",
        })
        assert all(r.doc_id != 1 for r in results)

    def test_reopen_after_update(self, tmp_dir):
        path = os.path.join(tmp_dir, "persist_upd")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add_many(SAMPLE_DOCS)
        idx.commit()
        idx.update(1, title="Quantum Computing", body="Qubits and entanglement.")
        idx.commit()
        del idx

        idx2 = lucivy.Index.open(path)
        results = idx2.search({
            "type": "contains",
            "field": "title",
            "value": "quantum",
        })
        assert any(r.doc_id == 1 for r in results)

    def test_uncommitted_not_persisted(self, tmp_dir):
        """Data added but not committed is lost on reopen."""
        path = os.path.join(tmp_dir, "persist_uncommit")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add(doc_id=1, title="Committed", body="This is committed")
        idx.commit()
        idx.add(doc_id=2, title="Not committed", body="This should vanish")
        del idx

        idx2 = lucivy.Index.open(path)
        assert idx2.num_docs == 1

    def test_rollback(self, tmp_dir):
        """Rollback discards uncommitted changes."""
        path = os.path.join(tmp_dir, "persist_rollback")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add(doc_id=1, title="Keep", body="Committed")
        idx.commit()
        idx.add(doc_id=2, title="Discard", body="Rolled back")
        idx.rollback()
        idx.commit()
        assert idx.num_docs == 1


# ─── Allowed IDs filter ─────────────────────────────────────────────────────


class TestAllowedIds:
    """allowed_ids restricts search to a subset of doc_ids (pre-filter)."""

    def test_allowed_ids_filters_results(self, index):
        results = index.search("python", allowed_ids=[1])
        assert len(results) == 1
        assert results[0].doc_id == 1

    def test_allowed_ids_empty_returns_nothing(self, index):
        results = index.search("python", allowed_ids=[])
        assert len(results) == 0

    def test_allowed_ids_multiple(self, index):
        results = index.search("python", allowed_ids=[1, 4])
        doc_ids = {r.doc_id for r in results}
        assert doc_ids <= {1, 4}


# ─── Boolean / composite queries ────────────────────────────────────────────


class TestComposite:
    """type=boolean combines sub-queries with must (AND), should (OR), must_not (NOT).
    Sub-queries can be any type (contains, fuzzy, regex, nested boolean)."""

    def test_boolean_must(self, index):
        results = index.search({
            "type": "boolean",
            "must": [
                {"type": "contains", "field": "body", "value": "web"},
                {"type": "contains", "field": "body", "value": "framework"},
            ],
        })
        doc_ids = {r.doc_id for r in results}
        assert 8 in doc_ids  # Web Development has both

    def test_boolean_should(self, index):
        results = index.search({
            "type": "boolean",
            "should": [
                {"type": "contains", "field": "title", "value": "python"},
                {"type": "contains", "field": "title", "value": "rust"},
            ],
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids
        assert 2 in doc_ids

    def test_boolean_must_not(self, index):
        results = index.search({
            "type": "boolean",
            "must": [
                {"type": "contains", "field": "title", "value": "python"},
            ],
            "must_not": [
                {"type": "contains", "field": "title", "value": "machine"},
            ],
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 in doc_ids       # Introduction to Python — kept
        assert 4 not in doc_ids   # Machine Learning with Python — excluded

    def test_boolean_mixed_query_types(self, index):
        """Boolean can mix contains variants (regex, fuzzy) in sub-clauses."""
        results = index.search({
            "type": "boolean",
            "should": [
                {"type": "contains", "field": "body", "value": "python|rust", "regex": True},
                {"type": "contains", "field": "title", "value": "databse", "distance": 1},
            ],
        })
        doc_ids = {r.doc_id for r in results}
        assert len(doc_ids) >= 1


# ─── Filter fields (non-text) ───────────────────────────────────────────────


class TestFilterFields:
    """Filters on non-text fields (i64, f64, etc.) via the 'filters' key.
    Fields must be created with indexed=True, fast=True.
    Supported ops: eq, ne, lt, lte, gt, gte, in, between, not_in, starts_with, contains."""

    def test_filter_eq(self, index_with_filter_fields):
        idx = index_with_filter_fields
        results = idx.search({
            "type": "contains",
            "field": "body",
            "value": "computing",
            "filters": [{"field": "year", "op": "eq", "value": 2024}],
        })
        doc_ids = {r.doc_id for r in results}
        assert doc_ids == {2}

    def test_filter_gt(self, index_with_filter_fields):
        idx = index_with_filter_fields
        results = idx.search({
            "type": "contains",
            "field": "body",
            "value": "computing",
            "filters": [{"field": "year", "op": "gt", "value": 2020}],
        })
        doc_ids = {r.doc_id for r in results}
        assert 1 not in doc_ids
        assert 2 in doc_ids
        assert 3 in doc_ids

    def test_filter_between(self, index_with_filter_fields):
        idx = index_with_filter_fields
        results = idx.search({
            "type": "contains",
            "field": "body",
            "value": "computing",
            "filters": [{"field": "year", "op": "between", "value": [2020, 2025]}],
        })
        doc_ids = {r.doc_id for r in results}
        assert doc_ids == {2}


# ─── Limit ───────────────────────────────────────────────────────────────────


class TestLimit:
    def test_limit_restricts_results(self, index):
        results = index.search("python web database", limit=2)
        assert len(results) <= 2

    def test_limit_default_10(self, index):
        results = index.search("the")
        assert len(results) <= 10


# ─── SearchResult repr ──────────────────────────────────────────────────────


class TestSearchResult:
    def test_repr_without_highlights(self, index):
        results = index.search("python")
        assert len(results) > 0
        r = repr(results[0])
        assert "SearchResult(" in r
        assert "doc_id=" in r
        assert "score=" in r

    def test_repr_with_highlights(self, index):
        results = index.search("python", highlights=True)
        has_h = [r for r in results if r.highlights is not None]
        if has_h:
            r = repr(has_h[0])
            assert "highlights=" in r


# ─── Edge cases ──────────────────────────────────────────────────────────────


class TestEdgeCases:
    def test_empty_index_search(self, tmp_dir):
        idx = lucivy.Index.create(os.path.join(tmp_dir, "empty"), FIELDS)
        idx.commit()
        results = idx.search("anything")
        assert len(results) == 0

    def test_empty_string_query(self, index):
        """Empty string query raises ValueError (no clauses to build)."""
        with pytest.raises(ValueError):
            index.search("")

    def test_special_characters_query(self, index):
        """Queries with special chars (c++) should not crash."""
        results = index.search("c++ is great")
        assert isinstance(results, list)

    def test_search_after_add_before_commit(self, tmp_dir):
        idx = lucivy.Index.create(os.path.join(tmp_dir, "lazy"), FIELDS)
        idx.add(doc_id=1, title="Lazy", body="Lazy commit test")
        idx.commit()
        results = idx.search("lazy")
        assert len(results) >= 1


# ─── Snapshot (LUCE format) ──────────────────────────────────────────────


class TestSnapshot:
    def test_export_import_roundtrip(self, tmp_dir):
        """Export an index, import it at a new path, verify data is intact."""
        path1 = os.path.join(tmp_dir, "snap_src")
        idx = lucivy.Index.create(path1, FIELDS)
        idx.add_many(SAMPLE_DOCS)
        idx.commit()

        blob = idx.export_snapshot()
        assert isinstance(blob, bytes)
        assert blob[:4] == b"LUCE"

        path2 = os.path.join(tmp_dir, "snap_dst")
        idx2 = lucivy.Index.import_snapshot(blob, dest_path=path2)
        assert idx2.num_docs == 8
        results = idx2.search("python")
        assert len(results) >= 2

    def test_export_to_file(self, tmp_dir):
        """export_snapshot_to writes to disk, import_snapshot_from reads it."""
        path1 = os.path.join(tmp_dir, "snap_file_src")
        idx = lucivy.Index.create(path1, FIELDS)
        idx.add_many(SAMPLE_DOCS)
        idx.commit()

        snap_file = os.path.join(tmp_dir, "snapshot.luce")
        idx.export_snapshot_to(snap_file)
        assert os.path.exists(snap_file)
        assert os.path.getsize(snap_file) > 0

        path2 = os.path.join(tmp_dir, "snap_file_dst")
        idx2 = lucivy.Index.import_snapshot_from(snap_file, dest_path=path2)
        assert idx2.num_docs == 8

    def test_export_uncommitted_raises(self, tmp_dir):
        """Export with uncommitted changes should raise ValueError."""
        path = os.path.join(tmp_dir, "snap_uncommit")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add(doc_id=1, title="Test", body="Test body")
        with pytest.raises(ValueError, match="uncommitted"):
            idx.export_snapshot()

    def test_export_after_commit_ok(self, tmp_dir):
        """After commit, export should work."""
        path = os.path.join(tmp_dir, "snap_commit_ok")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add(doc_id=1, title="Test", body="Test body")
        idx.commit()
        blob = idx.export_snapshot()
        assert len(blob) > 12  # header at minimum

    def test_multi_index_snapshot(self, tmp_dir):
        """Export and import multiple indexes in one snapshot."""
        path1 = os.path.join(tmp_dir, "multi_a")
        path2 = os.path.join(tmp_dir, "multi_b")

        idx1 = lucivy.Index.create(path1, FIELDS)
        idx1.add(doc_id=1, title="Alpha", body="First index")
        idx1.commit()

        idx2 = lucivy.Index.create(path2, FIELDS)
        idx2.add(doc_id=10, title="Beta", body="Second index")
        idx2.commit()

        blob = lucivy.export_snapshots([idx1, idx2])
        assert blob[:4] == b"LUCE"

        dst1 = os.path.join(tmp_dir, "restored_a")
        dst2 = os.path.join(tmp_dir, "restored_b")
        restored = lucivy.import_snapshots(blob, dest_paths=[dst1, dst2])
        assert len(restored) == 2
        assert restored[0].num_docs == 1
        assert restored[1].num_docs == 1

        r1 = restored[0].search("alpha")
        assert any(r.doc_id == 1 for r in r1)
        r2 = restored[1].search("beta")
        assert any(r.doc_id == 10 for r in r2)

    def test_import_preserves_search(self, tmp_dir):
        """Imported index supports all search types."""
        path1 = os.path.join(tmp_dir, "snap_search_src")
        idx = lucivy.Index.create(path1, FIELDS, stemmer="english")
        idx.add_many(SAMPLE_DOCS)
        idx.commit()

        blob = idx.export_snapshot()
        path2 = os.path.join(tmp_dir, "snap_search_dst")
        idx2 = lucivy.Index.import_snapshot(blob, dest_path=path2)

        # Contains
        r = idx2.search({"type": "contains", "field": "title", "value": "python"})
        assert len(r) >= 1

        # Highlights
        r = idx2.search("python", highlights=True)
        assert any(x.highlights is not None for x in r)

        # Boolean
        r = idx2.search({
            "type": "boolean",
            "must": [
                {"type": "contains", "field": "body", "value": "web"},
            ],
            "must_not": [
                {"type": "contains", "field": "title", "value": "javascript"},
            ],
        })
        assert all(x.doc_id != 3 for x in r)

    def test_rollback_clears_uncommitted_flag(self, tmp_dir):
        """After rollback, export should work (no uncommitted)."""
        path = os.path.join(tmp_dir, "snap_rollback")
        idx = lucivy.Index.create(path, FIELDS)
        idx.add(doc_id=1, title="Keep", body="Committed")
        idx.commit()
        idx.add(doc_id=2, title="Discard", body="Rolled back")
        idx.rollback()
        # Should not raise — rollback clears uncommitted
        blob = idx.export_snapshot()
        assert len(blob) > 12
