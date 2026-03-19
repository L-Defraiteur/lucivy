# Doc 02 — Progression : SegmentComponent natif pour SFX

Date : 20 mars 2026

## Fait

- SegmentComponent réécrit proprement : SuffixFst{field_id}, SuffixPost{field_id}
- InnerSegmentMeta gagne sfx_field_ids: Vec<u32> (persisté dans meta.json)
- list_files() inclut automatiquement les per-field .sfx/.sfxpost
- Manifest .sfx supprimé (no-op) — sfx_field_ids dans le meta suffit
- segment_reader: fallback legacy manifest pour vieux index
- segment_updater: plus de lecture de manifest dans list_files()

## 38 tests fail — à fixer

Tous dans les tests query (automaton_phrase, regex_continuation, suffix_contains).
Root cause : les segments créés dans les tests n'ont pas sfx_field_ids peuplé,
donc load_sfx_files() ne trouve plus les fichiers per-field.

## Fix nécessaire

Le segment_writer::finalize() retourne la liste des sfx_field_ids.
Le caller (IndexWriter/FinalizerActor) les propage dans le SegmentMeta
via new_segment_meta().

Concrètement :
1. segment_writer::finalize() retourne (doc_opstamps, sfx_field_ids)
2. new_segment_meta() prend sfx_field_ids en argument
3. Le merge aussi : merge_state close → SegmentMeta avec sfx_field_ids

C'est de la plomberie — pas de changement de logique, juste propager
l'info à travers la chaîne.
