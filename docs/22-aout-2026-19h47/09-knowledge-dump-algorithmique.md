# Knowledge dump algorithmique — index et requêtes du moteur SFX v3

Écrit de mémoire le 23 août 2026, sans relecture du code : ce que je sais après une
journée dedans. Les références de fichiers sont justes à la date d'écriture ; en cas
de doute, le code gagne. Lire après `08-rapport-23-aout.md`.

## 1. Ce qu'est l'index

Un moteur de **substring** : trouver `mutex` dans `pthread_mutex_lock`, avec les
octets exacts de chaque occurrence. L'index inversé classique (tantivy, hérité) sert
au BM25 et aux champs ; la recherche texte passe par le **SFX**, un FST de suffixes.

### Tokenizer (`tokenizer/equal_chunk.rs`)

Le texte est découpé en **mots** (runs de caractères « contenu », `is_content_char`
= alphanumérique au sens Unicode ; tout le reste est séparateur) puis chaque mot +
ses séparateurs suivants en **chunks** de ≤ 8 octets (`DEFAULT_MAX_TOKEN`), en parts
égales, alignés sur les frontières UTF-8. Un chunk porte `content_len`, `sep_len`,
`is_word_start`, `word_id`. Les séparateurs appartiennent au dernier chunk du mot
qui les précède ; une longue suite de séparateurs déborde en chunks « purs
séparateurs ». **Il ne doit jamais y avoir de chunk vide** (bug du jour : le snap
UTF-8 en produisait).

Chaque chunk a une **position** (index dans la séquence des chunks du document) et
des **octets** `[byte_from, byte_to)` dans le texte source. Offsets contigus :
`byte_from(p+1) = byte_from(p) + own_len(p)` au sein d'une valeur.

### Collector (`suffix_fst/collector_v3.rs`) et clés FST

Pour chaque chunk on interne un **token étendu** = texte du chunk (contenu +
séparateurs) + **overlap** = les 2 premiers octets du chunk suivant. L'overlap permet
à une requête qui déborde de 1-2 octets sur le chunk suivant d'être trouvée dans une
seule clé. Métas du token : `own_len` (contenu + seps), `sep_len`, `overlap_len`,
`is_word_start`.

Trois partitions de clés dans le FST (préfixe 1 octet) :
- `0x00` : suffixes commençant au début du token (SI = 0) → `anchor_start`, `term`.
- `0x01` : suffixes SI > 0 → contains « n'importe où ».
- `0x02` : **entrées word-stripped** : contenu du mot entier sans séparateurs +
  overlap de 2 octets de contenu du mot suivant ; suffixes jusqu'à
  `MAX_SUFFIX_INDEX = 256` octets, plus une entrée « tail » pour les mots > 264 octets
  (le reste est couvert par les chaînes chunk). Sert au mode relaxed.

Règle apprise le 23 août : une clé couvre plusieurs **formes** (`init` = mot `init` ou
`in`+overlap `it`), donc l'internement est clé par (texte, `content_len`) pour 0x02 et
(texte, `own_len`, `sep_len`, `is_word_start`) pour les chunks ; la fabrique FST
accepte plusieurs parents par clé. Le FST (`builder_v3.rs`) mappe chaque clé à un
u64 : soit un parent unique encodé inline (ordinal 24 bits, sti, own_len, sep_len,
overlap_len, flags), soit un offset vers une liste de parents (compteur u32, 11 octets
par parent). Ordinal = numéro du token étendu dans le segment.

### Fichiers par champ et par segment

| fichier | contenu | lecteur |
|---|---|---|
| `.sfx` | FST + table des listes de parents | `SfxFileReaderV3::open_owned` (zéro-copie sur mmap) |
| `.sfxpost` | postings chunk par ordinal : docs triés, payload vint (position, byte_from, byte_to) | `SfxPostReaderV2` (`resolve`, `resolve_doc`, `entry_at`) |
| `.word_sfxpost` (WSP2) | postings des entrées 0x02 : (doc, first_position, last_position, byte_from, byte_to = **fin de contenu**) | `WordSfxPostReader` |
| `.posmap` | inverse de `.sfxpost` : (doc, position) → ordinal | `PosMapReader::ordinal_at` |
| `.word_pos_map` (WMP2) | (doc, position de début de mot) → ordinal word \| span | `WordPosMapReader::word_start_at` |
| `.termtexts` | texte étendu et métas par ordinal | `TermTextsReaderV3::text/meta` |
| `.bytemap` | bitmap des octets présents par ordinal | filtre rapide (peu utilisé) |
| `.sibling_v3` | table des voisins (chunk suivant possible) | DFS de chaînes |
| `.freqmap`, `.chunk_word_map`, `.next_word_map` | **morts** (B5/B7), à supprimer |

### Merge (`indexer/sfx_dag_v3.rs`, `merge_dag.rs`)

Un merge v3 relit les termtexts de chaque segment source, ré-interne chaque token
(même clé de forme que le collector), remappe les ordinaux et les doc ids, concatène
les postings, reconstruit posmap/word_pos_map/sibling, et refait le FST. Il tourne
comme tâche luciole (`handle_start_merges`, réponse par `SuMergesDoneMsg`), jamais
en bloquant l'acteur. La policy (`LogMergePolicy`) est consultée au commit et en fin
de fusion ; `max_docs_before_merge` borne les entrées, `max_merged_docs` la sortie
(`LucivyHandle` : 10 000). Invariant testé : merge(A,B) ≡ index(A∪B) en spans.

Limites structurelles : 2^24 ordinaux par segment (50k docs kernel = 83 %), et un
chunk de 8 octets + overlap est quasi unique, donc les ordinaux croissent avec le
texte. Les gros segments sont mauvais partout ; 800 petits segments sont l'index le
plus rapide mesuré, la parallélisation est entre segments.

## 2. Contains (`briques/composite.rs::find_literal_v3`, `orchestrator.rs::contains_v3`)

Entrée : requête, `strict_separators`, `anchor_start`, `exact_match`. En relaxed la
requête est d'abord débarrassée de ses séparateurs.

1. **Singles** : `fst_candidates_v3` cherche la requête comme préfixe de clé dans les
   partitions (0x00/0x01, et 0x02 en relaxed). Chaque candidat = (ordinal, sti,
   métas). `resolve_single_v3` / `resolve_single_word_v3` résolvent les postings →
   `MatchV3 { doc, position, span, byte_from, byte_to, overlap_overflow }`. Pour 0x02,
   la fin de contenu vient du **posting** (WSP2), jamais des métas de la clé.
2. **Chaînes cross-chunk** : quand la requête est plus longue qu'un chunk +
   overlap. `falling_walk_chunks` marche le FST et émet des **splits** (tête
   consommant `query_consumed` octets jusqu'à `own_len`, reste de la requête) ;
   `cross_chunk_chain_from_splits` / `build_chains_from_splits` prolongent par les
   clés 0x00 du reste (avec mémo des remainders), le DFS sur la sibling table ajoute
   les continuations. Résultat : `TokenChainV3 { ordinals: Vec<Arc<Vec<u64>>>,
   first_sti, total_query_consumed, last_consumed }` — une liste d'ordinaux admis
   par position. Un split dont la clé contient toute la requête n'est pas une chaîne
   (match simple).
3. **Résolution des chaînes** (`resolve.rs`) : dirigée par **posmap**. Pour chaque
   posting de la tête, on demande `ordinal_at(doc, pos+1)` et on vérifie qu'il est
   dans la liste admise (dichotomie), etc. Les chaînes partageant une tête sont
   **groupées** (un balayage par posting de tête, dispatch par liste de queue
   distincte — c'est ce qui a fait `include` 35 s → 55 ms). Compteurs
   `n_posmap_mismatch` doit rester 0. En strict, un chemin **ancré sur le deuxième
   token** couvre les têtes courtes (`_` de `__init`) : candidats anchor_start du
   reste, puis vérification arrière de la tête via termtexts.
4. **Pipeline word** (relaxed) : même chose sur 0x02 avec `word_pos_map`
   (`resolve_word_chains_v3_wordmap_grouped`) ; les séparateurs entre mots sont
   ignorés ; `overlap_overflow` reporte les octets de la requête qui tombent dans le
   mot suivant, placés ensuite par l'orchestrateur via posmap/bytemap.
5. **Dedup** par (doc, position, byte_from) ; `exact_match` compare `token_end` ;
   `verify_literal` reconstruit une fenêtre (termtexts + posmap) et vérifie que le
   texte contient bien la requête — vérification, pas filtre.
6. Spans = `[byte_from, byte_to)` exacts ; highlights vers le `HighlightSink`.

Prescan : un `ContainsQueryV3` fait un prescan par segment **en parallèle** sur le
pool luciole (`build_scatter_dag`), dans `weight()` si le cache est vide ; résultats
(doc_tf, highlights) par segment ; BM25 global par `global_doc_freq`.

## 3. Fuzzy (`composite.rs::resolve_trigrams_v3`, `fuzzy_spans.rs`)

Levenshtein ≤ d, toujours relaxed (requête sans séparateurs, texte comparé sans
séparateurs).

1. **Candidats** (où regarder), trois générateurs, `V3_FUZZY_MODE` :
   - `ngram` : tous les n-grammes (n=2 si la requête est courte, sinon 3) résolus
     intégralement (singles chunk + word, échos dédupliqués), seuil pigeonhole
     `N − n·d` sur la région ;
   - `pivot` : seuls les `N − t + 1` n-grammes les plus rares (toute occurrence en
     contient un), seuil 1 ;
   - `pieces` : la requête coupée en d+1 pièces contiguës (une est intacte dans
     toute occurrence), chaque pièce résolue par `find_literal_v3`, partition à coût
     FST minimal ;
   - `auto` (défaut) : pièces si coût×2 ≤ coût pivot, sinon pivot.
2. **Régions** (`build_trigram_chains`) : hits d'un doc triés par octet, regroupés
   quand l'écart ≤ longueur de requête + d + slack (32 octets) ; positions min/max
   (un hit word porte la position du premier chunk de son mot). Pas de plafond par
   document.
3. **Fenêtre** (`rebuild_window_mapped`) : texte des positions de la région ±
   marge de `len + d + 1` **octets de contenu** (pas de positions : une suite de
   séparateurs est plusieurs chunks), minuscule, séparateurs retirés, avec
   back-map fenêtre → (offset source, longueur du caractère). Offsets **dérivés** par
   accumulation de `own_len` depuis une ancre (`resolve_doc_at`), une vérification
   en fin de fenêtre (`derive_miss` = 0).
4. **Vérification** : `within_edit_distance` (DP deux lignes, rejet précoce) puis
   `fuzzy_spans(needle, window, d)` — la **définition partagée** avec le harnais :
   DP semi-globale, une occurrence par plage d'extrémités à distance ≤ d, meilleure
   fin (distance min puis la plus à gauche), traceback match > suppression >
   substitution > insertion. Un span touchant un bord coupé est laissé à la fenêtre
   de sa propre région. Spans dédupliqués par doc, mappés à la source.

Sémantique à garder en tête : `__init` relaxed = `init` d=1 admet `int`, `unit`,
`inet` — 44 579 docs sur 50 000 est la vraie réponse. C'est une question
d'avertissement à l'utilisateur, pas d'algorithme.

## 4. Regex (`briques/regex_verified.rs`, `query/regex_query_v3.rs`)

1. **Plan** : `regex-syntax` parse le motif (sensible à la casse pour l'extraction,
   le contains étant déjà insensible) ; `Extractor` donne le jeu fini de **préfixes
   exacts** (sinon suffixes) ; `maximum_len()` dit si le motif est borné. Jeu vide
   possible (`[0-9]{8}`).
2. **Hits** : chaque littéral par `find_literal_v3` strict (le regex voit le texte
   brut, séparateurs compris).
3. **Fenêtres** :
   - motif borné par n : régions = hits à moins de 2n+2 octets, fenêtre = région +
     n+1 octets bruts de chaque côté, **casse d'origine**. Preuve : un match du
     fichier qui traverserait un bord contiendrait un hit d'une autre région à moins
     de n octets — exclu par la fusion. Donc `find_iter` sur la fenêtre ≡ sur le
     fichier (leftmost-first, non recouvrant compris).
   - motif non borné (`.*`, `+`) ou sans littéral : documents candidats (ou tous)
     reconstruits **entiers** (`rebuild_window_opts` de 0 à la fin, plafond de
     positions levé) et balayés une fois. Exact par construction ; c'est un grep
     restreint aux candidats (50k docs complets : 190 ms).
4. `regex::Regex` insensible à la casse, `find_iter`, matchs vides ignorés, spans
   mappés à la source, dédupliqués.

L'ancien chemin v3 (`regex_v3.rs`, `V3_REGEX_MODE=legacy`) est supprimé ; le
chemin vérifié est inconditionnel. `regex_gap_analyzer.rs` et `automaton_weight.rs`
restent : ils servent le chemin v2.

## 5. Luciole, en deux règles

- `execute_dag` exécute **inline** quand il est appelé depuis un acteur ou un thread
  du scheduler : pas de parallélisme réel à l'intérieur d'un handler. Pour un fan-out
  depuis un acteur : `submit_task` + continuation (`collect_replies_to`), jamais
  d'attente bloquante (emscripten).
- `build_scatter_dag` + `execute_dag` depuis un thread externe (le thread de
  recherche) donne le vrai parallélisme par segment : c'est le prescan de
  contains/fuzzy/regex (concurrence de pointe = cœurs). Une tâche qui échoue rend
  son nœud avec l'erreur et tous les receivers sont drainés (double free corrigé).

## 6. Invariants à ne pas perdre

- Fusionné ≡ frais, en spans (`v3_merge_equals_fresh_by_spans`).
- `n_posmap_mismatch`, `n_wordmap_mismatch`, `derive_miss` = 0.
- Aucun chunk vide ; aucun plafond silencieux (chaînes par doc, positions de fenêtre) ;
  toute limite refuse proprement ou avertit.
- La fin de contenu d'un mot vient du posting, jamais de la clé.
- Tout fichier d'un segment de l'inventaire est vivant pour le GC, quel que soit son
  meta.
- Le harnais assert les spans ; une requête vide ouvre chaque panel.
