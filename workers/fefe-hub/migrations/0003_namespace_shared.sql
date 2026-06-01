PRAGMA foreign_keys = off;

CREATE TABLE IF NOT EXISTS users_without_namespace_unique (
  user_id TEXT PRIMARY KEY,
  username TEXT NOT NULL,
  namespace TEXT NOT NULL,
  password_hash TEXT NOT NULL,
  password_salt TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(username, namespace)
);

INSERT INTO users_without_namespace_unique
  (user_id, username, namespace, password_hash, password_salt, created_at)
SELECT user_id, username, namespace, password_hash, password_salt, created_at
FROM users;

DROP TABLE users;

ALTER TABLE users_without_namespace_unique RENAME TO users;

PRAGMA foreign_keys = on;
