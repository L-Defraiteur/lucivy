# Bug BM25 : idf crash multi-field + f64 — progression

## Symptôme

```
thread 'scheduler-0' panicked at src/query/bm25.rs:54:5: 5 >= 9
```

`idf(doc_freq=9, doc_count=5)` — impossible, plus de docs avec le terme que de docs total.

## Repro

```python
idx = Index.create(path, [
    {"name": "title", "type": "text", "stored": True},
    {"name": "body", "type": "text", "stored": True},
    {"name": "score", "type": "f64", "fast": True},  # <-- f64 field nécessaire
], shards=2)
# 5 docs avec title + body + score
idx.search("mutex lock")  # CRASH
```

## Ce qu'on a éliminé

1. **Multi-field sans f64** : test Rust `disjunction_max(contains_split title, contains_split body)` sur 2 shards → **PASSE**. doc_freq correct (2-3 ≤ 5).

2. **Multi-field avec f64** : même test avec champ f64 ajouté → **PASSE aussi** en Rust natif.

3. **Single field** : Python binding avec 1 seul field text → **PASSE**.

## Ce qui crashe

Uniquement via le **binding Python** avec 2 fields text + 1 field f64.

## Différence Python vs Rust natif

Le binding Python (`build_contains_split_multi_field`) crée :
```
boolean should [
  boolean should [ contains "mutex" on title, contains "mutex" on body ],
  boolean should [ contains "lock" on title, contains "lock" on body ],
]
```

Le test Rust crée :
```
disjunction_max [
  contains_split "mutex lock" on title,
  contains_split "mutex lock" on body,
]
```

La structure est différente ! Mais les deux devraient fonctionner.

## Hypothèse

Le field f64 `score` change les Field IDs (Field(0)=_node_id, Field(1)=title, Field(2)=body, **Field(3)=score**).
Le prescan ou le SFX walk pourrait confondre les field IDs quand il y a des champs non-text dans le schema.

## Prochaine étape

Reproduire en Rust natif avec la **même structure de query** que le binding Python (boolean imbriqué, pas disjunction_max) + le f64 field.
