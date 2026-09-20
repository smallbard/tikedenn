# Tikedenn

Application web Rust (**axum** + **htmx** + askama + SQLite), calquée
architecturellement sur `../Lotr/site`.

- **Accès réservé aux utilisateurs inscrits** : tout est protégé par une
  authentification (connexion, cookie de session signé, hachage Argon2).
  Seul `/login` (et `/logout`) est public ; la route d'inscription est
  désactivée (commentée dans `main.rs`) — la BDD de prod se gère en direct
  si besoin.
- **Administration** : au tout premier démarrage, un compte `admin` est créé
  avec un mot de passe aléatoire affiché dans la console (journal systemd en
  production). `/admin/users` (réservé aux admins) permet de réinitialiser les
  mots de passe et de supprimer des utilisateurs.
- **Fonctionnalité principale : un tableau Kanban par utilisateur**
  (états `draft → todo → progress → done`), avec des **projets**
  (associés aux tâches, champ repo git optionnel), des descriptions
  longues et des **pièces jointes** (stockées en BLOB dans SQLite, 10 Mo
  max). Le tableau est filtrable par projet. Les cartes sont minimales
  (titre, projet, début de description) : la création se fait depuis une
  page dédiée (bouton « New task ») et la modification depuis la page
  d'édition de la tâche (`/tasks/{id}/edit`) — titre, projet, description,
  état, fichiers joints, suppression.
- **htmx** n'est utilisé que pour la suppression d'un fichier joint sur la
  page d'édition ; le reste de l'app est en HTML classique avec
  redirections (form POST + 303).
- Déploiement sur le VPS (même machine que LotR) : voir `site/publish.sh`
  et `site/deploy/` (service systemd + vhost nginx).

## Développement

```sh
cd site && cargo run   # http://127.0.0.1:3002 (BDD site/app.sqlite, auto-migrée)
```

## Publication

```sh
./site/deploy/setup-vps.sh   # une seule fois (systemd + nginx + certbot)
./site/publish.sh            # build musl + déploiement + vérification de santé
```

Options : `--skip-build`, `--skip-upload`, `--download-db`.
La BDD de prod n'est jamais écrasée par le déploiement.
