# Kaji — terminal simple, clair et léger

Demande du mainteneur, 2026-10-09 : privilégier le terminal ; interface esthétique
et minimale, compréhensible pour tout niveau ; performances maximales avec une
empreinte RAM et disque minimale. Les commandes doivent expliquer leur utilité.

Clarification ultérieure du mainteneur, même session : le poids du binaire est
secondaire si le runtime est très rapide, réactif et efficient en RAM. Le choix
full/lean doit donc reposer sur ces mesures, pas sur la taille seule.

## What problem would this solve?

L'accueil actuel pousse tout le catalogue de commandes et les raccourcis dans le
chat. Les descriptions de la palette mêlent actions, détails techniques et
variables d'environnement. Le rendu mesure deux fois chaque ligne de l'historique.
La compilation par défaut inclut plusieurs sous-systèmes optionnels lourds.

GitHub refuse la lecture des issues : les issues sont désactivées sur
`Taishi66/-Kaji-`. Ce document garde le périmètre local de cette reprise ; il ne
constitue pas une issue publiée. Type prévu : Feature.

## What would a good outcome look like?

- À l'ouverture : une invitation à décrire la tâche, les accès aux commandes et
  aux fichiers, le mode d'autorisation expliqué. Aucun manuel à parcourir.
- `/` : des descriptions courtes, formulées en actions. `/help` garde les commandes
  et les raccourcis complets. Les noms et comportements existants sont conservés.
- Un seul calcul de hauteur par ligne rendue, partagé entre les positions des
  tours et le défilement ; le loader reste inclus dans la mesure.
- Une commande de compilation allégée explicite, avec ses fonctionnalités et
  omissions documentées. Le profil existant reste disponible.
- Un README propre à Kaji, avec un démarrage local et une reprise de session
  compréhensibles.

## Possible approaches

Séparer l'accueil de l'aide complète. Utiliser `COMMANDS` comme source unique des
descriptions courtes. Réutiliser la somme des hauteurs déjà calculée pour les
positions de tours. Compiler une variante sans les features optionnelles
code-mode, local-inference, aws-providers, nostr et otel, en gardant TUI, TLS,
trousseau système et mise à jour. Placer son artefact dans `target/lean` avec un
profil dédié pour éviter de remplacer l'artefact complet.
Ce profil hérite de `release`, active Thin LTO et retire les informations de debug ;
le coût de compilation et l'effet sur l'artefact restent à mesurer.

Contraintes : aucune dépendance ajoutée ; conserver les changements locaux
préexistants ; aucun changement du moteur agent ou du prompt, donc aucune nouvelle
source d'état à servir au replay. Respecter les contrôles d'autorisation existants.

Non-objectifs de ce lot : desktop, renommage des commandes, modification des
permissions, cache de l'historique, migration du moteur, modification des defaults
de distribution, suppression de fichiers ou composants installés.

## Additional context

Références : ADR IPC du vault du 2026-08-08 (TUI in-process, desktop Tauri sur ACP),
specs TUI palette et lisibilité du 2026-08-11. La demande de simplicité du
2026-10-09 remplace le catalogue exhaustif au démarrage ; l'aide garde ce catalogue.

Vérification prévue : rendu TestBackend de l'accueil en 80 × 24, absence de
défilement initial, mode et accès à l'aide visibles ; tests existants de reprise,
palette, styles, loader, Unicode et navigation entre tours. Ajouter une vérification
de rendu à la recette d'auto-test. Formatage par fichier et revue du diff pendant
cette session. Builds, tests et clippy seulement sur demande, conformément à
AGENTS.md ; ce document ne vaut pas autorisation de les exécuter.

Pour évaluer l'objectif concurrentiel, mesurer ensuite sur le même matériel et
avec le même modèle : démarrage, RSS au repos et après un long historique, taille
de l'artefact, latence de saisie/rendu, réussite de tâches et tokens consommés.
Aucun gain chiffré ni supériorité sur Pi, OpenCode, Claude Code ou Codex n'est
affirmé sans ces mesures. Les budgets chiffrés restent à fixer après une baseline.

## Suite demandée — clarté de l'état et palette compacte

Le mainteneur demande de continuer et de mettre à jour la roadmap. La roadmap
canonique (last_update 2026-09-07) donne déjà priorité au CLI/TUI avant le desktop
et précise que les kanji sont des accents, jamais les seuls porteurs de sens.

Lot suivant : nom du mode toujours visible (l'accent de changement reste temporaire),
police standard par défaut avec Nerd Font en option, labels `tokens`, `agents` et
`thinking`, conservation de l'activité lorsque la télémétrie doit se réduire.
Palette limitée à six propositions visibles, sélection suivie au défilement,
descriptions sur une seconde ligne en colonne étroite et troncature explicite en
cellules Unicode. Les commandes et leurs raccourcis gardent leurs comportements.

Vérifications à ajouter : mode nommé dans les quatre états au repos, absence de
glyphes PUA par défaut, activité conservée sur une barre étroite, aucune cellule
hors de la palette, sélection visible en fin de liste et descriptions compactes
lisibles. Mise à jour de la roadmap canonique à partir des changements réellement
écrits ; builds, tests, clippy et validation visuelle restent à exécuter sur demande.

## Suite demandée — coût du rendu Markdown

Le lot suivant traite le reparsing et la mesure des réponses agent inchangées.
Cache purement TUI : au plus 32 réponses, 128 Kio de données retenues (texte source,
vecteurs de lignes/spans et chaînes possédées), plus métadonnées bornées de la map.
Les réponses trop grandes sont rendues sans être retenues. Aucun fichier, prompt,
événement de session ou dépendance supplémentaire.

La clé de validation utilise le contenu exact, la largeur de chat et le thème.
Les indices de chat ne suffisent pas : une restauration ou une modification au
même index doit rendre le nouveau contenu. La réduction/vidange du chat purge
le cache. Priorité aux indices récents plutôt que LRU : un parcours séquentiel
d'un historique plus grand que le cache ne doit pas évincer chaque réponse utile.

Le cache garde le rendu sans curseur et ses hauteurs. Le curseur animé est ajouté
après la lecture ; seule la hauteur de sa dernière ligne est remesurée. Les outils,
rapports et loaders restent dynamiques. Le rendu final ratatui garde son wrapping
et sa navigation actuels ; cette étape ne virtualise pas tout l'historique.

Vérifications prévues : cache hit sans reconstruction, modification de texte au
même index, resize, changement de thème et restauration ; dépassement du budget
et absence de thrashing ; équivalence de buffers à froid/chaud pour Markdown,
Unicode et tableaux ; curseur, streaming et positions des tours identiques.
Mesurer ensuite la différence de coût CPU et RSS. L'optimisation est écrite avant
toute affirmation de gain. Le mainteneur a explicitement autorisé compilation,
tests et clippy le 2026-10-09. Le test de mesure ignoré compare des dessins complets
du chat TestBackend en 80×24, avec et sans rétention, sur 32 et 128 réponses ;
200 frames par variante, ordre alterné pour réduire le biais de chauffe.

Incident rencontré lors de la validation : Rust 1.96.1 sur macOS 27 produit une
dylib `sqlx_macros` rejetée (`mis-aligned LINKEDIT string pool`) lorsque la
suppression du debuginfo est appliquée aux composants de compilation. Exemption
`strip = "none"` pour `sqlx-macros` en dev et les build-dependencies en lean ; le binaire lean
garde Thin LTO et `strip = "debuginfo"`. Cela corrige le chemin de compilation,
sans modification du moteur ou ajout de dépendance.
Références : [bug Rust #157750](https://github.com/rust-lang/rust/issues/157750),
[règles des profils Cargo](https://doc.rust-lang.org/cargo/reference/profiles.html).

## Reprise et priorité sécurité

Le mainteneur demande une robustesse de sécurité maximale. La validation inclut
les refus d'outils, la portée des autorisations et l'affichage des commandes
hostiles. Aucun mode de permission n'est assoupli. Les contrôles du moteur
doivent rester identiques dans les chemins legacy et state-machine.

Deux corrections issues du terminal réel : une sélection de fichier ferme la
complétion pour que l'Entrée suivante envoie le message ; un dossier continue à
afficher ses enfants. La confirmation d'outil utilise davantage de place pour
montrer les réponses et la portée d'une autorisation sans ouvrir le détail.
Régressions prévues : Tab/Entrée sur un fichier puis envoi ; commande et choix
de refus visibles en 80×24 et 40×24 ; maintien des tests anti-masquage existants.

La sécurité complète nécessite un audit distinct : exécution shell, frontières
MCP et réseau, secrets, fichiers et dépendances. Une interface ou une suite de
tests ne constitue pas une garantie d'absence de vulnérabilités.

L'auto-test réel a rencontré un modèle fournisseur retiré : erreur typée affichée,
mais code de sortie 0. Extension du lot à la CLI commune, sans modifier les deux
moteurs : une erreur finale typée ou une erreur de stream doit faire échouer le
run non interactif avant les sorties JSON `completed`/`complete`. Une erreur
antérieure suivie d'une récupération réussie reste une réussite. Tests de ce
contrat et vérification par le binaire sur un fournisseur local en erreur, dans
les chemins legacy et state-machine ; aucune recherche heuristique dans le texte.

Le terminal réel a aussi confirmé le bruit `<turn-context>` déjà au backlog lors
de la reprise. La vue masque les messages explicitement marqués par la métadonnée
`turn_context`, en live, au resume et au restore. Le contenu utilisateur contenant
ces balises reste visible. Historique persisté, prompt et replay restent intacts.

## Suite demandée — code, chat et documents dans le terminal

Le mainteneur choisit le rendu des documents dans le terminal, adapté à ses
capacités graphiques. Aucun lancement automatique d'application externe.

Premier lot : suggestions de prochaine question désactivées par défaut et
activables par `/suggest on|off` ; contexte récent de la vue courante, chronologique
et borné ; une seule requête annexe possédée par la boucle, annulée lors d'une
édition ou d'un nouveau tour et à la sortie. Échecs et délais doivent finir le
chargement. Aucune modification du prompt principal ou de l'historique/replay.

Lecteur : `/open <path>` pour consulter sans envoyer au modèle ; lecture réservée
aux fichiers réguliers, budgets distincts sur les octets source, le texte affiché
et le nombre de lignes. Rendu limité au viewport sans recopier les longues lignes.
Textes UTF-8/UTF-16 ; cartes explicites pour les formats non rendus. Les lecteurs
PDF/Office et images seront introduits selon des budgets vérifiables, sans
décompression illimitée dans le processus de l'interface. Les fichiers spéciaux
et les contenus hostiles doivent avoir des régressions avant de revendiquer une
couverture. Une limite dépassée doit être affichée, jamais passée sous silence.

Validation : suite CLI lean, clippy strict, cas hostiles et terminal réel avec un
fournisseur local déterministe. Mesurer le nombre d'appels annexes ; les anciens
binaires optimisés et leurs mesures restent la baseline du lot précédent.

Implémentation du lot documents : réutiliser `image 0.24.9`, `quick-xml 0.41.0`
et `libc 0.2.186` déjà présents dans le lockfile, ajoutés via `cargo add` comme
dépendances directes ; aucune version transitive mise à jour. Lecteur dédié unique
hors du pool Tokio, requête suivante remplacée par la plus récente, résultats
identifiés et refusés après fermeture/remplacement. Texte retenu 256 Kio / 4096
lignes ; un cache de repli distinct garde au plus autant de texte et de lignes.

DOCX/ODT, PPTX jusqu'à 32 parties de slides et XLSX jusqu'à 16 parties de feuilles :
8 Mio source, 1024 entrées ZIP, pas de ZIP64/multidisque, 512 Kio XML cumulé. DTDs
et entités externes refusées. Aucun macro, formule ou lien externe exécuté.
Cellules XLSX repérées par leur adresse ; ordre des parties, valeurs mises en cache,
pas de reproduction de la mise en page ni recalcul des formats de date.

PDF : copie privée bornée, convertisseur `pdftotext` local avec arguments séparés
et environnement vidé, jusqu'à 20 pages, deadline 4 s. Unix : CPU 3 s et sortie
512 Kio ; Linux : espace d'adressage 512 Mio. macOS refuse ce dernier plafond,
donc ne pas annoncer de borne RAM dure du convertisseur sur macOS. À la fermeture,
le lecteur signale l'annulation et attend au plus 100 ms ; le convertisseur observe
le signal et tue/récolte son groupe. Ce mécanisme n'est pas une sandbox OS.

Images PNG/JPEG/WebP : 5 Mio source, 4 M pixels, formats vérifiés par signature,
limite d'allocation du décodeur 32 Mio (non stricte selon le codec), vignette
160×160 max. Aperçu portable en demi-cellules colorées ; protocoles Kitty/Sixel/iTerm,
PDF graphique/OCR, formats Office historiques et pièces jointes Office/PDF au
modèle restent des lots distincts. Échecs et formats non pris en charge affichés.

Suggestions : une seule tâche, contexte actuel de la vue jusqu'à quatre blocs
utilisateur/agent et 8 Kio, sortie jusqu'à 240 caractères et 256 chunks, délai 10 s,
plafond fournisseur demandé 256 tokens. Le client fournisseur peut retenter une
requête en erreur dans ce délai ; ne pas promettre un seul appel HTTP facturé.
L'usage annexe reste à raccorder au ledger. Acceptation explicite par Tab, aucun
nouveau contexte implicite du moteur principal ou événement de replay.
