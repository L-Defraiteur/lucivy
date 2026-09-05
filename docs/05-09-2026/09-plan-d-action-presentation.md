# Se vendre mieux — sur quoi, à qui, dans quel ordre (5 septembre 2026, tard)

Suite de `docs/28-08-2026/04-strategie-diffusion.md` (les canaux, la séquence,
le sujet tantivy), qui reste valable. Ce qui a changé depuis le 28 août et
qui change le discours : la taille est divisée par 3,7 (×21 → ×5,8 le texte,
×3,9 en RAM), la 4.0.0 est numérotée, le banc comparatif est **rejouable en
une commande** et jugé par le même scan des fichiers, et la vitrine a douze
corpus au prompt. En août on ne pouvait pas parler d'Elasticsearch sans
perdre ; aujourd'hui on peut, à condition de dire aussi où il gagne.

---

## 1. La phrase

**lucivy répond à la question que les autres ne peuvent pas poser, et prouve
chaque réponse.**

Tout le reste en découle : on ne vend pas « plus rapide », on vend **exact,
là où les autres rendent zéro en silence**, et **livré avec les positions,
dans votre transaction**.

---

## 2. Les six piliers, avec la preuve derrière chacun

Ordre de force. Chaque pilier a une phrase autorisée, une preuve qu'un
lecteur peut relancer, et la phrase interdite.

| # | pilier | phrase autorisée | preuve | interdit |
|---|---|---|---|---|
| 1 | **Exact, et prouvé contre le disque** | « chaque compte et chaque span est comparé à un scan des fichiers ; neuf lignes, zéro écart ; le banc échoue si une seule diffère » | `v3_ground_truth_demo`, 93 983 fichiers ; `compare_engines.md` §2 : Elasticsearch rend **0** sur `de` et 5 440 sur la regex (70 de moins), sans le dire | « les autres sont faux » — ils répondent à une autre question, le rapport le dit |
| 2 | **Trouve à travers les séparateurs et les jetons** | « `spinlock`, `spin_lock`, `spin lock` : la même chose ; 9 552 documents, contre 6 577 pour la seule formulation d'Elasticsearch ; `spinlokc` à deux fautes : 10 034 contre 3 549 » | `compare_engines.md` §3 | « plus de résultats » (ce n'est pas du rappel, c'est une autre question) |
| 3 | **Les positions livrées avec les documents** | « 20 797 spans dans 5 145 documents en 15 ms ; Elasticsearch, `highlight` sur 200 documents, 179 ms » | `compare_engines.md` §4 | rien : c'est le seul endroit où on gagne d'un ordre de grandeur |
| 4 | **Dans votre transaction** | « des fichiers immuables plus une méta écrite en dernier : l'index se pose sur votre base ou votre blob store, et le commit de vos lignes et celui de l'index sont le même commit ; le rollback emporte l'index » | `BlobStore` / `BlobShardStorage`, `create_with_blob_store` (Python), `BlobIndex` (Node), `lucivy::BlobBackend` (C++), `lucivy_fts` dans rag3db ; snapshots LUCE, deltas LUCID/LUCIDS | « ACID » seul, sans dire comment (c'est le store qui est ACID ; lucivy écrit dedans dans l'ordre qui le permet) |
| 5 | **Shardé et fédéré comme une bibliothèque, avec le bon BM25** | « N shards en parallèle, un top-k borné, et **les mêmes scores qu'un index unique** ; deux nœuds indépendants exportent leurs statistiques, les fusionnent, et un document score pareil sur son nœud que dans l'index qui aurait tout » | `test_federated_search.rs` : union = index unique **et** scores égaux ; `ExportableStats`, `search_with_global_stats`, le pré-filtre qui compose | « Elasticsearch ne sait pas faire de distribué » — il est distribué de naissance ; ce qu'il n'offre pas, c'est ça **dans une bibliothèque in-process**, et tantivy non plus (ses statistiques sont par index : deux index, deux échelles de scores, à recoller soi-même) |
| 6 | **Au prix d'Elasticsearch en taille, et dans un onglet** | « ×3,9 le texte en RAM, ×5,8 sur disque, contre ×3,6 pour l'Elasticsearch qui fait à peu près le même travail ; 39 044 fichiers TypeScript indexés dans un onglet en 33 s, la 2.6.0 entière en 28 s » | `compare_engines.md` §1 ; `04` §2 bis (les douze corpus) | « plus petit qu'Elasticsearch » (faux : ×1,08 en RAM, ×1,6 sur disque) |

Sur le pilier 5, le fond historique à raconter honnêtement, parce qu'il
explique le fork : tantivy agrège bien ses statistiques **à l'intérieur d'un
index** (le searcher voit tous les segments) ; dès qu'on a plusieurs index —
ce qu'on fait dès qu'on sharde soi-même — chaque index a son `N` et ses
`doc_freq`, et les scores de deux shards ne sont pas comparables. C'est ce
qu'il a fallu réécrire en mars (`docs/16-mars-2026-14h41/09`, `10`) : des
statistiques séparables, injectables, agrégées avant de scorer. Ce n'est pas
un défaut de tantivy, c'est une bibliothèque mono-index ; c'est la raison
d'être d'une couche au-dessus.

---

## 3. Ce que les autres font mieux, à laisser écrit

C'est ce qui rend le reste crédible. Le README l'a déjà ; ne pas le retirer.

- **Elasticsearch** répond à une sous-chaîne pure en 3 à 8 ms, lucivy en 12 à
  15. Sa phrase floue par `span_near` est juste (14 446 contre 14 449). Il est
  distribué, répliqué, opéré par des milliers d'équipes.
- **tantivy** indexe 857 Mo en 1,3 s (défaut) et 4,9 s (trigrammes), lucivy en
  56 s (v3) et ~255 s (dictionnaire). Son fuzzy par terme fait 16 ms là où
  notre d2 en fait 793 (sur une autre question). Il est la base de Quickwit.
- **Les deux** ont une communauté, des utilisateurs nommés, des années. Nous
  n'avons aucune preuve d'usage hors rag3db. C'est le trou numéro un du 28
  août, il l'est toujours.

---

## 4. Le tableau qui vend : « où ils trébuchent », plus deux lignes

Le tableau de `compare_engines.md` §3 est le seul comparatif qu'un lecteur
informé ne peut pas contester : il peut le relancer. À mettre sur la page
d'arrivée et en tête de la section comparative du README, avec deux lignes
qui ne se mesurent pas mais se constatent :

| demandé | vérité | lucivy | Elasticsearch | tantivy |
|---|---|---|---|---|
| `spin_lock`, séparateurs relâchés | 9 552 | **9 552** | 6 577 — inexprimable | 6 601 — ne sait qu'être relâché |
| `spinlokc`, deux fautes, à travers la frontière | 10 034 | **10 034** | 3 549 | 6 557 |
| `spin_lock_[a-z]+`, regex | 5 510 | **5 510** | 5 440, 480 ms | 0 |
| `de`, deux caractères | 93 009 | **93 009** | **0**, en silence | **0**, en silence |
| où ça matche, 5 145 documents | 20 797 spans | **tous, 15 ms** | 200 documents, 179 ms | 200 documents, 96 ms |
| votre index dans votre transaction | — | **oui** : store branchable, commit atomique, rollback | non : un serveur à côté, une synchronisation à écrire | non : son répertoire, son commit |
| shards et nœuds, mêmes scores qu'un index unique, en bibliothèque | — | **oui**, testé | oui, mais c'est un cluster | non : un index, une échelle |

La ligne « ce qu'ils font mieux » (§3) juste en dessous, en une phrase.

---

## 5. Plan d'action, dans l'ordre

Le principe du 28 août tient : d'abord la page d'arrivée, puis ce qui a de
la valeur en soi, et seulement après les communautés.

### 5.1 La page d'arrivée (une demi-journée)

- [ ] **Le titre.** « Search code the way you grep it » dit le geste, pas la
  différence. Proposition : *« Substring search across tokens — every answer
  checked against the files »*, ou en deux temps : le titre actuel, et un
  sous-titre qui porte le pilier 1.
- [ ] **Le tableau du §4** dans la page, sous « Numbers », avec la colonne
  vérité et le lien vers le rapport généré. Cinq lignes mesurées, deux
  constatées, une phrase pour ce qu'ils font mieux.
- [ ] **« Bring your own storage » remonté** d'une fonctionnalité parmi
  d'autres à un argument : « votre index dans votre transaction », avec le
  schéma en trois lignes (fichiers immuables, méta en dernier, rollback).
- [ ] **La fédération dite** : une phrase et le test qui l'affirme (« deux
  nœuds, statistiques fusionnées, scores égaux à l'index unique »).
- [ ] Le second acte est là (`index mdn`, `index linux`…) ; le chiffre en
  tête de page reste à poser (04 §2 bis).

### 5.2 Le README (une heure) — **fait, tard le 5**

- [x] Les **trois lignes** en haut (« Full-text search for code and technical
  text, as a library. Substrings, fuzzy and regex across token boundaries,
  BM25, exact byte spans — and every answer checked against the files. Runs
  in your process, in your transaction, in your browser. »), « What's new in
  4.0.0 » (taille ÷3,7, `shared_dictionary`, `derived_in_ram`, le banc, les
  corpus, le contrat), le 3.0.x renvoyé au CHANGELOG.
- [x] La section comparative reçoit les deux lignes constatées (transaction,
  fédération) avec ce qui les fonde.
- [x] **Les README des bindings reflètent le principal** : même accroche
  adaptée (Python, Node, C++, WASM), « What's new in 4.0.0 » avec les noms
  d'options propres à chacun, l'ancien « What's new » devenu « What 3.0.x
  brought », versions d'installation avec la mention « non publié, 3.0.8
  dernière » ; `lucivy_core/README.md` idem.
- [x] **`ARCHITECTURE.md`** (la page que Google montre en premier) : en-tête
  4.0.0, les quatre propriétés en tête, la table des fichiers au format 4.0
  avec les poids mesurés sur le noyau, le dictionnaire partagé, le tableau
  navigateur contre natif sur la 2.6.0, la table des bindings, une section
  « One corpus, one truth » avec le tableau des trébuchements.
- [ ] L'ordre des sections : Performance avant Query reference ? Le lecteur
  qui survole cherche « pourquoi », pas « comment ».

### 5.3 Publier (décision de Lucie)

- [ ] **4.0.0** : `PUBLISH_ENABLED` à vérifier (`gh`, avec son accord), `RELEASE.md`,
  crates en dernier ; puis **fusion de `v4` dans `main`**, qui met la page
  en ligne (`pages.yml` part de `main`) avec les corpus bâtis au déploiement.
- [ ] Le CHANGELOG 4.0.0 dit le contrat de compatibilité ; y ajouter deux
  lignes sur la taille (×21 → ×5,8) et le banc.

### 5.4 L'article (le premier à écrire, une journée)

Le 28 août disait : l'article du panel de vérité qui a trouvé le bug de
rappel. Il a maintenant un second acte plus fort : **« un corpus, une
vérité, trois moteurs »**. Ce qu'il raconte, dans l'ordre :

1. pourquoi un chiffre de vitesse ne dit rien sans la vérité à côté (les
   deux chiffres retirés en août, le panel qui a trouvé le bug) ;
2. le banc : mêmes fichiers, même scan, chaque moteur configuré au mieux ;
3. **la phrase de trigrammes de tantivy rend 0** — pas une mesure, une ligne
   de leur source (`position = 0`), et ce que ça implique : un ET de
   trigrammes, une vérification sur le texte, ce que lucivy fait en interne ;
4. Elasticsearch qui rend 0 sur deux caractères et 70 de moins sur une
   regex, sans avertir ;
5. où ils gagnent, chiffres devant ;
6. la commande pour tout relancer.

Ton du 28 août : créditer tantivy haut et clair (« fork de tantivy 0.26 »),
jamais « mieux que », toujours « ce que X ne fait pas, par conception ».

### 5.5 Les issues #12 et #15 (une heure)

- [ ] **#12 (benchmark vs tantivy)** : la réponse brouillonnée le 28 août
  disait « pas fait ». Il l'est en partie : `compare_engines.sh` publie
  taille, indexation, sous-chaîne, fuzzy, regex, positions, tantivy
  compris, avec la commande. Ce qui manque toujours : term et boolean sur
  `wiki.json` / `hdfs.json`. Le dire tel quel.
- [ ] **#15 (migration depuis tantivy)** : toujours pas fait ; le rapport
  donne au moins le tableau des différences de conception.

### 5.6 Les canaux (28 août §6-7, inchangé)

LinkedIn et This Week in Rust d'abord (sans risque), l'article puis r/rust et
r/programming, et **Show HN une seule fois**, quand la page, l'article et le
banc sont en ligne sur `main`. Bloquer une demi-journée pour répondre.

---

## 6. Ce qu'on ne dit pas, et les réserves à garder visibles

- Pas « plus rapide qu'Elasticsearch », pas « plus petit », pas « ils sont
  faux ». Les trois sont réfutables en une ligne du rapport.
- Pas « distribué » sans « en bibliothèque » : Elasticsearch l'est mieux que
  nous, c'est un cluster.
- Les temps d'Elasticsearch en §3 du rapport peuvent être des hits de cache
  (la même requête a tourné en §2) ; le rapport le dit, le dire aussi.
- Les temps d'indexation lucivy du README viennent des références du 08 (index
  réutilisés), sauf le v3 remesuré (56 s). Remesurer les deux autres avant
  publication si on les met en avant.
- Mot entier et préfixe : les comptes des trois moteurs sont proches mais pas
  égaux parce que chaque tokenizer définit le mot autrement. Ne pas les
  mettre en gras comme des écarts de rappel.
- `derived_in_ram` : ×3,9 en RAM, jamais le défaut, et pas dans le navigateur
  (pic mémoire, 04 §2 ter). Le dire comme une option.

---

## 7. Ce qu'il faut regarder ensuite

Le trou reste la preuve d'usage. rag3db est le seul utilisateur nommé. Un
deuxième projet tiers qui s'en sert, même petit, vaut plus qu'un tableau. Le
second acte de la vitrine (MDN, TypeScript, PostgreSQL dans l'onglet) est là
pour ça : donner à quelqu'un une raison de l'essayer sur *son* dépôt
(`index github owner/repo`) avant de lui demander de l'installer.
