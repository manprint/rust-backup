# rust-backup — Usage

The complete operator guide lives in **[`docs/usage/`](docs/usage/README.md)**
and is written in Italian. It is the single source of truth for running this
program: every subcommand, every flag, every operating mode, each page opening
with its minimal working example.

| Page | Contents |
|------|----------|
| [Index and concepts](docs/usage/README.md) | execution model, configuration precedence, credentials, verified completion |
| [Coordination server](docs/usage/01-server.md) | `rust-backup server`, TLS, `--max-conns`, systemd, containers |
| [Transport](docs/usage/02-trasporto.md) | flags shared by every module: `--to`, `--channel`, secrets, carriers, UDP/QUIC, rate limit |
| [PostgreSQL](docs/usage/03-postgres.md) · [MongoDB](docs/usage/04-mongodb.md) · [Filesystem](docs/usage/05-filesystem.md) · [S3](docs/usage/06-s3.md) | one page per module |
| [YAML sessions](docs/usage/07-sessioni-yaml.md) | `rust-backup run --config`, full file schema |
| [`plan` dry-run](docs/usage/08-plan.md) | analyze a source without transferring anything |
| [Docker and Compose](docs/usage/09-docker.md) | the image as server and as client |
| [Environment variables](docs/usage/10-variabili-ambiente.md) | complete flag ⇄ variable map |
| [Exit codes](docs/usage/11-codici-uscita.md) | exit-code table and troubleshooting |

`scripts/help_parity.sh` (part of `scripts/gates.sh`) checks in both directions
that `docs/usage/` and the binary's `--help` describe exactly the same switches.
