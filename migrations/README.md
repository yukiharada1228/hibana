# Platform migrations

`hibana-migration` manages the PostgreSQL schema using SeaORM Migration.

- `src/m20260915_000001_platform.rs`: frozen table and CHECK definitions.
- `src/m20260915_000001_indexes.rs`: frozen indexes and foreign keys.
- `src/m20260915_000001_security.sql`: PostgreSQL roles, RLS, grants and security functions.
- `src/lib.rs`: migration ordering, empty-database guard, serialized transactional runner.

Apply via `MIGRATION_DATABASE_URL=... cargo run -p hibana-control-plane -- --migrate-only`.
Use an empty verification database. Existing SQLx databases are deliberately rejected
without changing their data or schema; this baseline is not an in-place upgrade.

For each later schema change, add a new numbered Rust migration and register it in
`Migrator::migrations()`. Update the corresponding Entity models in `crates/database`.
Never edit applied files or generate historical migrations from current Entity models.
Migration history records versions, not source checksums. The initial baseline has
no destructive `down`; disposable databases must be recreated explicitly.

Run `bash scripts/test-http.sh` for fresh installation, repeat/concurrent migration,
legacy-database rejection, ORM operations, RLS, and HTTP execution tests.
See [the platform database guide](../docs/database.md) for architecture and cutover.
