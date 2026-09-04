# Prochain chantier : la phase FST d'une requête en un nœud par shard — rapport

Écrit le 5 septembre 2026 au matin pour la session suivante, qui repart
sans l'historique. À lire avec [07](07-architecture.md) (§2.5 : le mode
dictionnaire tel qu'il existe) et [08](08-knowledge-dump-baselines-tests-outils.md)
(§6 bis : le construire, le mesurer). Le détail de ce qui a été essayé est
dans [09](09-journal-chantier-dictionnaire.md) §8 et §11.

---

## 1. Pourquoi

Le dictionnaire partagé par shard (`sfx_version` 4) fait ce qu'on voulait
pour la taille : noyau entier 11,06 → **5,98 Go** (×6,7 le texte ; 18 Go et
×21 le 4 au matin), référence 10 000 fichiers 508 → 390 Mo, comptes et
spans identiques partout. Il ne peut pas être le défaut : **à froid, une
requête est ×2 à ×22 plus lente** qu'en v3 sur 30 000 fichiers (`sched
term` 3,2 → 70 ms, `mutex_lock relax` 1,6 → 21, fz1 12 → 121), ×1,3-1,5
à chaud.

La cause est structurelle, pas un détail : en v3, chaque segment marche sa
petite FST dans sa propre tâche, 24 threads ; avec le dictionnaire, la
marche est faite **une fois** (mémo du lecteur partagé) mais **sur un
thread** — le premier segment qui la demande calcule, les 159 autres
attendent. Le CPU total est le même, sa répartition est pire.

Trois rustines ont été mesurées ([09](09-journal-chantier-dictionnaire.md) §11) :
cellules par partition et pré-calculs en tâches (fuzzy froid −25 % sur
10 000, rien sur les exactes) ; scans par sous-plage en 257 tâches (**plus
lent** : les prescans en attente se pompent entre eux) ; rotation des
n-grammes (rien). La conclusion : le travail FST d'une requête doit être
**un nœud du DAG de recherche, par shard**, avec son propre parallélisme,
au lieu d'une mémo qui intercepte des appels faits depuis 160 tâches.

---

## 2. Ce qu'on propose

**Deux phases explicites** dans les briques, au lieu d'une :

1. **Le plan** (par shard, une fois) : tout ce qui ne dépend que de la FST
   du dictionnaire et de la requête — candidats (`fst_candidates_v3`) par
   partition, splits (`falling_walk_chunks` / `_words`), chaînes par
   reste (`build_chains_from_splits`, dont les restes des seconds tokens
   ancrés), et pour le fuzzy les comptes des sous-chaînes, le choix des
   pièces et leurs listes ; pour la regex les littéraux. Le plan est un
   objet immuable (`QueryPlan`) : listes triées par identifiant global.
2. **L'exécution** (par segment, en parallèle comme aujourd'hui) : vue par
   segment du plan (marche fusionnée avec le `.gmap`, `keep_in_segment`),
   puis résolution — postings, posmap, fratrie, fenêtres, vérification —
   exactement les briques d'aujourd'hui après leur partie FST.

**Le parallélisme du plan** : ses cellules sont indépendantes par
partition, par reste et par pièce. Le nœud les soumet comme tâches d'un
fan-out (une par cellule, en priorité Critical) et les attend — et
**personne n'attend sous lui** : les nœuds par segment ne démarrent
qu'après (dépendance de DAG), donc plus d'attente coopérative qui pompe un
prescan qui attend la même cellule. Les restes ne sont connus qu'après les
splits : deux vagues (candidats + splits, puis chaînes par reste), une
troisième pour les restes imbriqués s'il y en a (rares : chaînes de trois
tokens et plus).

**Où ça s'insère** : `lucivy_core/src/search_dag.rs` — aujourd'hui
`BuildWeightNode` appelle `query.prescan_segments(&refs)` pour tous les
segments v3 de tous les shards, et le prescan v3 est fait *dans*
`contains_query_v3::run_sfx_v3_prescan` par segment. Ajouter un nœud
`PlanShardNode` par shard (sfx_version 4 seulement), qui produit le
`QueryPlan` par champ, et donner le plan aux prescans par segment (via
`BriquesContext`, champ `plan: Option<&QueryPlan>`). Sans dictionnaire,
rien ne change (le plan est absent, les briques marchent la FST du
segment comme aujourd'hui).

**Ce que ça remplace** : la mémo `FstMemo` (`file_v3.rs`) et la vue
`for_segment` deviennent inutiles pour la requête — à retirer une fois le
nœud en place (garder `keep_in_segment` et `part_views`). Les cellules
`compute_in_tasks` peuvent servir de fan-out, ou être remplacées par des
tâches du DAG.

---

## 3. Ce qui est su du code

- `src/suffix_fst/briques/composite.rs` : `find_literal_v3` (§ chunk
  chains, sibling, word chains, second token anchored) et
  `resolve_trigrams_v3` / `resolve_pieces` / `resolve_all_trigrams` /
  `pivot_cost_estimate` ; `prefetch_fuzzy_scans` est un brouillon du plan
  fuzzy (quelles cellules il faut).
- `src/suffix_fst/briques/fst_walk.rs` : `fst_candidates_v3` (mémo →
  cellules par partition → `keep_in_segment`), `falling_walk_*`,
  `cross_*_chain_v3`, `build_chains_from_splits` (les restes, mémo locale),
  `sibling_chain_dfs` (par segment : fratrie locale, textes du dictionnaire),
  `splits_from_fst_candidates`, `second_token_anchored_v3` dans composite.
- `src/suffix_fst/file_v3.rs` : `SfxFileReaderV3` à plusieurs parties
  (générations), `FstMemo` (trois états), `for_segment`, `part_views`.
- `src/suffix_fst/dictionary.rs` : `SfxDictionary`, `DictionaryField`
  (`sfx_reader()` partagé, `termtexts_reader()` multi-générations,
  `lookup`), `DictionarySlot`.
- `lucivy_core/src/search_dag.rs` : `PrescanShardNode` (v2), `MergePrescanNode`,
  `BuildWeightNode` (le v3 y passe par `prescan_segments`) ;
  `src/query/contains_query_v3.rs` / `fuzzy_query_v3.rs` /
  `regex_query_v3.rs` : les trois chargeurs de `BriquesContext`, qui
  prennent déjà le lecteur et les textes du dictionnaire par
  `SegmentReader::sfx_dictionary_field`.
- Les lecteurs par segment traduisent global ↔ local (`with_gmap`) : ne
  pas y toucher.

---

## 4. Ce qu'il faut mesurer

1. Le plan seul : son temps par requête du panel sur `idx30k-dict` (6,5 M
   de textes) avec 1, 4, 24 tâches — c'est le mur incompressible.
2. Le panel complet, A/B au même binaire contre `idx30k-v7` (script
   `run-ab-dict.sh` du scratchpad), objectif ×1,5 partout, à froid.
3. Le noyau entier (`idx90k-dict`, 22,5 M de textes) : le plan y est plus
   lourd encore ; si une cellule (un préfixe d'un octet) reste trop chère,
   la découpe par sous-plage a sa place *dans* le fan-out du nœud, sans
   attente au-dessus.
4. La mémoire du plan : les listes de candidats d'un préfixe court sur le
   noyau (centaines de milliers d'entrées) — plafonner, ou ne pas
   matérialiser les pièces non retenues (les comptes suffisent).

---

## 5. Où regarder d'abord

1. [09](09-journal-chantier-dictionnaire.md) §8 et §11 (ce qui a été
   essayé et pourquoi ça ne suffit pas), puis §6-7 (le modèle).
2. `composite.rs::find_literal_v3` de bout en bout : c'est lui à scinder.
3. `search_dag.rs` : où vit un nœud par shard.
4. `lucivy_core/tests/test_dictionary_index.rs` : la vérité (v3 contre v4,
   onze requêtes, documents et spans) — il doit rester vert à chaque pas ;
   et le panel `V3_SFX_VERSION=4` sur 10 000 puis 30 000.

Décisions en attente : le mode dictionnaire par défaut (après ce chantier
seulement), la version 4.0.0, la pile v2.
