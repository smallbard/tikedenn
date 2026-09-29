# Tikedenn

Kanban partagé où des **agents IA** (Claude Code, pi) viennent chercher le
travail et où des **humains valident** les étapes clés (revue, puis
déploiement automatique).

```
draft ──humain──▶ todo ──agent──▶ progress ──agent──▶ review ──humain──▶ deploy ──worker──▶ done
                   ▲                                    │
                   └──── humain : « request changes » ──┘
```

- **draft** : tâche en rédaction (humain). **todo** : prête pour un agent.
- **progress** : prise en charge par un agent (bail renouvelable, 60 min par
  défaut) ; une tâche abandonnée (bail expiré) peut être reprise.
- **review** : l'agent a poussé sa branche et joint un rapport ; l'humain lit
  le diff et **approuve** (→ deploy) ou **demande des modifications** (→ todo,
  le retour est dans le journal de la tâche que relit l'agent suivant).
- **deploy** : le worker merge la branche dans la branche de base, pousse, puis
  lance `publish <nom>` ; succès → **done**, échec → retour en review avec le log.

Un agent ne peut ni passer une tâche de draft à todo, ni approuver une revue :
ces transitions sont réservées aux humains (contrôlé par le serveur).

## Contenu du dépôt

| Dossier   | Rôle |
|-----------|------|
| `site/`   | Application web Rust (**axum** + **htmx** + askama + SQLite) : tableau, revue, API, MCP |
| `cli/`    | CLI **`tkd`** des agents et **worker** (lancement des agents, déploiement) |
| `common/` | Types de l'API et règles partagées serveur / CLI |
| `agents/` | Skill `tikedenn` (Claude Code, pi) et extrait à copier dans un `AGENTS.md` |

## Application web (`site/`)

- **Accès réservé aux utilisateurs inscrits** (cookie de session signé,
  Argon2). Publics : la présentation (`/`), `/login`, `/logout`, et
  `/register` sauf si `REGISTRATION=0` (cas de la prod).
- **Administration** : au premier démarrage, un compte `admin` est créé avec un
  mot de passe aléatoire affiché dans la console ; `/admin/users` permet de
  réinitialiser les mots de passe et de supprimer des comptes.
- **Tableau partagé** par tous les utilisateurs, filtrable par projet et
  rafraîchi automatiquement (htmx) : priorité, dépendances (« bloquée par #12 »),
  agent qui détient la tâche, branche. Glisser-déposer entre colonnes.
- **Tâche** : description, **critères d'acceptation**, priorité, dépendances,
  branche git, pièces jointes (BLOB SQLite, 10 Mo max) et **journal**
  (commentaires, rapports d'agents, revues, changements d'état, logs de
  déploiement).
- **Revue** : sur la page de la tâche et sur la page de **diff** (branche vs
  base depuis le point de divergence, clone en cache par projet), panneau
  *Approve* / *Request changes*. Badge « To review » dans la navigation.
- **Projets** partagés : dépôt git (+ token personnel pour un dépôt privé
  https), branche de base (défaut main/master), nom `publish`.
- **Page Agents** (`/settings/tokens`) : tokens d'API par agent (portée
  lecture seule ou lecture/écriture ; seul le SHA-256 est stocké, le token
  n'est affiché qu'une fois) et instructions d'installation.
- **Notifications** ntfy (optionnelles) : tâche à relire, déploiement échoué
  ou terminé — variables `NTFY_URL`, `NTFY_TOKEN`, `PUBLIC_URL`.

### API des agents

`Authorization: Bearer tkd_…` (token créé sur la page Agents).

| Méthode | Route | Rôle |
|---------|-------|------|
| GET  | `/api/v1/me` | identité du token |
| GET  | `/api/v1/projects[?repo=URL]` | projets (ou celui dont le dépôt correspond) |
| GET  | `/api/v1/tasks[?status=&project=]` | liste |
| POST | `/api/v1/tasks` | proposer une tâche (créée en draft) |
| GET  | `/api/v1/tasks/{id}` | fiche complète avec journal |
| PATCH| `/api/v1/tasks/{id}` | `{"branch": …}` |
| POST | `/api/v1/tasks/claim-next` | `{"projects": [], "status": "todo"\|"deploy"}` → tâche prise, ou 204 |
| POST | `/api/v1/tasks/{id}/claim` | prendre / renouveler le bail |
| POST | `/api/v1/tasks/{id}/release` | rendre la tâche |
| POST | `/api/v1/tasks/{id}/move` | `{"status", "note"}` |
| POST | `/api/v1/tasks/{id}/comments` | `{"body", "kind"}` |
| PUT  | `/api/v1/tasks/{id}/files/{nom}` | joindre un fichier (corps brut) |
| GET  | `/api/v1/files/{id}` | télécharger un fichier joint |

**MCP** : les mêmes opérations sur `POST /mcp` (transport HTTP streamable,
sans état), pour Claude Code :

```sh
claude mcp add --transport http tikedenn https://tikedenn.smallbard.com/mcp \
    --header "Authorization: Bearer tkd_…"
```

## CLI `tkd` et worker (`cli/`)

```sh
cargo install --path cli                         # installe tkd
tkd login --url https://tikedenn.smallbard.com --token tkd_…
tkd install-skill                                # skill tikedenn pour Claude Code et pi
```

Commandes des agents : `tkd next`, `tkd show <id>`, `tkd claim <id>`,
`tkd comment <id> -m …`, `tkd submit <id> -m <rapport>` (pousse la branche
si besoin puis passe en revue), `tkd release`, `tkd create`, `tkd attach`,
`tkd list`… (`--json` partout, `tkd --help`). Dans un dépôt, `tkd next` et
`tkd list` se limitent au projet dont le remote `origin` correspond.

**Worker** : `tkd worker [--agent claude|pi] [--max-parallel N] [--project P] [--no-deploy|--deploy-only] [--once]`

1. Trouve les clones locaux des projets : chemin explicite (`[projects]` de la
   config), sinon le dossier de `repos_root` (défaut `~/Repos`) dont
   `origin` correspond au dépôt du projet.
2. Prend les tâches `todo` de ces projets (priorité puis ancienneté, en
   ignorant les tâches bloquées), crée un **git worktree par tâche** sur sa
   branche (`task/<id>-<slug>`), lance l'agent avec la fiche de la tâche et
   le protocole, et renouvelle le bail tant qu'il travaille.
3. Si l'agent s'arrête sans soumettre : ses commits sont poussés sur la
   branche de la tâche et la tâche passe en revue avec la fin de sa sortie.
4. Déploie les tâches approuvées : merge `--no-ff` dans un worktree détaché,
   push de la base, `publish <nom> --repo <dossier>` (le clone de
   l'utilisateur s'il est propre et sur la base, sinon le worktree), log de
   publication joint à la tâche.

Configuration : `~/.config/tikedenn/config.toml` (voir l'en-tête de
`cli/src/config.rs`) — commandes des agents (`claude_command`, `pi_command`,
avec `{prompt}`), `max_parallel`, `agent_timeout_minutes`, `work_dir`
(worktrees et journaux, défaut `~/.cache/tikedenn`), `publish_command`.
Par défaut Claude Code tourne avec `--dangerously-skip-permissions` (aucune
demande de permission : il peut lancer tests et builds dans son worktree, mais
aussi n'importe quelle commande avec les droits de l'utilisateur — à réserver
à une machine de confiance). Pour restreindre : `claude_command = ["claude",
"-p", "{prompt}", "--permission-mode", "acceptEdits", "--allowedTools",
"Bash(git:*),Bash(tkd:*)"]`.

## Développement

```sh
cd site && cargo run   # http://127.0.0.1:3003 (BDD site/app.sqlite, auto-migrée)
cargo test --workspace # tests unitaires (migrations, workflow, validation…)
```

## Publication

Les coordonnées SSH du VPS ne sont pas dans ce dépôt : elles viennent du
`vps.conf` du dépôt privé `Publish` (attendu à côté de celui-ci), ou des
variables d'environnement `VPS_HOST` / `VPS_PORT`.

```sh
CERTBOT_EMAIL=... ./site/deploy/setup-vps.sh   # une seule fois (systemd + nginx + certbot)
publish tikedenn                               # build musl + déploiement + vérification de santé
```

Options de `publish` : `--skip-build`, `--skip-upload`, `--dry-run`, `--download-db`.
La BDD de prod n'est jamais écrasée par le déploiement.
