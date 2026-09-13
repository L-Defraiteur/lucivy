# L'indexation, profilée — 13 septembre 2026

Point de départ : le noyau entier (101 141 fichiers, 899 Mo) s'indexait en **94-97 s**
en dictionnaire, contre 5 s pour tantivy — le plus gros écart qui nous restait, et
jamais profilé depuis le repli différé du 6 septembre. Arrivée : **65,6 s**, mêmes
réponses, deux changements de quelques lignes chacun et un cache. Voici le chemin,
avec ses fausses pistes, parce qu'elles disent comment mesurer la prochaine fois.

## 1. Outils

- **`samply`** (installé, `cargo install samply`) refuse de tourner tant que
  `kernel.perf_event_paranoid` vaut 2 ; il faut `echo 1 | sudo tee
  /proc/sys/kernel/perf_event_paranoid` (ne survit pas au redémarrage). Pas fait
  cette session.
- **Le profileur du pauvre** qui a servi : `benches/gdb_sample.sh` lance le binaire
  sous gdb, lui envoie `SIGALRM` toutes les 250 ms, gdb l'intercepte (le programme ne
  le voit pas), note la pile de tous les fils et continue. `benches/gdb_top.py`
  agrège : temps propre, inclusif, par fil, par fenêtre de temps ; il compte à part les
  fils en attente (futex, condvar, park). `ptrace_scope` 1 suffit, le programme est
  l'enfant de gdb. Deux pièges rencontrés : `handle SIGALRM noprint` implique
  `nostop` (utiliser `stop print nopass`), et le pid à viser est l'enfant de gdb, qui
  met plusieurs secondes à charger les symboles.

```bash
CARGO_TARGET_DIR=~/lucivy_bench/target-prof CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
  cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth --no-run
benches/gdb_sample.sh samples.txt 0.25 400 -- <binaire> v3_ground_truth_demo --ignored --nocapture
python3 benches/gdb_top.py samples.txt --inclusive --top 60
python3 benches/gdb_top.py samples.txt --threads
```

- **Les compteurs du code** : `LUCIVY_VERBOSE=1` imprime par commit les recherches
  dans le dictionnaire (`[dictionary] commit: … lookups (… from the cache, … in a
  generation, … pending, … minted, … skipped by the filter): … ms, of which fst …,
  lock …`), les replis (`[dictionary] fold: …`), la finalisation de chaque segment
  (`[finalize] field 2: sfx build … ms`). Horodater chaque ligne (`python3 -c` qui
  préfixe `time.time()`) donne la chronologie sans profileur.

## 2. La leçon de méthode, d'abord

Mes premières références à 10 000 fichiers disaient 6,0 s. Après la réécriture du
collecteur : 4,7 s, « −21 % ». Un A/B propre par `git stash` a donné **4,8 s pour
l'ancien code aussi** : les 6,0 s étaient mesurés pendant qu'une compilation tournait
en fond (CPU 30,6 s au lieu de 23,5 pour le même binaire). Le mémo « sur machine
chargée, on mesure la charge » vaut pour mes propres tâches de fond. Règle appliquée
ensuite : **avant et après dans le même état, et l'ancien binaire rebâti** (attention :
`Cargo.lock` n'est pas suivi par git, un `git stash` qui le nomme échoue en bloc).

La réécriture du collecteur (table d'internement sans clé allouée, `into_data` sans
`BTreeMap` de clés formatées ni clones) est donc **neutre en temps**. Elle est gardée
comme simplification : la clé d'internement n'est plus stockée (le texte vivait deux
fois), plus de `BTreeMap`, et en mode v3 les ordinaux sont ordonnés par (texte, forme)
numériquement au lieu de l'ordre lexicographique d'un nombre décimal.

## 3. Ce que la chronologie a montré

À 10 000 fichiers (commit tous les 2 000), le profil par fil sur 40 échantillons :

- le fil principal passe **79 % du temps dans `commit`**, à attendre ;
- pendant ce temps le pool a ~4 fils occupés sur 24 : le flux (8 fils dans
  `add_value`) et la finalisation des segments (8 fils, puis une queue à 1-2)
  **alternent au lieu de se recouvrir** ; un commit est un arrêt du monde ;
- CPU : `add_value` 45 % dont `lookup_or_mint` 25-35 % (marche des parents dans la
  FST du dictionnaire 17 %), `finalize` 32 % dont `into_data` 21 %, désallocation 12 %.

Au noyau entier (commit tous les 10 000), les traces horodatées :

- **à partir du 3ᵉ commit, chaque commit replie le dictionnaire de façon synchrone**
  : 18 à 41 paires en attente, au-delà du plafond `LUCIVY_DICT_MAX_PENDING` = 16 ;
  1,8 à 2,9 s par commit, **10,2 s** pour celui qui compacte, 25 s en tout, plus 6 s de
  « nommage » — 31 s des 97 sur le chemin sériel ;
- les recherches dans le dictionnaire : **68,6 M de lookups**, 46,9 M de marches
  FST (un texte qui existe est cherché partie par partie), **4,1 µs la marche, 190 s
  de CPU** sur les 8 fils collecteurs ; 32,9 % de mintages, presque tous filtrés par le
  Bloom ; 57,9 % trouvés en génération, 9,2 % en attente ;
- le flux passe de 0,3 ms/doc à 30 000 fichiers à 0,7 ms/doc au noyau : plus de
  générations et de paires à traverser par recherche.

## 4. Ce qui a été fait, et ce que ça donne

| étape | 30 000 fichiers | noyau entier |
|---|---|---|
| avant | 19,5-20,0 s | 97,4 s |
| plafond de paires en attente 16 → **64** (`LUCIVY_DICT_MAX_PENDING`) | — | 75,9 s |
| + cache partagé des ids trouvés, vérifié | **14,2-14,8 s** | **65,6 s** |

Les temps du noyau comptent 3,4 s de persistance du harnais (copie RAM → disque).

1. **Plafond 64.** Un commit de 10 000 fichiers nomme 18 à 41 paires ; à 64 les
   replis restent en fond et les recherches ne sont pas plus lentes (fst 190 s de
   CPU dans les deux cas — les paires supplémentaires ne coûtent rien de mesurable).
   Sur WASM le repli est synchrone de toute façon (`LUCIVY_DICT_SYNC_FOLD`).
2. **Le cache partagé** (`dictionary::LookupCache`) : une table fixe de paires
   atomiques (hash de champ + clé d'internement → id), deux slots par hash, lue et
   écrite en `Relaxed`, **sans verrou**. Deux écrivains peuvent déchirer une paire, un
   texte récent en évince un ancien : sans importance, parce qu'un hit n'est utilisé
   qu'après `SfxDictionary::verify`, qui relit texte et forme de cet id dans
   `.termtexts`. **Le cache propose, le fichier décide** ; l'exactitude vient de la
   vérification, pas de la structure. 64 Mo par index en natif (4 M de slots, pages
   touchées au remplissage), 4 Mo en WASM. Ids stables à travers les replis : rien à
   vider. Résultat au noyau : 28,1 M de hits sur 39,7 M de textes trouvés en
   génération (71 %), marches FST 190 → 87 s de CPU.
   - Un cache **par fil** collecteur, essayé d'abord, ne voyait que 30 % des
     répétitions : le premier hit d'un texte sur chacun des huit fils marchait encore.
   - Le cache partagé **à verrous** avait été mesuré et refusé le 6 septembre ; c'est
     le verrou qui coûtait, pas le partage.
3. **Compteur d'ids atomique** (`fetch_add` par champ au lieu d'un mutex global par
   mintage) : neutre en temps mesuré, gardé parce que plus simple.
4. Essayé et retiré : trier les parties du dictionnaire « la plus grosse d'abord »
   (fst 22,8 → 26,3 s à 30k — la localité compte : les textes qu'un segment réutilise
   sont dans les parties les plus récentes) ; 64 stripes au lieu de 16 (rien).

## 5. Ce qui reste, par taille

Chronologie du noyau après (65,6 s) : flux jusqu'à 55,3 s pour 100 000 fichiers,
puis **compaction de 6 générations 10,5 s** dont ~5 sur le chemin critique, dernier
repli 1,3 s, persistance 3,4 s.

1. **La compaction** : fusion en flux sur un fil (deux avec `.termtexts`), 10,5 s
   pour ~14 M de textes ; celle qui tombe à la fin bloque la fermeture. Pistes :
   paralléliser par plages de clés (l'union FST reste séquentielle, les records
   non), ou déclencher plus tôt en fond pour qu'elle ne finisse pas dernière.
2. **Les recherches** : encore 166 s de CPU (87 fst, 26 verrou-et-travail sous
   verrou de mintage, le reste Bloom et vérification). La marche restante décode
   linéairement les groupes de parents d'une clé (jusqu'à 35 000 parents pour un
   texte fréquent) : un index de groupes à largeur fixe dans le record (format 9)
   en ferait une recherche binaire. Les 21,7 M de mintages coûtent chacun une
   allocation de clé et une insertion sous stripe.
3. **La finalisation d'un segment** (0,5-0,65 s pour 256 documents) enchaîne ses
   nœuds (préparation, FST, postings) en ligne sur le fil de la tâche ; deux tâches
   du scheduler raccourciraient la queue de chaque commit.
4. **L'arrêt du monde au commit** : le harnais commite depuis un seul fil, donc rien
   ne recouvre la finalisation. Fermer les segments par taille avant le commit
   (`WRITER_HEAP`) recouvre déjà en partie au noyau ; à 2 000 documents par commit,
   tout tombe sur le commit.
5. **La persistance** du harnais (3,4 s) n'est pas le moteur.

## 6. Vérification

- `cargo test --release --lib` : 1 471 verts (22 ignorés) ; sans features par
  défaut : 1 437 ; `lucivy-core` : toutes les suites vertes (dont `test_positions_off`,
  `test_compat_308`, fédéré, filtré, LUCE) ; C++ : 19 ; clippy : 0 erreur.
- Vérité terrain à 10 000 fichiers du noyau épinglé, panel de 10 requêtes (littérales
  strict et relâché, mot entier, préfixe, fuzzy d1 et d2, Jaro-Winkler, regex) :
  **10/10, spans exacts**, dans les trois modes — dictionnaire, sans positions, v3
  (celui dont l'ordre des ordinaux a changé).
- Navigateur (WASM rebâti) : playground `?corpus=corpus-kernel-10k.tar.gz`,
  dictionnaire par défaut, 10 000 fichiers indexés (index 1 103 Mo en mémoire, pic
  2 003 Mo), `mutex_lock` 968 documents comme avant le chantier, 968/968 spans exacts
  à l'octet sur le contenu rendu, 500 hits sans champs en 16 ms.
- Le cache ne change aucun octet des fichiers : il ne fait que raccourcir le chemin
  vers un id que la marche aurait trouvé, et un hit non vérifié reprend ce chemin.
