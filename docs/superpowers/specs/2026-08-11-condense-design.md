# Spec — `condense` : compression native des tool-results de l'historique

Statut : approuvée (design validé en session 2026-08-11).
Objectif produit : l'équivalent rtk intégré au Core kaji — réduire les tokens consommés par
les sorties d'outils passées, sans hook externe, sans appel LLM, sans risque comportemental.

## Décisions structurantes (validées une à une)

1. **Historique seul.** Le consommateur d'un tool-result kaji est le LLM. Le tour courant
   voit toujours le résultat **brut** ; seuls les tool-results des tours passés sont
   compressés. Aucune réécriture façon rtk-CLI avant première lecture par le modèle.
2. **Règle générique v1.** Un seul pipeline universel (strip ANSI, dédup de lignes
   répétées, budget de lignes head/tail). Les règles par-outil (ls/git/grep → tableaux)
   sont un non-objectif v1 : elles n'arrivent en v2 que si la mesure par outil
   (`CondenseStats`) montre où elles paient. Pas de résumé LLM (contraire à la
   philosophie 0-token/0-latence de la mémoire kaji).
3. **Fenêtre de fraîcheur : 2 tours.** Le tour courant + le précédent restent bruts
   (pattern « lis le fichier → édite-le au tour suivant »). Défaut 2, configurable.
4. **À la volée, jamais destructif.** La DB de session reste intégralement brute. La
   compression ne s'applique qu'à la **vue** de la conversation construite pour l'appel
   provider. `--resume`, l'export, le debug et `/cost` voient toujours le brut.

## Composant

`crates/kaji/src/context_mgmt/condense.rs` — fonctions pures, zéro I/O.

```rust
pub struct CondenseBudget { pub max_lines: usize }          // défaut 40
pub struct CondenseStats { /* par outil: lignes/octets avant→après, nb résultats touchés */ }

pub fn condense_history(
    messages: &[Message],
    keep_raw_turns: usize,          // défaut 2
    budget: &CondenseBudget,
) -> (Vec<Message>, CondenseStats)
```

Algorithme :

- Marche arrière depuis la fin de `messages` en comptant les messages **user-texte**
  (les ToolResponse portés par des messages role=user ne comptent pas comme tours).
- Tant que moins de `keep_raw_turns` messages user-texte ont été franchis : zone fraîche,
  aucun changement. Au-delà : chaque contenu ToolResponse est réécrit :
  1. strip des séquences ANSI ;
  2. lignes consécutives identiques → une occurrence + suffixe `×N` ;
  3. si > `max_lines` lignes : conserver `head = 60 % de max_lines` premières lignes et
     `tail = 25 % de max_lines` dernières (arrondis vers le bas, minimum 1 chacun), le
     marqueur `[… N lignes omises — résultat complet conservé en session]` entre les deux ;
     total émis ≤ max_lines ;
  4. whitespace de fin de ligne retiré.
- Les contenus non-texte (images…) et tout ce qui n'est pas ToolResponse sont intacts.
- Invariant structurel : les tool-results du tour courant sont toujours dans la zone
  fraîche (0 message user-texte franchi), y compris entre deux appels d'inférence du
  même tour agentique. Aucun cas spécial nécessaire.
- Idempotence : recompresser une sortie déjà compressée ne la change plus (le marqueur
  n'est jamais re-tronqué, les stats ne double-comptent pas).

## Injection — parité legacy / state-machine

Même patron que le splice mémoire (`crate::kaji::splice_memory_block`, parité assurée
`f5e181336`) :

- chemin legacy : dans `prepare_reply_context` (voisinage `agents/agent.rs:801`), sur la
  liste de messages finalisée envoyée au provider ;
- chemin state-machine : dans `InferenceRunner` (voisinage
  `agents/state_machine/ops_llm.rs:404`), au même point logique.

La transformation s'applique à la **copie sortante**, jamais à la conversation de session.
Contrainte AGENTS.md respectée : comportement implémenté et testé dans les deux chemins.

## Configuration

| Env var | Défaut | Rôle |
|---|---|---|
| `KAJI_CONDENSE` | `1` (actif) | kill-switch global (`0` = off) |
| `KAJI_CONDENSE_KEEP_TURNS` | `2` | fenêtre de tours bruts |
| `KAJI_CONDENSE_MAX_LINES` | `40` | budget lignes par tool-result |

Résolution des valeurs au même endroit et sur le même modèle que les options existantes
(zéro/invalide → défaut). Lues une fois par tour, pas dans la boucle chaude.

## Observabilité

- `CondenseStats` émis en `tracing::debug!` à chaque construction de prompt
  (lignes/octets avant→après, par nom d'outil).
- Agrégat de session exposé dans `/cost` (TUI) : ligne « condensé ~N tok (est.) » —
  estimation octets/4, affichée comme estimation ; pas de conversion $ (le gain est
  contrefactuel, pas de fausse précision).
- `kaji gain` complet (historique par commande, à la rtk) : v2, hors scope.

## Tests

- **Unitaires purs** (condense.rs) : fenêtre (0/1/2 tours user-texte, tool-results du
  tour courant intacts), budget head/tail + marqueur exact, dédup `×N`, strip ANSI,
  contenus non-texte intacts, idempotence, stats correctes, budget 0/1 sans panique.
- **Parité** : un test d'intégration par chemin (legacy + state-machine) vérifiant que le
  prompt sortant contient la version condensée d'un vieux tool-result et la version brute
  d'un récent — sur le modèle des tests du splice mémoire.
- **Baseline** : `kaji --lib` 8 échecs préexistants inchangés ; clippy workspace reste vert.

## Non-objectifs v1

Règles par-outil ; réécriture in-place / maigrissement du storage ; commande `/expand`
(inutile : la DB est brute) ; résumé sémantique LLM ; `kaji gain` complet ; compression
du tour courant sous quelque forme que ce soit.

## Risques identifiés

- Le modèle re-demande un vieux résultat complet → il relance l'outil (coût d'un appel
  outil, comportement standard) ; le marqueur l'indique explicitement.
- Sur-compression d'un outil au format dense (ex : JSON une-ligne) : la dédup et le
  head/tail par lignes dégradent peu le JSON compact (1 ligne = sous le budget) — cas
  couvert par test.
- Interaction avec la compaction existante (seuil 0.6) : condense réduit la pression de
  compaction (moins de tokens → compaction plus tardive) ; aucun couplage de code, les
  deux mécanismes restent indépendants.
