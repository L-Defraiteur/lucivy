# Récap de progression et plan — fin de journée du 24 août 2026

Document autonome pour reprendre une session fraîche : où on en est, ce qui
est à faire tout de suite, ce qui vient après. Le détail commit par commit
est dans `01-rapport-progression.md` ; l'architecture dans `07-architecture.md` ;
les tests, benchs et points critiques dans `08-knowledge-dump-tests-benchs.md`.
Branche : `v3-recovery` HEAD `e8b5414` (la session rag3weaver y est pinnée) ; la
finalisation se fait sur `wip/publication-2.1.0`, fusionnée point complet par
point complet.

## 1. Où on en est

**Le moteur FTS v3 est en état de livraison.** Toutes les suites sont vertes
(lib 1415, lucivy-core 22 binaires hors 2 échecs pré-existants de
`bench_sharding`, luciole 169, sparse 62) ; les vérités terrain comparent des
spans à l'octet contre le disque (rag3db 4 600 fichiers, kernel 50 000
fichiers) ; les chiffres de référence 50k tiennent (plancher 25-27 ms,
`include` 46 ms, fz2 171 ms, regex ~200 ms).

**La journée a été pilotée par la migration de rag3weaver** (leur session,
docs 04 → 36 dans `rag3db/extension/rag3weaver/docs/23-aout-2026-20h33/`).
Ils sont sur le `ShardedHandle` v3 Rust direct, `BlobShardStorage` + leur
`CypherBlobStore`, toutes leurs suites e2e vertes, valgrind à 0, l'extension
C++ `lucivy_fts` supprimée chez eux. Ce dialogue a trouvé et fait corriger :

- `parse` inatteignable (code mort) → deux formes honnêtes, puis le booléen
  traduit en composite de `contains` avec highlights (`8f14edc`) ;
- le plancher de commit (fsync d'un cache jetable, `9a66fbf` ; puis
  `.managed.json` par commit et routage collant des docs, `e8ace07`/`8e2db07`) :
  9 docs / 2 shards de 733 ms à 5,6 ms ;
- le double free luciole (`ptr::read` des nœuds de DAG, `3675c3d`) et sa
  cause première, un scorer non monotone (`doc_tf` dans l'ordre d'un HashMap,
  `8a91053`) ; `Pool` tolérant aux workers partis (`a37d330`) ;
- un vrai trou v3 : le dernier mot d'une valeur sans séparateur final absent
  de la partition mots (`36b1edd`) — leurs `_content` n'ont jamais de `\n`
  final, ils étaient exposés ;
- un bug de chaîne (« clé avalante », `4d00531`) révélé par le routage collant.

**Le crate `sparse_vector` a rejoint le workspace** — copié depuis rag3db,
puis **réécrit** : cœur WAND original (`src/wand/`, écrit sur spécification
par un agent sans ouvrir les fichiers dérivés), fichiers dérivés de Qdrant
supprimés, crate **MIT** (audit ligne à ligne : 0-10 % de lignes communes,
triviales), perfs meilleures qu'avant (137 vs 147 µs RAM, 127 vs 154 mmap,
insert 3,2 s → 139 ms), et un `ShardedSparseHandle` sur la même
infrastructure que le FTS (routeur, pool luciole, storages lucistore).

**Le filtre pré-calculé paie enfin** (FTS et sparse) : seuls les shards qui
tiennent des `allowed_ids` travaillent, chacun sur sa part ; le sparse seek
les ids autorisés quand ils sont peu nombreux ; les ex æquo FTS sont
déterministes ; `node_ids_of(&results)` évite de recharger les documents.

## 2. À faire dans l'immédiat

1. **Publication crates.io 2.1.0** — `ld-lucivy`, `lucivy-core`, `luciole`
   0.2.0, `lucistore` 0.2.0, `sparse-vector` 0.3.0 (nouveau). rag3weaver vit
   sur `[patch.crates-io]` + chemins en attendant ; c'est ce qui les débloque
   pour livrer. **Préparé sur la branche `wip/publication-2.1.0`**
   (`fb7e2af`, 24 août soir) : versions et dépendances bumpées, READMEs
   `lucistore`/`sparse-vector`, exclusions du paquet `ld-lucivy` (394
   fichiers), CHANGELOG 2.1.0, imports morts nettoyés,
   `cargo publish --dry-run` des cinq crates dans l'ordre passe. Reste :
   le go, puis
   `cargo publish -p luciole -p lucistore -p ld-lucivy -p lucivy-core -p sparse-vector`
   (cargo ≥ 1.90 publie le lot dans l'ordre), tag `v2.1.0`, fusion dans
   `v3-recovery`.
2. **Emscripten** : le build passe (`lucivy.wasm` 8,5 Mo, emsdk 6.0.8 +
   nightly), l'exécution sous Node pend (main proxifié sorti, `ccall`
   orphelins). À tester dans son vrai habitat, le playground navigateur
   (`cd playground && node serve.mjs`) — le snapshot `playground/dataset.luce`
   est un **v2** (67 Mo commité, non régénéré exprès). Si ça pend aussi dans
   le navigateur : revoir `PROXY_TO_PTHREAD` / le cycle de vie du main.
3. ~~**Bindings natifs** : recompiler et rejouer les smokes~~ — fait le 24 au
   soir sur la branche wip : Python et Node recompilés, smokes étendus aux
   deux formes de `parse` (warnings, highlights, `AND` avec mot absent → 0),
   0 échec. Le script Node veut un chemin absolu vers une copie `.node`
   (en-tête du script).
4. **rag3weaver** : attendre leur retour sur le doc 36 (filtre routé) et le
   basculement de leur dépendance sparse sur `lucivy/sparse_vector`.

## 3. Après

5. **Publication v3 « officielle »** (PyPI `lucivy`, npm `lucivy`) une fois
   emscripten testé et les bindings rejoués ; changelog à partir de
   `01-rapport-progression.md` et du rapport du 23 août.
6. **Unification FTS / sparse** : un `Sharded<I: ShardIndex>` générique dans
   lucistore (phase stats pour le FTS, vide pour le sparse), le sparse porté
   dessus d'abord (petit), le FTS quand la fiabilisation est jugée finie ;
   ensuite un transport réseau commun — le distribué devient le même objet
   avec un autre transport. Design : `04-sparse-sharding-design.md`.
7. **Sparse, suite** : deltas LUCIDS incrémentaux (générations de postings),
   plafonds par bloc (l'élagage ne saute que 0,1-0,4 % des postings sur des
   listes longues), `tail_min` pour les poids négatifs, partage du seuil
   top-k entre shards.
8. **Perf FTS** : `verify_literal` = 40-70 % du CPU des requêtes à gros
   volume de spans ; deltas LUCIDS après grosses fusions (un segment fusionné
   repart entier — à borner côté policy si ça gêne).
9. **`is_content_char`** : tout non-ASCII est contenu (`→`, `«`, `—` sont des
   mots). Cohérent, sans perte, mais discutable ; changement de format si on
   y touche (bump de `v=` dans le harnais, STATS déjà versionné).
10. **BlobDirectory dans lucistore** (dépend du trait `Directory` de
    ld-lucivy — soit une variante fichiers plats, soit remonter le trait) pour
    que le sparse partage le cache lazy du FTS.

## 4. Règles de travail (rappel)

- Toujours `> /tmp/fichier.txt 2>&1` puis `grep`, jamais `| tail`.
- `cargo test -p lucivy-core --no-fail-fast` : sans, la suite s'arrête à
  `bench_sharding` et les binaires suivants ne tournent pas.
- Jamais de mention de Claude dans docs, code, commits ; docs en français,
  code et commentaires en anglais ; pas de trailer dans les messages de commit.
- Identité git : `user.email` local sur les 4 repos (la globale reste le
  compte de travail) ; SSH `Host github.com` → clé perso.
- Dossiers docs : `JJ-MM-AAAA`.
- Chaque changement notable → un doc pour la session rag3weaver dans leur
  dossier du jour (numérotation continue, 36 aujourd'hui).
