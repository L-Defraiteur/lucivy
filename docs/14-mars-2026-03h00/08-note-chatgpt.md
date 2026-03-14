Bon. J’ai lu ton pavé. Tu pourrais presque publier ça comme papier de système d’IR et faire croire à des reviewers fatigués que c’est une “nouvelle structure d’index substrings”. Tragique pour eux, intéressant pour toi.

Réponse courte avant que je râle davantage : oui, c’est un très bon design.
Pas parfait, rien ne l’est, mais il est propre, cohérent et surtout économiquement intelligent côté mémoire et IO.

Maintenant la vraie analyse.

1. L’idée centrale est excellente

Le cœur du truc :

Suffix FST → redirection vers posting list du token parent

C’est exactement la bonne intuition.

Tu exploites un fait simple mais puissant :

un suffixe apparaît exactement aux mêmes positions que son token parent.

Donc :

posting(suffix) = posting(parent) + offset

Résultat :

aucune duplication de postings

aucune vérification texte

highlight direct

C’est structurellement élégant.

Ton ancienne approche :

trigram → candidats → vérification

C’était du probabilistic filtering.

Ton nouveau design :

suffix index → preuve directe

Donc :

zéro faux positif

pas de stored text

pas de token map

Ça change complètement le hot path.

2. La redirection FST → ._raw est exactement la bonne couche

Ton découplage :

._raw = vérité des tokens
.sfx  = index substrings

C’est très sain.

Ça donne :

query
 ↓
.sfx lookup
 ↓
._raw posting
 ↓
positions

Le coût supplémentaire :

+1 FST lookup

En pratique :

~50ns à ~200ns

Donc négligeable.

Et surtout ça t’évite :

350MB de postings dupliqués

C’est un trade parfait.

3. L’encodage u64 est très propre

Ton layout :

bit63 = multi parent flag

single parent:
  bits0..19  raw ordinal
  bits20..29 SI

multi parent:
  bits0..31 parent_list_offset

Très bon.

Pourquoi :

fast path sans branch dans 95% des cas

pas d’indirection mémoire

compact

Juste un détail que je corrigerais :

bits0..19 = 1M tokens

C’est limite si tu indexes beaucoup de code.

Perso je ferais :

bits0..23  raw ordinal  (16M tokens)
bits24..31 SI

Toujours 64 bits, mais plus safe.

Parce que dans les index code :

tokens uniques > 1M

arrive vite.

4. Le FST des suffixes va être plus gros que tu penses

Tu estimes :

15–20MB

C’est plausible, mais dépend énormément de la distribution des tokens.

Pour du code :

identifiers:
  getUserById
  getUserByName
  getUserProfile

Les suffixes partagent peu de préfixes.

Mais :

Le FST compresse les suffixes communs.

Ex :

framework
homework
network

→ work partagé.

Donc au final ton ordre de grandeur :

20–40MB

reste très raisonnable.

5. Le merge de multi-parents est correct

Ton algo :

merge de posting lists triées

Complexité :

O(total_postings)

C’est optimal.

Et tu as raison sur un point :

multi parents rares

C’est vrai.

Les collisions typiques :

"ing"
"ed"
"er"
"s"

Mais :

ces suffixes sont très courts, donc tu peux aussi les filtrer.

Option possible :

suffix length >= 2 ou 3

Ça évite :

"s"
"e"
"a"

qui génèrent beaucoup de parents.

6. Ton pipeline multi-token est très bien pensé

Ta règle :

premier token → .sfx (suffix)
milieu       → ._raw (exact)
dernier      → .sfx (prefix)

C’est exactement la stratégie utilisée dans plusieurs moteurs substring.

Parce que :

milieu d'un token ne peut pas matcher un mot complet

Donc tu simplifies énormément.

Ça évite les intersections absurdes.

7. Le DFA Levenshtein sur le suffix FST est malin

C’est une réutilisation directe du système fuzzy existant.

Tu transformes :

startsWith fuzzy

en :

contains fuzzy

juste en changeant le FST.

Très propre.

Et tu as raison sur :

d <= 2

Parce que :

d=3 substring

devient vite inutile.

8. GapMap : bonne idée mais optionnelle

La GapMap sert à vérifier :

séparateur exact

C’est utile si tu veux :

"foo bar"
≠
"foo_bar"

Mais beaucoup de moteurs ignorent ça.

Donc ta stratégie :

mode strict → GapMap
mode relaxed → positions seulement

est parfaite.

Et ça permet :

.sfx minimal = ~20MB

si on la désactive.

9. Le gain mémoire est réel

Ton estimation :

avant  : ~537MB
après  : ~70MB

Même si on corrige un peu :

.sfx FST ~30MB
GapMap ~50MB

tu restes autour de :

~85MB

contre 537MB.

Donc x6 plus petit.

Et surtout :

plus rapide.

10. Le vrai piège : l’indexation

Le seul point dangereux dans ton design :

génération de tous les suffixes

Pour un token longueur L :

L suffixes

Donc coût :

O(total_chars)

Mais ça reste raisonnable.

Ex :

token moyen = 6 chars
→ 5 suffixes

Rien de dramatique.

Juste penser à :

builder streaming

sinon RAM explode.

11. Une optimisation très simple possible

Limiter les suffixes courts :

par exemple :

SI >= 2

Donc indexer :

"rag3db"
"ag3db"
"g3db"
"3db"

mais pas :

"db"
"b"

Ça réduit :

taille FST

multi parents

bruit

Et la plupart des queries substring font ≥3 chars.

Verdict

Ton design est :

solide.

Il combine intelligemment :

FST

redirection

postings existants

offsets exacts

Le résultat est :

contains search exact
O(prefix_walk + posting_fetch)

sans vérification texte.

Franchement ?

Si tu implémentes ça proprement, ton moteur va avoir un contains substring plus rapide que beaucoup de moteurs existants.