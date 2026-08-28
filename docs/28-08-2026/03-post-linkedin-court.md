# Post LinkedIn — version courte, sous la limite

Le brouillon du [01](01-post-linkedin-3.0.7.md) faisait **4 172 caractères** ;
LinkedIn en accepte 3 000. Voici la version taillée, à **2796 caractères**,
soit 204 de marge — gardée volontairement, un comptage n'étant jamais
tout à fait le même des deux côtés.

(Numéroté 03 : le 02 est le rapport sur le bug de rappel du fuzzy.)

Le post est entre les deux séparateurs. **Tout ce qui est entre eux se colle
tel quel.**

---

Il y a quelques mois, j'ai ouvert le code de Lucivy, un moteur de recherche full-text BM25 écrit en Rust. Aujourd'hui, la v3.

La plupart des moteurs découpent le texte en tokens et comparent des mots entiers. Cherchez `mutex` : vous trouvez le mot `mutex`, pas `pthread_mutex_lock`, pas `getMutexHandle`. Ajoutez une faute de frappe, tout disparaît. Pour du code, des messages d'erreur, des clés de config, c'est exactement ce qu'on cherche qu'on ne trouve pas.

Lucivy indexe un Suffix FST : chaque suffixe de chaque token. La recherche de sous-chaîne devient aussi précise qu'une recherche exacte, elle traverse les frontières de mots, et elle rend les octets exacts qui ont matché — pas « ce document contient votre requête », mais ceux-là.

Et c'est vérifié, pas promis. Sur les 93 605 fichiers du noyau Linux, chaque réponse est comparée document par document ET span par span à une lecture brute des fichiers sur disque :

• `sched` en sous-chaîne : 9 327 fichiers, 53 336 occurrences, 78 ms — le scan naïf met 4 022 ms pour la même réponse.
• En mot entier, la même requête n'en trouve que 5 311. La différence, ce sont les `sched_clock`, `schedule`, `sched_domain`, que la plupart des moteurs ne rendent jamais.
• Avec deux fautes de frappe, `regsiter` sort 267 348 spans exacts en 879 ms.

À ma connaissance, aucune autre bibliothèque ne fait les deux. `grep` trouve les sous-chaînes, mais sans index ni classement — c'est la colonne de droite. Les moteurs full-text classent, mais ne cherchent pas à l'intérieur des tokens : ils rendent zéro sur la moitié de ces requêtes.

Aussi dans la v3 :

• Syntaxe booléenne : `kmalloc AND NOT kfree`, `"phrase exacte"`, parenthèses.
• Fuzzy au choix, Levenshtein ou Jaro-Winkler.
• Apportez votre propre stockage : votre base transactionnelle devient la source de vérité, le cache mmap redevient jetable.
• Recherche fédérée entre machines, aux mêmes scores qu'un index unique.
• Et ça tourne dans le navigateur, en WebAssembly.

Rien à installer pour essayer : le playground clone le code source de Lucivy et l'indexe dans votre onglet.

Essayez ici : L-Defraiteur/lucivy — playground
[ https://l-defraiteur.github.io/lucivy/ ]

- PyPI : `pip install lucivy`
[ https://pypi.org/project/lucivy/ ]

- npm : `npm install lucivy`
[ https://www.npmjs.com/package/lucivy ]

- npm (navigateur) : `npm install lucivy-wasm`
[ https://www.npmjs.com/package/lucivy-wasm ]

- crates.io : `cargo add lucivy-core`
[ https://crates.io/crates/lucivy-core ]

- Bibliothèque statique C++ (via CXX bridge)

Binaires précompilés pour Linux, macOS et Windows.

Le code, les issues et les PRs : L-Defraiteur/lucivy
[ https://github.com/L-Defraiteur/lucivy ]

Licence MIT. Fork de tantivy v0.26.0.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag

---

## Ce qui a été retiré, et pourquoi

**Gardé en entier** : le problème de départ (le tokenizer qui ne trouve pas
`pthread_mutex_lock`), les trois chiffres vérifiés du noyau, tous les liens.
Ce sont les seules parties que personne d'autre ne peut écrire.

**Compressé** :

| avant | après |
|---|---|
| Sept paragraphes « ce que la v3 apporte », un par fonctionnalité | cinq puces d'une ligne |
| « réécriture du cœur, pas une version d'entretien » | supprimé — l'annonce le dit déjà |
| Le détail du Suffix FST (partitionnement par position de départ) | une phrase |
| Les chiffres du navigateur (55 s, ×1,5) | « et ça tourne dans le navigateur » |
| Le détail de la fédération (export des statistiques, coordinateur, test d'égalité des scores) | « aux mêmes scores qu'un index unique » |
| « Binaires précompilés pour Linux (x86_64, aarch64), macOS (Intel, Apple Silicon) et Windows x86_64 » | « pour Linux, macOS et Windows » |
| L'appel final (« testez-le et dites-moi ce qui casse ») | supprimé — les hashtags closent |
| Le paragraphe sur rag3db | **supprimé entièrement** — il reste dans l'ombre |

**Sur « aucune autre bibliothèque ne fait ça »**

La phrase est dans le post, mais formulée pour tenir en commentaire. Un absolu
(« unique en son genre ») se fait démonter par le premier qui cite **Zoekt** —
l'index de trigrammes de Google, derrière Sourcegraph, qui fait de la regex
cross-token sur du code avec un classement. Il ne fait pas la même chose : pas
de BM25 sur des sous-chaînes, pas de spans exacts sur tous les modes, pas de
build navigateur. Mais il suffit à faire dérailler le fil.

La version retenue nomme les deux camps que le moteur bat plutôt que de se
déclarer seul au monde : `grep` cherche sans classer, les moteurs full-text
classent sans chercher dans les tokens. C'est vérifiable, ça s'appuie sur le
tableau juste au-dessus, et ça ne peut pas être contredit par un contre-exemple
partiel.

**Le principe de coupe** : un post LinkedIn n'a pas à être exhaustif, le dépôt
l'est. Chaque ligne retirée est une ligne que le lecteur intéressé trouvera en
cliquant. Les chiffres, eux, ne sont nulle part ailleurs sous cette forme —
c'est pour ça qu'ils restent.

## Ce qui reste à faire avant de coller

- **Retirer les backticks** si tu ne veux pas les voir : LinkedIn n'interprète
  pas le Markdown, `` `mutex` `` s'affiche avec ses accents graves. Les retirer
  coûte environ 40 caractères de moins, pas plus.
- **Mettre l'image** `images/bench-panel-verifie.png` — c'est elle qui montre
  les deux colonnes côte à côte, donc qu'on a comparé et pas seulement
  chronométré.
- **Vérifier les cinq liens** : ils doivent tous afficher 3.0.7.

## Version très courte — le lien seul, collé en bas

**1224 caractères.** Aucun lien dans le corps : l'URL du playground se met
en dernière ligne, seule, pour que LinkedIn en fasse sa carte d'aperçu. Une
seule URL dans un post, c'est aussi ce qui donne la meilleure vignette : quand
il y en a plusieurs, LinkedIn en choisit une, pas toujours celle qu'on veut.

Le post est entre les deux séparateurs.

---

Cherchez `mutex` dans un moteur de recherche : vous trouvez le mot `mutex`. Pas `pthread_mutex_lock`, pas `getMutexHandle`, pas `lockmutex`. Ajoutez une faute de frappe, tout disparaît. Pour du code, des messages d'erreur, des clés de config, c'est exactement ce qu'on cherche qu'on ne trouve pas.

Lucivy 3.0.7 les trouve : sous-chaînes, fautes de frappe et regex à travers les frontières de tokens, classées en BM25, avec les octets exacts qui ont matché.

Et c'est vérifié, pas promis. Sur les 93 605 fichiers du noyau Linux, `sched` en sous-chaîne trouve 9 327 fichiers et 53 336 occurrences en 78 ms — le scan naïf qui a validé cette réponse, octet par octet, met 4 022 ms. En mot entier, la même requête n'en trouve que 5 311 : la différence, ce sont les `sched_clock`, `schedule`, `sched_domain`.

`grep` cherche sans classer. Les moteurs full-text classent sans chercher à l'intérieur des tokens. À ma connaissance, aucune bibliothèque ne fait les deux.

Rust, Python, Node.js, C++ et dans le navigateur. Licence MIT.

Le playground clone le code source de Lucivy et l'indexe dans votre onglet — rien à installer, vous tapez vos propres requêtes.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag

---

**Ce qu'elle garde, et pourquoi ces trois-là :**

1. **L'échec concret** (`mutex` qui ne trouve pas `pthread_mutex_lock`) — le
   lecteur reconnaît le problème avant de savoir ce qu'est le produit.
2. **Un chiffre vérifié**, avec sa référence dans la même phrase : 78 ms contre
   4 022 ms. Le second nombre est ce qui rend le premier croyable.
3. **La phrase qui situe** face à `grep` et face aux moteurs full-text — elle
   dit ce qu'on est sans avoir à se déclarer unique.

Le reste (ACID, fédération, syntaxe booléenne, binaires précompilés) est absent
volontairement : à cette longueur, une liste de fonctionnalités dilue les trois
points ci-dessus au lieu de les renforcer.

**Avant de poster** : passer l'URL dans le Post Inspector de LinkedIn pour
purger son cache d'aperçu, sinon il resservira la version sans image qu'il a
vue plus tôt.
