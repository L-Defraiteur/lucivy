PL trigramme: 



    "rag" → [doc 42, Ti= 0, ts=0, doc=58, etc.....]
    "ag3" → [doc 42, Ti=0, ts=1,    .... doc 87, ...]
    "g3d" → [doc 42, Ti=0, ts=1  .....doc 201, ...]
    "3db" → [doc 42, Ti=0, ts = 2 ...]


    (ici un numero dans la gapmap d'un doc, correspond a debut de séparateur en tant que Ti)
   GapMap -> per doc : 1-1, 5-2, 12-1...


    "aut" -> [doc 42, Ti= 2, ts=0, doc=58, etc.....]
    "utr" -> [doc 42, Ti= 2, ts=1, doc=58, etc.....]
    "tre" -> [doc 42, Ti=2, ts=2]

    .... etc

 indexation chemin trigramme:

 tant que reste 1 trigram après le premier d'un "token logique", je peux indexer le reste en fst aussi.

 exemple:
rag3db -> indexé en entier fst.
rag3dbIsACoolFramework -> fst
rag 3dbIsACoolFramework -> fst
rag3db IsACoolFramework -> fst
rag3dbIsA CoolFramework -> fst 
rag3dbIsACoo lFramework -> fst
rag3dbIsACoolFr amework -> fst
rag3dbIsACoolFrame work -> fst 
rag3dbIsACoolFramewor k -> pas fst 


Fst ici veut aussi dire stemming eventuellement sur chaque variant.

mot complet -> fst 

après premier trigramme -> fst 

si pu de trigram après -> pas fst.

