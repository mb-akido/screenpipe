# screenpipe SQLite compatibility patch

This directory keeps the Rust API and Cargo version of `libsqlite3-sys`
0.26.0 so SQLx 0.7 and rusqlite 0.29 continue to resolve one compatible
native library. Its normal `bundled` build compiles SQLite 3.51.3, the first
SQLite patch release containing the WAL-reset corruption fix.

Upstream advisory: <https://www.sqlite.org/wal.html#walresetbug>

- Rust wrapper/build files: `libsqlite3-sys` 0.26.0 from crates.io
- Wrapper crate SHA-256 (Cargo checksum): `afc22eff61b133b115c6e8c74e818c628d6d5e7a502afea6f64dee076dd94326`
- SQLite source: <https://www.sqlite.org/2026/sqlite-amalgamation-3510300.zip>
- Archive SHA3-256: `ced02ff9738970f338c9c8e269897b554bcda73f6cf1029d49459e1324dbeaea`
- `sqlite3.c` SHA3-256: `32d5424f97e0a7fc5ed2f6335afbb58be4e0298bd7117a34e39d345ff13d859e`
- SQLite source ID: `2026-03-13 10:38:09 737ae4a34738ffa0c3ff7f9bb18df914dd1cad163f28fd6b6e114a344fe6d618`

Screenpipe's production dependency enables the ordinary `bundled` SQLite
feature, not SQLCipher. The old `libsqlite3-sys` 0.26 SQLCipher amalgamation is
intentionally omitted, and the build script rejects SQLCipher features. It is
based on SQLite 3.39.4 and must receive a separate WAL-reset-safe upgrade before
future use.

Do not remove the path patches in either workspace manifest: the desktop app
has a standalone Cargo workspace and lockfile in addition to the repository
workspace.
