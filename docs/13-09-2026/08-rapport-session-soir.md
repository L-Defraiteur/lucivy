# Rapport de session — 13 septembre 2026, après-midi et soir

*Suite de `03-rapport-session.md` (le matin : 4.1.0, les doublons de spans, la CI,
le « 70 short », le banc épinglé, les spans à la demande). Ici : le document store,
le fetch des résultats, l'indexation ×2, et la 4.2.0 publiée — avec son incident.*

## 1. En une page

| sujet | avant | après | où |
|---|---|---|---|
| relire les documents d'une liste de hits (5 202 hits, 147 Mo) | 114 ms, séquentiel, même sans `fields` | 15 ms en parallèle, 2 ms sans champs | `06-document-store.md`, `ShardedHandle::fetch_docs` |
| indexation du noyau entier (101 141 fichiers) | 97,4 s | **48,2 s** | `07-indexation-profil.md` |
| compaction du dictionnaire (4 générations, 5,8 M de clés) | 8,4 s | 2,6 s, mêmes octets | `07` §5 bis |
| Linux 2.6.0, natif / navigateur | 23 s / 41 s | 9 s / 35 s | README « Browser against native » |
| comparatif, indexation lucivy (4 dispositions) | 94-112 s | 46-51 s | `docs/compare-engines-2026-09-13.md` |
| publication | 4.1.0 | **4.2.0** sur PyPI, npm ×6, wasm, crates.io ×5 + `lucivy-fst` 0.1.1 | `CLAUDE.md` § Packages |

Même index, mêmes réponses, mêmes temps de requête : mesuré sur les deux index du
noyau (matin, soir) avec le même binaire, mêmes comptes, tailles égales à 0,1 %.

## 2. Les chantiers, dans l'ordre

1. **Le document store** (`06`). Les 123 ms « de fetch » du harnais n'étaient pas le
   store : 147 Mo de texte relus en entier, séquentiellement, dans un processus neuf
   (décompression LZ4 48 ms, première touche du mmap). Les deux hypothèses du matin
   (cache de 4 blocs, relecture du document entier pour un champ) mesurées fausses.
   Ce qui était vrai : les quatre bindings relisaient le document entier pour lire
   `_node_id`, un fast field, et le fetch était séquentiel. Corrigé : fast field
   partout, `fetch_docs` parallèle par (shard, segment) au-delà de 64 hits, via
   luciole ; vérifié dans Chrome sur 10 000 fichiers (parallélisation ⇒ test WASM).
2. **L'indexation profilée** (`07`). Sans `perf` (paranoid 2), un sampler gdb
   (`benches/gdb_sample.sh` + `gdb_top.py`). Trois découvertes : chaque commit du
   noyau repliait le dictionnaire en synchrone (plafond de paires 16, il en nommait
   18 à 41 — 25 s des 97) ; 46,9 M de marches FST pour des textes existants (190 s de
   CPU) ; un commit est un arrêt du monde. Fait : plafond 64, **cache partagé sans
   verrou des ids trouvés, vérifié sur `.termtexts` avant usage** (71 % des marches
   en moins ; un cache par fil ne voyait que 30 % des répétitions, un cache partagé à
   verrous avait été refusé le 6 septembre — c'était le verrou), compteur d'ids
   atomique, collecteur sans clé allouée (neutre en temps, gardé).
3. **La compaction** (`07` §5 bis). Reproduite hors moteur
   (`bench_dict_compaction.rs`, liens symboliques vers les `dict-*`) : un
   tri-dédoublonnage inutile avant l'encodeur (ids disjoints entre parties), puis un
   pipeline union → deux encodeurs → écrivain qui ne gagnait rien tant qu'une
   allocation par clé faisait se battre les fils sur le verrou de malloc (27 % des
   échantillons) ; arènes par lot → 2,6 s, sortie identique à l'octet (empreintes
   comparées à chaque étape). Les replis passent par le même code.
4. **Les fils d'indexation** (`07` §5 ter). À budget SFX total fixe, plus de fils =
   segments plus petits (×2 de segments pour −11 %) ; à budgets par fil constants, 16
   fils font −17,5 % pour +17 % de segments, même pic mémoire, panel de requêtes égal
   sur l'index à 308 segments. Défauts : `MAX_NUM_THREAD` 16, tas et budget SFX **par
   fil**. Remarque de Lucie qui a tranché : moins de segments, moins de fils au prescan
   — d'où le rejeu systématique du panel sur toute forme de segments nouvelle (mémo).
5. **Mesures restantes** (`07` §5 quater) : à 16 fils, 203 s de CPU de recherches
   dont 52,7 s de balayage linéaire des groupes de parents (~3,3 s de mur) et 37-42 s
   sous le verrou de mintage (le travail sous le verrou, pas l'attente ; 64 stripes).
   Conception écrite du fichier dérivé `.pidx` (index des groupes, par défaut, format
   8 intact, reconstruit en RAM quand il manque).

## 3. Mise en valeur et publication

- Comparatif régénéré sur le noyau épinglé (lucivy rebâti, Elasticsearch réutilisé,
  tantivy rebâti) ; étiquettes 4.2 ; README, article (« Mine takes fifty »), page.
  Conséquence : le dictionnaire partagé ne coûte plus rien à l'indexation (47 s contre
  51 sans) — les phrases « ×1,5 » corrigées.
- Tableau « navigateur contre natif » remesuré sur Linux 2.6.0 (corpus extrait dans
  `~/lucivy_bench/linux-2.6.0/linux`) ; requêtes à chaud dans le navigateur.
- 4.2.0 : numéro partout, CHANGELOG daté, « What's new in 4.2 » dans les cinq README
  (celui de PyPI est `README.md`), architecture (indexation 4.2, résultats 4.2), WASM
  rebâti. Branche `v4.1` renommée `v4.2`, PR #17, fusion **rebasée par GitHub** (le
  tag a attendu la CI verte du nouveau SHA de `main`), tag `v4.2.0`.
- **Incident** : `ld-lucivy` a échoué sur crates.io — il compile contre le
  `lucivy-fst` 0.1.0 publié, sans `MapBuilder::with_registry` ajouté le jour même.
  PyPI, npm, wasm, `luciole`, `lucistore` étaient sortis. Correction (PR #18) :
  `lucivy-fst` 0.1.1 publié en premier, version par crate lue par `cargo metadata`,
  PyPI/npm/wasm sautent une version présente. Le premier dispatch de rattrapage a été
  refusé par l'environnement `release` (tags `v*` seulement) ; `main` ajoutée comme
  cible de déploiement ; second dispatch : tout vert, crates.io complet.

## 4. Décisions de Lucie

- Prouver les traversées de jetons (courtes, longues, à cheval) avant tout raccourci
  de la relecture sans positions ; l'indexation d'abord.
- Une optimisation pure est le défaut, pas une option — d'où le `.pidx` dérivé plutôt
  qu'un format 9 optionnel.
- Rejouer le panel de requêtes sur toute nouvelle forme de segments.
- Publier en 4.2.0 (pas 4.1.1), renommer la branche, PR puis tag sur CI verte ; `main`
  autorisée dans l'environnement de façon permanente plutôt qu'un tag jetable.

## 5. Erreurs de méthode, écrites pour ne pas les refaire

- Une référence de 6,0 s mesurée pendant une compilation de fond a fait croire à un
  gain de 21 % de la réécriture du collecteur ; l'A/B propre (ancien binaire rebâti,
  `git stash`) a donné 4,8 s des deux côtés. Mesurer avant et après dans le même état.
- `git stash push -- Cargo.lock` échoue en bloc (fichier non suivi) et laisse croire
  qu'on mesure l'ancien code.
- `ls target/release/deps/x-* | head -1` prend un binaire périmé : un `CARGO_TARGET_DIR`
  à part n'en garde qu'un.
- zsh ne découpe pas `$VAR` ni `set -- $spec` : scripts bash pour tout ce qui boucle.
- Un crate modifié doit changer de numéro **avant** le tag, la fourche FST comprise.

## 6. Ce qui reste

Par ordre : le `.pidx` (~3 s sur 48, sans format), le mintage sans `String` par clé,
le coût par document des collecteurs (jamais profilé au-delà de `add_value`), la
finalisation d'un segment en deux tâches, l'arrêt du monde au commit ; côté requête,
la vérification sans positions (avec le harnais des traversées), la regex à 220 ms.
Côté store : blocs plus petits ou store par champ, à mesurer sur des petits documents.
