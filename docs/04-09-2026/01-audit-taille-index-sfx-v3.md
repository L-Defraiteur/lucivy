# Audit de l'empreinte du format SFX v3 — disque et RAM

Audit du 4 septembre 2026, en lecture seule : le writer v3, tous les lecteurs,
le chemin de requête, la fusion, les `Directory`, et une mesure réelle sur
l'index de bench. Objectif : réduire le disque et la RAM **sans perdre une
fonctionnalité ni ralentir une requête**. Aucun code n'a été modifié, aucune
requête n'a été lancée : les effets annoncés sur le temps de requête sont des
raisonnements sur le code, à mesurer avant de conclure (§7).

**Base de mesure.** En plus de la table du rapport du 28 août
([07](../28-08-2026/07-rapport-progression-et-taille-index.md), 10 000
fichiers → 1,2 Go), l'index `~/lucivy_bench/lucivy_bench_sharding/single` a été
lu : 93 605 fichiers kernel (857 Mo de texte), **10 segments** (9 × 10 000
docs + 1 × 3 605), 2 champs SFX, **11,56 Go** (×13,5). Un segment (3 605 docs,
champ 2) a été scanné entièrement : décodage de la table de parents, des
postings, des varints de fratrie. Script : `benches/scan_index_size.py`
(Python, lecture seule), sortie brute dans
[`01-annexe-scan-single.txt`](01-annexe-scan-single.txt) :

```bash
python3 benches/scan_index_size.py ~/lucivy_bench/lucivy_bench_sharding/single 09fc9407784f44ebba5debeeca7a54cf
```

Les chiffres « segment scanné » viennent de là ; les formules viennent du code.

Répartition mesurée sur les 11,56 Go :

| fichier | Go | part | dont |
|---|---|---|---|
| `.sfx` | 4,92 | **42,5 %** | FST 1,24 Go (10,8 %) + **table de parents 3,58 Go (31 %)** |
| `.sfxpost` | 1,31 | 11,3 % | table d'offsets 158 Mo |
| `.bytemap` | 1,27 | 11,0 % | 39,6 M ordinaux × 32 o |
| `.word_sfxpost` | 1,12 | 9,7 % | table d'offsets 158 Mo |
| `.termtexts` | 0,69 | 6,0 % | offsets 158 + textes 298 + META 238 |
| `.posmap` | 0,67 | 5,8 % | 166,9 M positions × 4 o |
| `.word_pos_map` | 0,67 | 5,8 % | idem |
| `.sibling_v3` | 0,52 | 4,5 % | offsets 158 + entrées 359 |
| `.store` | 0,34 | 3,0 % | LZ4, blocs 16 Ko |

Le fait central que la table du 28 août ne pouvait pas montrer : **le `.sfx`
n'est pas surtout une FST ; c'est aux trois quarts une table de parents**
(`SECTION_PARENTS`, `file_v3.rs:23`), 325 millions d'entrées de 11 octets.

---

## 1. Inventaire exact de chaque fichier

### Vocabulaire commun

- Le tokenizer découpe chaque mot (contenu + son séparateur de queue) en
  **chunks de ≤ 8 octets** (`DEFAULT_MAX_TOKEN = 8`,
  `src/tokenizer/equal_chunk.rs:4`, `equal_chunks` `:158-200`). Le « token »
  indexé n'est pas un mot.
- Le collecteur étend chaque chunk des **2 premiers octets du chunk suivant**
  (`DEFAULT_OVERLAP = 2`, `collector_v3.rs:14`, `:185-205`) : texte étendu =
  contenu + sep + overlap, `own_len` = contenu + sep.
- Deux espaces d'ordinaux dans **un seul** espace numérique (`into_data`,
  `collector_v3.rs:610-700`) : les *chunks* (partitions FST 0x00/0x01,
  postings `.sfxpost`) et les *mots dépouillés* (partition 0x02, postings
  `.word_sfxpost`). Segment scanné : 1 182 705 ordinaux = 691 780 chunks +
  490 925 mots.
- Notation : C chunks distincts, W mots distincts, O = C+W, P positions
  (occurrences de chunks), Pw occurrences de mots, D docs. Segment scanné :
  C = 692 k, W = 491 k, P = 3,58 M, Pw = 2,92 M, D = 3 605 ; longueur moyenne
  du texte étendu d'un chunk 7,7 o (own_len ≈ 5,7), d'un mot 6,8 o
  (contenu ≈ 4,9).

### 1.1 `.sfx` — la Suffix FST et sa table de parents

Conteneur de sections (`section_file.rs:8-23`), magic `SFX3`, deux sections
(`file_v3.rs:21-23`).

**Clés** (`builder_v3.rs:256-320`, `add_token`) : pour chaque chunk distinct,
texte étendu **en minuscules**, une clé par suffixe `si ∈ [0, min(len, 256))`
sur frontière UTF-8 : préfixe `0x00` pour si=0, `0x01` sinon ; puis, si le
suffixe dépasse `own_len` (overlap présent, 99,9 % des chunks), **une seconde
clé « marqueur » tronquée à `own_len`** (`:299-310`) pour rendre final le nœud
à la frontière du token. Partition `0x02` (`add_word_stripped`, `:334-393`) :
contenu du mot entier (sans séparateurs) + overlap de contenu du mot suivant,
une clé par suffixe du contenu (≤ 256). `LUCIVY_MIN_SUFFIX_LEN` (défaut **1**,
`:21-25`) coupe les suffixes SI>0 plus courts — mais pas les marqueurs.

Segment scanné (estimé depuis META, formule du builder) : 5,34 M suffixes de
chunks + **3,95 M marqueurs** + 2,39 M suffixes de mots = 11,68 M entrées
(clé, parent). Soit **13,4 entrées par chunk distinct** et 4,9 par mot.

**Valeurs** (`builder_v3.rs:27-42`) : un `u64` par clé. Parent unique :
bit 63 = 0, puis `is_word_start` (1) | `overlap_len` (4) | `sep_len` (8) |
`own_len` (14) | `sti` (12) | `ordinal` (24). Parents multiples : bit 63 = 1 +
offset dans l'`OutputTable` (`lucivy-fst/src/output_table.rs:13-16`,
`[varint len][record]`). Un record = `u32 count` + **11 octets par parent** :
`u32 ordinal, u16 sti, u16 own_len, u8 sep_len, u8 overlap_len, u8 flags`
(`encode_parent_entries_v3`, `:150-166`), triés par `sti`.

Mesuré (segment scanné, champ 2) : 824 123 records, **9,22 M parents (79 %
des 11,68 M entrées)**, record max 317 383 parents ; ≈ 3,28 M clés
distinctes ; FST 41,1 Mo = **12,5 o/clé**, table 105,7 Mo = **11,45
o/parent**. Sur l'index entier : FST 1,24 Go, parents 3,58 Go. Les ordinaux
sont bornés à 2^24 par segment (`ORDINAL_MASK`, refus explicite `:468-478` et
`sfx_dag_v3.rs:522-528`), `sti` < 256 en pratique (`MAX_CHUNK_BYTES`, `:16`).

Formule : entrées ≈ C·(ℓ_ext + own_len) + W·ℓ_contenu ; taille ≈ 12,5 × clés
distinctes + 11,45 × parents partagés.

**Lecture** : `Map<OwnedBytes>` directement sur la tranche du fichier, zéro
copie (`file_v3.rs:86-118`) ; `decode_parents` alloue un `Vec` par appel
(`:127-136`). Natif : mmap (voir §4). Consommateurs : `fst_candidates_v3`
(scan de plage sur préfixe, `fst_walk.rs:124-190`),
`walk_partition`/`overlap_lookahead` (marche octet par octet, décodage des
parents à chaque nœud final, `:343-433`).

### 1.2 `.sfxpost` — postings de chunks (SFP3)

Par ordinal global (bloc vide de 2 octets pour les 41 % d'ordinaux qui sont
des mots) : `[varint num_docs][varint headers_len][12 o × checkpoints, 1 par
8 docs][en-têtes par doc : d_doc, payload_len, count en varints][payload :
(d_ti, d_bf, bt−bf) en varints]` (`sfxpost_v2.rs:12-24`, writer `:93-180`).
Table d'offsets `u32 × (O+1)`.

Mesuré : 3,58 M entrées, 1,65 M paires (ord, doc) ; **payload 4,42
o/entrée**, en-têtes 3,6 o/doc, checkpoints 0,83 Mo, offsets 4,73 Mo (16 % du
fichier, dont 1,96 Mo pour des blocs vides).

Formule : 4(O+1) + 4,4·P + 3,6·(paires ord,doc) + 1,5·(paires)/8·12.

Lecture : zéro copie sur `OwnedBytes` (`open_owned`, `:224-243`, le `to_vec`
de `open_slice` n'est utilisé qu'à la fusion) ; `resolve()` matérialise un
`Vec<PostingEntry>` (16 o/entrée) par ordinal (`posting_resolver.rs:170-178`) ;
`entry_at` binaire par checkpoints (`:352-363`).

### 1.3 `.word_sfxpost` — postings de mots (WSP3)

Par ordinal global (bloc vide pour les 59 % d'ordinaux chunks) : `[varint
n][16 o × checkpoints, 1 par 32][entrées : d_doc, d_first, last−first, d_from,
to−from]` (`word_sfxpost.rs:22-58`). Mesuré : 2,92 M entrées, **7,0
o/entrée**, offsets 4,73 Mo (19 % du fichier).

Sémantique (`:70-85`) : `first/last_position` = premier/dernier chunk du mot,
`byte_from` = début du premier chunk, `byte_to` = fin du **contenu**
(séparateurs exclus).

### 1.4 `.posmap` — (doc, position) → ordinal de chunk

`[PMAP][u32 num_docs][u64 × (D+1) offsets][u32 par position]`
(`posmap.rs:8-15`). **4 octets par position**, dense, `u32::MAX` aux positions
vides (frontières de valeurs). Mesuré : 3 583 099 slots = exactement les
3 583 076 entrées de `.sfxpost` (+ frontières), 0 slot vide (un seul champ par
doc). C'est **l'inverse exact** de `.sfxpost`, construit à partir de lui
(`index_registry.rs:203-236`, `posmap.rs:31-49`).

Consommateurs : adjacence stricte des chaînes (`resolve.rs:790-850`,
`:1185`), relaxée (`:632`, `:924`, `:1335`), **reconstruction de fenêtre
texte** pour la vérification fuzzy/regex (`composite.rs:875-1000`), placement
des débordements (`orchestrator.rs:36-58`).

### 1.5 `.word_pos_map` — (doc, position) → ordinal de mot qui commence ici

Même conteneur (`WMP2`, `word_pos_map.rs:19-28`), un slot `ordinal(24) |
span(8)` par position, `u32::MAX` si aucun mot ne commence là. Inverse exact
de `.word_sfxpost`, produit dans la même boucle (`collector_v3.rs:730-748`).
Mesuré : 18,6 % de slots vides ; span 0 (mot d'un seul chunk) : 81,5 % des
remplis, span 1 : 16,7 %. Lu par `resolve_word_chains_v3_posmap`
(`resolve.rs:610`, `:918`).

### 1.6 `.termtexts` — ordinal → texte étendu + méta (TTX3)

Sections (`termtexts_v3.rs:5-12`) : TEXTS = `u32 num` + **offsets u32 ×
(O+1)** + textes concaténés en **casse d'origine** ; META = **6 o par
ordinal** (`own_len u16, sep_len, overlap_len, is_word_start,
is_word_stripped`, `:136-149`) ; STATS 4 o. Mesuré : offsets 4,73 Mo, textes
8,67 Mo (7,3 o/ordinal), META 7,10 Mo. Les ordinaux sont attribués **en ordre
alphabétique** (`collector_v3.rs:640-676`, BTreeMap ; fusion
`sfx_dag_v3.rs:530-540`) : le préfixe commun avec le texte précédent totalise
**6,49 Mo sur 8,67 (75 %)**.

Lu partout : DFS de fratrie (`fst_walk.rs:747`, `:763`), reconstruction de
fenêtre, `may_have_long_words` (`context.rs:105`).

### 1.7 `.bytemap` — 256 bits par ordinal

`[BMAP][u32 n][32 o × n]` (`bytemap.rs:7-13`), calculé sur `token[..own_len]`
en casse d'origine (`index_registry.rs:196-211`). **Tous** ses appels au
chemin v3 se réduisent à une question : « ce chunk contient-il un octet de
contenu ? » (`bytes_in_ranges(ord, CONTENT_RANGES)`, `resolve.rs:642`, `:925`,
`:1339`, `orchestrator.rs:47`). Les deux autres consommateurs sont le
préfiltre DFA regex (`dfa_byte_filter.rs:21-49`,
`regex_gap_analyzer.rs:242-262`), qui n'ont besoin que de l'ensemble des
octets du texte. Mesuré : 387 k bitmaps distincts sur 1,18 M ; 5,2 %
d'ordinaux « pur séparateur ».

### 1.8 `.sibling_v3` — successeurs d'un ordinal (SIB2)

`[0xFFFFFFFF][SIB2][u32 n][u32 × (n+1) offsets][par ordinal : varint (Δnext
<< 1 | gap≠0), varint gap si ≠0]` (`sibling_table.rs:15-36`). `gap_len` n'est
**pas** un écart : c'est la longueur de contenu de la destination
(`collector_v3.rs:286-290`, `:405-409` ; commentaire `fst_walk.rs:751-758`).
Mesuré : 2,68 M entrées, 3,07 o/entrée, **le varint `gap` est présent sur
94 % des entrées et pèse 31 % des entrées** ; table d'offsets = 36 % du
fichier. Lu par `sibling_chain_dfs` (`fst_walk.rs:735`) : `siblings()` alloue
un `Vec` par appel (`:194-236`).

### 1.9 `.store`

Docstore LZ4, blocs de 16 Ko (`meta.json`). **Il ne sert pas aux
vérifications** : la fenêtre texte du fuzzy et de la regex est reconstruite
depuis `posmap` + `termtexts` (`composite.rs:875-1000`). Le store n'est lu
que pour rendre les documents.

---

## 2. Redondances et dérivabilités

| information | stockée dans | aussi stockée dans / dérivable de | coût de la dérivation à la requête |
|---|---|---|---|
| `own_len, sep_len, overlap_len, is_word_start` **par parent** | table de parents (5 o × 325 M) | `.termtexts` META, par ordinal (6 o × 39,6 M) | une lecture aléatoire de 6 o par parent ; sur les listes de 300 k parents (clés d'1-2 octets), ~300 k défauts de cache — **pas gratuit sur la marche** |
| les 2 octets d'overlap dans **chaque** suffixe et la clé marqueur | FST (clés) | `.termtexts` texte `[own_len..]` | une comparaison de 2 octets par candidat de coupure — les DFS lisent déjà `termtexts.text(next_ord)` |
| `bt − bf` d'un posting de chunk | `.sfxpost` payload (1 varint/entrée) | = `own_len` de l'ordinal (`collector_v3.rs:214-215` : `byte_to = offset + content_len + sep_len` ; `own_len = chunk_len`) | une lecture META par **ordinal résolu**, pas par entrée |
| `to − from` d'un posting de mot | `.word_sfxpost` (1 varint/entrée) | = `own_len − sep_len` de l'ordinal mot **depuis que l'internement est clé par longueur de contenu** (`collector_v3.rs:492-497`) | idem ; **hypothèse** : le commentaire `word_sfxpost.rs:8-12` affirme le contraire, il date d'avant ce changement |
| `gap_len` de fratrie | `.sibling_v3` (31 % des entrées) | META `own_len − sep_len` de `next_ordinal` | déjà lu dans la branche stricte (`fst_walk.rs:763`) |
| 256 bits de présence d'octets | `.bytemap` (32 o/ordinal) | contenu ⇔ `own_len − sep_len > 0` (META) ; ensemble d'octets = scan de `termtexts.text(ord)[..own_len]` (≤ 8 o pour un chunk) | équivalent ou moins cher qu'une lecture de 32 o dans un autre fichier |
| `.posmap` | 4 o/position | inverse de `.sfxpost` ; ou re-tokeniser le `.store` | inverse : balayage de tout le fichier — non ; store : décompresser 16 Ko + tokeniser ~9 Ko + ~1 000 lookups FST ≈ 0,4 ms/doc, × milliers de docs consultés par requête → **secondes**. À garder |
| `.word_pos_map` | 4 o/position | `posmap` + META (`is_word_start`) donnent *où* un mot commence, mais pas son ordinal (deux mots différents partagent un premier chunk) ; un lookup FST 0x02 par position consultée | ~10× une lecture de tableau, par match émis — plus lent sur le fuzzy |
| positions/octets des mots | `.word_sfxpost` | (doc, first_ti, bf) = le posting du premier chunk dans `.sfxpost` | 2 lookups par match de mot émis — plus lent sur les highlights |
| tables d'offsets `u32 × O` | `.sfxpost`, `.word_sfxpost`, `.sibling_v3`, `.termtexts` | 4 tables sur le même espace d'ordinaux = **634 Mo (5,5 %)** ; `.sfxpost` en paie 41 % pour des blocs vides, `.word_sfxpost` 59 % | — |

Largeurs fixes pour petites valeurs : `posmap` u32 pour des ordinaux ≤ 2^24
(25 % de gâchis, `builder_v3.rs:44`) ; META `own_len` u16 alors que
`own_len ≤ 8` pour un chunk ; `sti` u16 dans la table de parents pour < 256 ;
`u32 count` par record.

Stocké par *suffixe* alors que par *token* suffirait : les 5 champs de méta
répétés à chaque suffixe (§1.1), et les marqueurs, qui répètent le parent
entier (11 o ou 8 o inline) pour marquer une frontière que `own_len − sti`
connaît déjà.

---

## 3. La FST elle-même

**Ce qu'elle contient** : tous les suffixes de tous les *chunks étendus*
distincts (pas des mots — un chunk fait ≤ 10 octets), plus un marqueur par
suffixe qui déborde dans l'overlap, plus tous les suffixes de tous les mots
distincts. Ce n'est donc pas quadratique en longueur de mot ; c'est **13,4
entrées par chunk distinct et 4,9 par mot**, à ~11,5-12,5 o l'entrée. La
sortie porte une valeur **distincte par clé** (ordinal + sti + méta) : aucun
partage de sortie entre clés voisines, chaque clé finale paie ses 8 octets
(`output_pack_size`, `lucivy-fst/src/raw/node.rs:764-772`) ou son entrée de
table.

**Quelles recherches exigent l'entrée par suffixe arbitraire ?** `contains`
(scan de plage sur `0x01`), les trigrammes du fuzzy (`resolve_all_trigrams`,
`composite.rs:554-561` → `fst_candidates_v3`), les littéraux de la regex.
`anchor_start` n'utilise que `0x00` (`fst_walk.rs:139-141`). La marche
`falling_walk` n'a besoin que de : marcher le préfixe de la requête, détecter
un nœud final, décoder ses parents et savoir si `prefix_len ≥ own_len − sti`
(`:213-247`). **L'automate de Levenshtein n'est pas utilisé par le chemin
v3** : `levenshtein_automata` n'apparaît que dans `file.rs`, `fuzzy_query.rs`
(v2) et les tests de `term_dictionary.rs` ; le fuzzy v3 est pigeonhole de
trigrammes + vérification sur fenêtre. (Hypothèse forte : aucun
`automaton::`/`Automaton` dans `briques/` ni `*_v3.rs` hors filtre DFA regex
qui, lui, lit le bytemap.)

Évaluation des options :

**(a) FST des tokens seuls (SI=0) + structure séparée pour SI>0.** C'est ce
que la partition 0x00 fait déjà ; le coût est dans 0x01/0x02. Ne gagne rien
seul.

**(b) Suffixes à une position sur k, vérification sur le texte.** k=2
supprime la moitié des suffixes (pas les marqueurs) ; chaque lookup devient k
marches + une comparaison sur `termtexts` ; les trigrammes à offset impair
paient toujours la vérification. Gain ≈ −25 % d'entrées pour un coût M/L et
une marche plus complexe. Dominé par (e).

**(c) Suffix array sur le dictionnaire `termtexts`.** Un SA sur les *octets
propres* du dictionnaire (8,67 Mo → SA de 34,7 Mo en u32, + 8,67 Mo de copie
minuscule puisque `termtexts` garde la casse) remplacerait FST + parents
(146,8 Mo) par ~45-60 Mo : **−60 à −70 % du `.sfx`, ≈ −3 Go sur l'index
(−26 %)**. Une plage SA *est* la liste des (ordinal, sti) — la table de
parents disparaît par construction, et un « nœud final à own_len » devient
« entrées de la plage dont le suffixe a exactement `prefix_len` octets
propres ». Lookup O(|q| log n) ≈ 23 comparaisons contre une marche O(|q|) :
comparable ; énumération d'une plage = tranche contiguë, plus rapide que le
streaming FST. Ce qui change : `fst_walk.rs` entier (marche, look-ahead, scan
de plage), le builder, la fusion (le SA se reconstruit comme la FST
aujourd'hui, §5). Pas de perte de fonctionnalité identifiée (pas d'automate à
intersecter, la fratrie et le posmap sont indépendants). Coût **L**,
`sfx_version = 4`, et un bench avant/après obligatoire sur le panel de 21
requêtes et `v3_ground_truth_demo`.

**(d) Index dense au lieu d'offsets larges.** L'offset multi-parents (27 bits
pour 106 Mo) coûte ~4 o packés ; un index dense de record (20 bits) en
coûterait 3 : ~1 Mo sur 41. Négligeable. Ce sont les **valeurs inline à
parent unique** (62 bits utilisés → 8 o) qui pèsent : 2,46 M × 8 o ≈ 20 des
41 Mo de FST. Les ramener à `ordinal(24)|sti(8)` = 4 o gagnerait ~24 % de la
FST (≈ −300 Mo, −2,6 %) mais renvoie `own_len/overlap_len` vers META à chaque
parent de chaque nœud final — plus lent sur la marche des clés courtes (§2,
ligne 1). **Non recommandé** tel quel.

**(e) L'option que le code suggère : sortir l'overlap des clés.** Clé =
suffixe des octets *propres* (`text[si..own_len]`), overlap vérifié sur
`termtexts.text(ord)[own_len..]` (2 octets, déjà stockés). Conséquences
mécaniques : plus de marqueurs (la fin de clé *est* la frontière), plus de
suffixes en zone d'overlap (`sti ≥ own_len`, déjà rejetés par la marche
`fst_walk.rs:213`, et redondants pour le scan de plage puisque le chunk
suivant les porte), clés plus courtes de 2 octets. Entrées par chunk : **13,4
→ 5,7 (−57 %)** ; mots : 4,9 → idem mais clés −2 o. Total ≈ −46 % d'entrées,
donc **≈ −40 % de FST et −46 % de parents ≈ −2 Go (−17 %)**, et le pic RAM du
builder baisse d'autant (§4). Ce qui change : `add_token`/`add_word_stripped`,
`check_split` (`fst_walk.rs:213-247`, `:276-296` : `available`/
`overlap_consumed` se calculent contre `termtexts` au lieu du chemin FST), et
`fst_candidates_v3` doit accepter les candidats « la requête déborde de ≤ 2
octets » que la marche produit déjà (`prefix_len >= query_len → None`, `:219`,
à lever). Trigrammes traversant une frontière (« x_l ») : marche « x_ » → nœud
final → parents dont l'overlap commence par « l ». Highlights,
`resolve_single_v3` (`byte_from + sti`, `resolve.rs:150`) : inchangés.
`sort_and_dedup_splits` trie sur `overlap_validated` (`:323-330`) : à
recalculer à partir de la comparaison. Coût **M/L**, `sfx_version = 4`.

---

## 4. Mémoire à l'indexation et à la lecture

**Indexation.** Le collecteur tient, par segment : `token_intern` (clé =
texte + forme, deux copies du texte), `token_postings` (24 o/occurrence
estimés, `collector_v3.rs:27-39`), `word_postings` (28 o), `sibling_pairs`
(12 o), `word_stripped_entries` (deux `String` + 80 o par mot *occurrence*,
pas par mot distinct : `:445-455`). C'est `mem_usage()` (`:566`) contre
`LUCIVY_SFX_HEAP` (**1 Go natif / 128 Mo wasm**, divisé par les threads,
`indexer_actor.rs:33-45`) qui coupe un segment. `into_data` reclone :
`ord_map` avec clés `String` préfixées et `postings.clone()` deux fois
(`:606-666`). Puis `BuildFstV3Node` : `entries: Vec<(u32,u32,ParentEntryV3)>`
= 24 o par entrée (`raw_ordinal` est un `u64`, `builder_v3.rs:66`) +
`key_buf` (~5 o), puis `keyed` à 32 o par entrée pendant le tri (`:397-403`) :
**≈ 56 o × entrées**, soit ≈ 650 Mo pour le segment scanné de 3 605 docs
(11,68 M entrées) — l'ordre de grandeur du « 384 Mo demandés dans le
navigateur ». Deux gains directs : `raw_ordinal: u32` (−8 o/entrée) et (e)
(−46 % d'entrées).

**Lecture, natif.** Tout est zéro copie : `MmapDirectory` rend une
`OwnedBytes` sur l'`Arc<Mmap>` (`mmap_directory/mod.rs:387`),
`SfxFileReaderV3::open_owned` tranche sans copier (`file_v3.rs:99-118`),
`SfxPostReaderV2::open_owned` idem (`posting_resolver.rs:220-224`), tous les
autres lecteurs sont des `&[u8]` (`fuzzy_query_v3.rs:97-121`). Résident =
pages touchées. Rien n'est lu entier sauf le bitset alive
(`segment_reader.rs:306`) et le `termtexts` v2 (`contains_query_v3.rs:291`,
chemin v2 seulement). Le transitoire par requête : `resolve()` matérialise
16 o par posting, `siblings()` un `Vec` par appel, `decode_parents` un `Vec`
par nœud final, `CachedPrescan` 24 o par highlight jusqu'au plafond de 4 M.

**Lecture, WASM.** `LazyFsHandle` : lecture ≤ 64 Ko directe, sinon **le
fichier entier** entre dans un LRU global de 768 Mo
(`lucivy_core/src/directory.rs:110-124`, `:298-341`). Une requête qui touche
un `.sfx` de 147 Mo charge 147 Mo. Là, **chaque octet gagné sur disque est un
octet de RAM**, et c'est la table de parents (72 % du `.sfx`) qui remplit le
cache.

---

## 5. Ce que fait la fusion

`merge_segments_v3` (`sfx_dag_v3.rs:335-560`) ne re-tokenise rien et ne
fusionne pas les FST : elle **réinterne** les textes de `termtexts` (arène +
table ouverte, clé = forme + texte), remappe les postings de `.sfxpost` et
`.word_sfxpost` (doc et ordinal), remappe la fratrie, puis **reconstruit tout
depuis zéro** par le même DAG que la création (`build_initial_sfx_dag_v3`,
`merge_dag.rs:270-278`) : FST, table de parents, `.sfxpost`, `termtexts`,
`posmap`, `bytemap`, `word_pos_map` (dérivé, pas lu : `:457-459`). Deux textes
identiques de forme différente gardent deux ordinaux (`:400-430`).

**Pourquoi la compaction ne gagne que ~40 %.** Les fichiers « dictionnaire »
— `.sfx`, `.bytemap`, `.termtexts`, `.sibling_v3` = **7,4 Go, 64 %** — sont
proportionnels à la somme des ordinaux distincts *par segment* : un chunk
présent dans 10 segments y est 10 fois, avec ses 13 suffixes et sa méta. Les
fichiers « occurrences » — `.sfxpost`, `.word_sfxpost`, `.posmap`,
`.word_pos_map`, `.store` = **4,1 Go, 36 %** — ne bougent pas en fusionnant.
Le même corpus en 42 segments (`round_robin/`) fait 14,09 Go, en 32
(`token_aware/`) 13,71 Go, en 10 (`single/`) 11,56 Go. Et la fusion est
bornée : 2^24 ordinaux par segment (~3 M par 10 k docs kernel → ~50 k docs
par segment au plus, refus explicite `sfx_dag_v3.rs:522-528`).

---

## 6. Plan de réduction, classé par gain × sûreté

Gains en % de l'index de 11,56 Go (10 segments) ; ils se composent
approximativement.

| # | changement | gain estimé | requêtes | fonctionnalités | coût | format |
|---|---|---|---|---|---|---|
| **1** | **Table de parents : 11 o → le même `u64` que la valeur inline** (`encode_parent_entries_v3`, ordinal 24 + sti 12 + own_len 14 + sep 8 + overlap 4 + flag 1 = 63 bits). `count` en varint. | parents −27 % ≈ **−1,0 Go (−8,7 %)** | aucune : même décodage, moins d'octets lus par `decode_parents` | aucune | **S** (2 fonctions + test round-trip) | nouvelle section `0x03`, `VERSION` 4 du conteneur ; lecteurs acceptent les deux |
| **2** | **Supprimer `.bytemap`** : contenu ⇔ `own_len − sep_len > 0` (META) ; préfiltre DFA regex depuis `termtexts.text(ord)[..own_len]` ; `has_word_pipeline` ne l'exige plus | **−1,27 Go (−11 %)** | neutre ou plus rapide (un fichier de moins à faulter ; 6 o lus au lieu de 32) | aucune (`CONTENT_RANGES` ≡ `is_content_char`, `resolve.rs:509`/`equal_chunk.rs:103`) | **S/M** (5 sites + 2 fonctions regex + registre) | absence de fichier ; `written_for` en v4 ou drapeau |
| **3** | **Parents en delta-varint** : tri par ordinal, `Δordinal` varint + `sti` u8 + `own_len` varint + `sep_len` u8 + (`overlap`\|`ws`) u8 ≈ 5,5 o | parents −50 % ≈ **−1,8 Go (−16 %)**, remplace #1 | décodage séquentiel comme aujourd'hui ; à mesurer sur les listes de 300 k | aucune, **sauf** si un consommateur dépend de l'ordre par `sti` (non trouvé, à vérifier) | **M** | idem #1 |
| **4** | **Clés sans overlap, plus de marqueurs** (§3-e) | `.sfx` ≈ −40 % ≈ **−2,0 Go (−17 %)** avant #1/#3 ; RAM de build −46 % | marche plus courte de 2 octets, une comparaison de 2 o par coupure ; scan de plage identique | aucune identifiée ; à prouver par `v3_ground_truth_demo` (comptes **et** spans) | **M/L** | `sfx_version = 4` |
| 5 | `.termtexts` : META 6 → 4 o ; offsets u32 → longueur u8 + base par bloc de 16 ; front coding par blocs de 16 (75 % de préfixe commun) | −79 / −119 / −220 Mo ≈ **−3,6 %** au total | META/offsets : neutre ; front coding : `text()` devient ≤ 16 petites copies — **à mesurer**, la DFS de fratrie en dépend (56 % du fichier touché) | aucune | S / S / M | TTX4 |
| 6 | `.sibling_v3` sans `gap_len` (= META de la destination) ; table d'offsets en 2 niveaux | −110 Mo + ~−120 Mo ≈ **−2 %** | une lecture META de plus dans la branche relaxée (`fst_walk.rs:764-766`), déjà faite dans la stricte | aucune (`contiguous_siblings` n'est appelé que par les chemins v2) | S / M | SIB3 |
| 7 | `.sfxpost` : supprimer `bt − bf` (= `own_len`) ; `.word_sfxpost` : supprimer `to − from` (= `own_len − sep_len`, **hypothèse** à tester) | −167 Mo + −140 Mo ≈ **−2,6 %** | une lecture META par ordinal résolu | aucune si l'hypothèse tient | S | SFP4 / WSP4 |
| 8 | `.posmap` en 3 o/slot (ordinaux ≤ 2^24) | **−167 Mo (−1,4 %)** | lecture non alignée de 4 o + masque : neutre | aucune | S | PMAP2 |
| 9 | Deux espaces d'ordinaux (chunks / mots) → plus de blocs vides ni d'offsets inutiles dans `.sfxpost`/`.word_sfxpost` | ≈ **−200 Mo (−1,7 %)** | neutre | aucune | M (plomberie des ordinaux partout) | v4 |
| 10 | `LUCIVY_MIN_SUFFIX_LEN=3` (pas de code) | ≈ −20 % d'entrées, celles des plus grosses listes : ≈ **−700 Mo (−6 %)** | scans de plage plus courts | **risque** : une requête d'1-2 octets à la fin du dernier chunk d'une valeur (sans overlap) n'a plus de clé ; non couvert par un test trouvé | 0 | aucun |
| 11 | Suffix array à la place de FST + parents (§3-c) | ≈ **−3 Go (−26 %)**, rend #1/#3/#4 caducs | à mesurer ; plausiblement égal ou meilleur | aucune identifiée | **L** | v4 |
| — | *non recommandé* : dériver `.word_pos_map` ou `byte_from` des mots à la requête, dériver `.posmap` du store | −3 à −6 % | **plus lent** (lookups par match émis, ou décompression par doc) | — | — | — |

**Gratuit (encodage pur, aucun algorithme touché)** : #1, #5-META/offsets,
#7 (chunk), #8, #10. **Modèle de données** : #3 (ordre), #4, #9, #11.

Ordre proposé : **#1 + #2 + #8 + #6** (une journée, ~−22 %), puis **#4**
(c'est le vrai changement structurel, il baisse aussi la RAM de build et de
WASM), puis #3 ou directement #11 selon le bench. Cumulé #1..#8 sans #11 : de
l'ordre de **−45 à −50 %** de l'index, sans toucher aux occurrences ni au
store.

---

## 7. Ce qui n'a pas pu être vérifié

- **La table du 28 août (10 000 fichiers, 1,2 Go) et celle-ci (93 605
  fichiers, 11,56 Go) ne sont pas le même index** ; les parts sont proches
  (`.sfx` 50 % contre 42,5 %), mais la part de la table de parents *dans
  l'index du 28* est une extrapolation : 72 % mesurés sur le segment de 3 605
  docs et 73 % sur les 10 segments.
- **Aucune requête ni aucun bench n'a été lancé** : tous les effets
  « neutre / plus lent » sont des raisonnements sur le code, pas des mesures.
- Le nombre de clés distinctes de la FST (3,28 M) est déduit (entrées −
  parents partagés + records), pas compté ; la ligne de profil `[fst]`
  (`builder_v3.rs:495-503`) l'imprimerait.
- **Hypothèses** marquées comme telles : `to − from` de WSP3 dérivable de
  META (contredit par un commentaire antérieur au changement d'internement) ;
  aucun consommateur ne dépend de l'ordre par `sti` des parents ; l'automate
  de Levenshtein absent du chemin v3 ; l'exactitude de `MIN_SUFFIX_LEN=3` ; la
  survie de `overlap_lookahead` et du classement `overlap_validated` sous #4.
- Le « 40 % » de la compaction du 28 : le modèle 64/36 l'explique, il n'a pas
  été reproduit.
- Les ratios tantivy ×0,8 et Elasticsearch ×3,6 sont ceux du 28 ; ils n'ont
  pas été remesurés.

Fichiers clés : `src/suffix_fst/builder_v3.rs`, `file_v3.rs`,
`collector_v3.rs`, `sfxpost_v2.rs`, `word_sfxpost.rs`, `posmap.rs`,
`word_pos_map.rs`, `termtexts_v3.rs`, `bytemap.rs`, `sibling_table.rs`,
`index_registry.rs`, `briques/{fst_walk,resolve,composite,context,
orchestrator}.rs`, `src/indexer/sfx_dag_v3.rs`,
`src/query/posting_resolver.rs`, `lucivy_core/src/directory.rs`,
`src/directory/mmap_directory/mod.rs`, `lucivy-fst/src/output_table.rs`,
`lucivy-fst/src/raw/node.rs`.
