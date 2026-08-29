# Où en est lucivy, et pourquoi la promotion attend la taille d'index

Rapport de la journée du 28 août 2026, écrit pour être lu seul. Deux versions
publiées, un bug de rappel vieux de cinq versions corrigé, la synchronisation
incrémentale ouverte au navigateur — et une mesure qui met la communication
en pause.

---

## 1. Ce qui a été publié

**3.0.7**, dans la nuit du 27 au 28. **3.0.8** dans la journée. Les deux sur
PyPI, les six paquets npm, `lucivy-wasm`, les cinq crates et la release
GitHub. Le numéro de version est le même pour tout le workspace.

### 3.0.7 — le fuzzy relâché perdait des documents

Le défaut le plus sérieux trouvé jusqu'ici, et il datait du **23 août**
(commit `9866bc1`), donc présent dans **3.0.2 à 3.0.6**.

La génération de candidats fuzzy a deux implémentations et une estimation de
coût choisit entre elles. Celle qu'on appelle `pivot` tire ses candidats des
postings de trigrammes, qui n'existent qu'**à l'intérieur** des chunks d'un
token. Une occurrence dont tous les trigrammes partagés avec la requête
enjambent un séparateur n'a donc aucun posting : elle n'est jamais proposée,
et **son document n'est pas rendu du tout**. Pas un surlignage incomplet — un
résultat manquant, en silence.

Comme l'estimation bascule vers `pivot` quand l'index grossit, la perte
n'apparaissait qu'à l'échelle : `kvaser_usb_leaf.c` répondait exactement seul
et perdait quatre de ses cinq occurrences parmi trente-et-un fichiers.

**Correctif** : séparateurs relâchés ⇒ `pivot` exclu. La condition est connue
avant la recherche, elle n'a pas à être devinée. Séparateurs stricts ⇒
l'occurrence tient dans un token par définition, et l'estimation décide comme
avant. Ça n'a rien coûté : sur 93 605 fichiers, le chemin correct était déjà
le plus rapide (238,1 ms → 223,8 à distance 1 ; 990,0 → 878,9 à distance 2).

**Comment il a été trouvé** : par un panel neuf, `v3_ground_truth_demo`, dont
chaque ligne compare documents *et* spans d'octets à une lecture brute du
disque. `bench_sharding` ne pouvait pas le voir — toutes ses lignes affichent
« 20 hits » parce que 20 est le plafond de résultats. **Il chronométrait une
réponse que personne n'avait vérifiée.**

### 3.0.8 — la synchronisation incrémentale atteint le navigateur

`shardVersions()`, `exportShardedDelta()`, `applyShardedDelta()` sur
`LucivyIndex`. Les trois entrées C étaient compilées dans le wasm et listées
dans `EXPORTED_FUNCTIONS` depuis toujours, mais rien au-dessus ne les
appelait : un client navigateur ne pouvait prendre qu'un snapshot entier.

Ce qui devait être de la tuyauterie a révélé autre chose : `lucivy_create`
stockait le chemin **nu** du caller alors que l'index vivait sous
`/opfs/lucivy/<chemin>`, là où `lucivy_open` stockait la forme préfixée.
Toute entrée passant par `ctx.index_path` cherchait au mauvais endroit —
**l'export de snapshot était cassé** pour tout index créé dans la session, et
le worker le rapportait comme « uncommitted changes », son message par défaut
pour un retour nul. Une ligne.

Vérifié de bout en bout dans un vrai navigateur
(`playground/test_delta_sync.mjs`) : amorçage par snapshot, le serveur avance,
le client demande avec ses propres versions, applique, et répond la même
requête avec les mêmes résultats. 5 919 octets contre un snapshot de 7 499, et
209 octets pour un client déjà à jour.

**Et le wasm est enfin construit par la CI.** C'était le seul artefact produit
sur une machine de développement, non reproductible depuis un tag, et le
dernier paquet à réclamer un mot de passe à usage unique.

### La chaîne de publication

Tout part maintenant d'un `git push origin vX.Y.Z` : cinq plateformes de
wheels et d'addons, le wasm, puis PyPI, npm et crates.io — **en trusted
publishing partout, sans jeton ni OTP**. Le job crates.io est écrit et attend
seulement que les cinq publieurs de confiance soient déclarés sur crates.io.

Deux choses à savoir, découvertes en route :

- **L'environnement `release` n'a aucun réviseur requis.** Le commentaire du
  workflow promettait une approbation ; elle n'existe pas. Un tag poussé par
  erreur publie. `PUBLISH_ENABLED` (variable **de dépôt**) est le seul verrou.
- Les téléchargements npm et PyPI ont bondi (0-2 par jour, puis 300-450) —
  **exactement les jours de publication**, sans aucune annonce. Ce sont des
  miroirs, des caches et des scanners, pas des utilisateurs. Le premier jour
  sans publication tranchera.

---

## 2. Les six issues de mars, enfin traitées

`nicolas-geysse` avait ouvert six issues détaillées le 21 mars, en évaluant
lucivy pour remplacer tantivy dans un SaaS multi-tenant. **Cinq mois sans
réponse.** Quatre ont été répondues aujourd'hui : #11 (CI, faite), #13
(sharding, ACID, distribué, indexation navigateur — les quatre livrés), #14
(delta incrémental, livré depuis), #10 (la disposition à trois champs
n'existe plus en v3).

Deux restent ouvertes **volontairement**, parce qu'elles demandent des mesures
et pas des promesses : **#12** (benchmarks contre tantivy) et **#15** (guide de
migration). Trois engagements publics en découlent : mesurer l'ouverture d'un
index tantivy 0.25, publier le banc contre tantivy, et exposer le delta en
wasm — ce dernier est fait.

À noter, parce que ça s'est produit deux fois : j'ai posté une correction qui
était elle-même fausse. J'avais vérifié qu'un chemin de code existait sans
vérifier qu'il s'exécutait. C'est l'erreur exacte contre laquelle le panel de
vérité terrain a été écrit, commise trois heures après l'avoir écrit.

---

## 3. La mesure qui met la promotion en pause

Un banc de comparaison a été monté contre **Elasticsearch 8.19** et **tantivy
0.25** sur un corpus commun matérialisé de **93 983 fichiers du noyau**
(857 Mo de texte), avec `grep` comme arbitre. Le détail est dans
[06](06-comparaison-moteurs-mesures.md) ; voici ce qui commande la suite.

| moteur | index sur disque | rapport au texte |
|---|---|---|
| tantivy, tokenizer par défaut | 615 Mo | ×0,7 |
| tantivy, trigrammes | 681 Mo | ×0,8 |
| Elasticsearch, standard | 815 Mo | ×0,95 |
| Elasticsearch, trigrammes + `wildcard` | 3 083 Mo | ×3,6 |
| **lucivy** | **18 Go** | **×21** |

C'est **26 fois l'index trigramme de tantivy**, qui répond pourtant à la même
question de sous-chaîne.

Ce n'est pas un chiffre qu'on peut publier à côté d'une section comparative
sans que ce soit lui qu'on retienne. Et c'est précisément l'objection de
l'issue #10, dont l'auteur écrivait qu'une limite de volume rendait lucivy
inutilisable pour son service hébergé. Il avait raison de s'inquiéter.

**Décision : la promotion attend.** Pas par prudence excessive — parce que
publier « ×21 » quinze jours avant de le réduire, c'est offrir à chaque
lecteur le seul argument dont on ne veut pas.

---

## 4. L'ambition : réduire la taille d'index

### Ce que la mesure dit déjà

Sur l'index de 10 000 fichiers (41,5 Mo de texte, 1,2 Go non compacté) :

| fichier | poids | part |
|---|---|---|
| `.sfx` — la Suffix FST | **606 Mo** | 50 % |
| `.bytemap` | 159 Mo | 13 % |
| `.termtexts` | 87 Mo | 7 % |
| `.sfxpost` | 80 Mo | 7 % |
| `.word_sfxpost` | 60 Mo | 5 % |
| `.sibling_v3` | 45 Mo | 4 % |
| `.word_pos_map` | 31 Mo | 3 % |
| `.posmap` | 31 Mo | 3 % |
| `.store` — le texte stocké | 18 Mo | **1,5 %** |

**Le texte n'est pas ce qui pèse.** C'est la structure de suffixes, et la FST
en est la moitié.

**La compaction fait gagner 40 %** — 1,2 Go → 733 Mo à 24 segments — et rien
de plus. Le rapport reste d'un ordre de grandeur au-dessus des concurrents.
Ce n'est donc pas un problème de fragmentation.

### La piste

L'intuition de Lucie : **beaucoup de ces structures sont peut-être
reconstructibles à la lecture plutôt que stockées.** La question à instruire,
fichier par fichier, est toujours la même : ce que sa reconstruction coûte à
la requête, comparé à ce que son stockage coûte au disque.

Les candidats évidents sont les **tables de correspondance** — `.bytemap`,
`.posmap`, `.word_pos_map`, **221 Mo à elles trois, 18 %** de l'index. Elles
associent des positions à des octets ; si le texte est là (et il l'est,
18 Mo), une partie est peut-être recalculable.

La FST elle-même, à 50 %, est le vrai sujet. Les pistes classiques — partage
de préfixes déjà fait par la FST, compression des transitions, ne pas indexer
tous les suffixes mais un sur *k* avec vérification — sont à évaluer, et
chacune se paie en temps de requête.

### Comment mesurer les progrès

Le harnais existe et il est fait pour ça : `v3_ground_truth_demo` compare
**comptes et spans** à une lecture du disque, donc toute optimisation qui
casse la justesse se voit immédiatement. La discipline est :

1. mesurer la taille **et** faire tourner le panel,
2. deux passes, machine au repos, `uptime` vérifié,
3. ne publier un chiffre qu'accompagné de sa commande de reproduction.

**Objectif raisonnable à discuter** : passer de ×21 à ×5 rendrait lucivy
utilisable là où il ne l'est pas — 4 Go au lieu de 18 sur ce corpus. Ce n'est
pas un engagement, c'est le seuil à partir duquel la conversation avec
quelqu'un comme l'auteur de l'issue #10 redevient possible.

---

## 5. Ce qui attend la suite

- **Réduire la taille d'index** — le préalable à tout le reste.
- **Élucider les 70 documents** que la regex d'Elasticsearch manque sur le
  corpus commun (5 440 contre 5 510) : limite de longueur du champ `wildcard`,
  ou autre chose. À documenter précisément, pas à supposer.
- **Le chemin honnête pour tantivy** : candidats par ET de trigrammes puis
  vérification sur le texte stocké, chronométrée et mise à son compte. Le
  harnais est écrit, il manque cette étape.
- **Faire lire `last_search_truncated()` au panel** : sur une requête de deux
  caractères il affiche `FAIL` là où le moteur a atteint sa borne mémoire
  documentée et l'a signalé. Cinq minutes.
- **Puis** la section comparative du README, les issues #12 et #15, et le plan
  de diffusion ([04](04-strategie-diffusion.md)), dont la semaine 1 est déjà à
  moitié faite : le GIF est en tête du README, les trois lignes de
  positionnement et la section comparative restent.
