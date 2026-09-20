# Tikedenn

Application web Rust (**axum** + **htmx** + askama + SQLite), calquée
architecturellement sur `../Lotr/site`.

- **Accès réservé aux utilisateurs inscrits** : tout est protégé par une
  authentification (inscription / connexion, cookie de session signé,
  hachage Argon2). Seuls `/login`, `/register`, `/logout` et `/static` sont publics.
- **htmx** : le tableau de bord (notes) est mis à jour par fragments HTML
  rendus côté serveur, sans rechargement de page.
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
