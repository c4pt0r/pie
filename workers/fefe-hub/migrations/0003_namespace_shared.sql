DROP TABLE IF EXISTS users_without_namespace_unique;
DROP TABLE IF EXISTS users_new;
DROP TABLE IF EXISTS human_sessions_data;
DROP TABLE IF EXISTS agents_data;
DROP TABLE IF EXISTS agent_tokens_data;
DROP TABLE IF EXISTS notifications_data;
DROP TABLE IF EXISTS auth_exchanges_data;

CREATE TABLE human_sessions_data AS SELECT * FROM human_sessions;
CREATE TABLE agents_data AS SELECT * FROM agents;
CREATE TABLE agent_tokens_data AS SELECT * FROM agent_tokens;
CREATE TABLE notifications_data AS SELECT * FROM notifications;
CREATE TABLE auth_exchanges_data AS SELECT * FROM auth_exchanges;

CREATE TABLE users_new (
  user_id TEXT PRIMARY KEY,
  username TEXT NOT NULL,
  namespace TEXT NOT NULL,
  password_hash TEXT NOT NULL,
  password_salt TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(username, namespace)
);

INSERT INTO users_new
  (user_id, username, namespace, password_hash, password_salt, created_at)
SELECT user_id, username, namespace, password_hash, password_salt, created_at
FROM users;

DROP TABLE auth_exchanges;
DROP TABLE agent_tokens;
DROP TABLE notifications;
DROP TABLE agents;
DROP TABLE human_sessions;
DROP TABLE users;

ALTER TABLE users_new RENAME TO users;

CREATE TABLE human_sessions (
  session_id TEXT PRIMARY KEY,
  session_hash TEXT NOT NULL UNIQUE,
  user_id TEXT NOT NULL,
  namespace TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  revoked_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(user_id)
);

CREATE TABLE agents (
  agent_id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  namespace TEXT NOT NULL,
  handle TEXT NOT NULL,
  display_name TEXT NOT NULL,
  description TEXT NOT NULL,
  capabilities_json TEXT NOT NULL,
  discoverable TEXT NOT NULL,
  inbox TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_seen_at TEXT,
  deleted_at TEXT,
  UNIQUE(namespace, handle),
  FOREIGN KEY (user_id) REFERENCES users(user_id)
);

CREATE TABLE agent_tokens (
  token_id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE,
  agent_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  namespace TEXT NOT NULL,
  permissions_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_used_at TEXT,
  expires_at TEXT,
  revoked_at TEXT,
  FOREIGN KEY (agent_id) REFERENCES agents(agent_id),
  FOREIGN KEY (user_id) REFERENCES users(user_id)
);

CREATE TABLE notifications (
  notification_id TEXT PRIMARY KEY,
  receiver_agent_id TEXT NOT NULL,
  sender_agent_id TEXT NOT NULL,
  sender_handle TEXT NOT NULL,
  sender_namespace TEXT NOT NULL,
  summary TEXT NOT NULL,
  payload_json TEXT,
  payload_visibility TEXT NOT NULL,
  status TEXT NOT NULL,
  first_contact_required INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  delivered_at TEXT,
  acked_at TEXT,
  FOREIGN KEY (receiver_agent_id) REFERENCES agents(agent_id),
  FOREIGN KEY (sender_agent_id) REFERENCES agents(agent_id)
);

CREATE TABLE auth_exchanges (
  exchange_request_id TEXT PRIMARY KEY,
  client_kind TEXT NOT NULL,
  client_version TEXT NOT NULL,
  loopback_redirect_uri TEXT NOT NULL,
  code_challenge TEXT NOT NULL,
  state_hash TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  code_hash TEXT,
  code_issued_at TEXT,
  user_id TEXT,
  used_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(user_id)
);

INSERT INTO human_sessions
  (session_id, session_hash, user_id, namespace, created_at, expires_at, revoked_at)
SELECT session_id, session_hash, user_id, namespace, created_at, expires_at, revoked_at
FROM human_sessions_data;

INSERT INTO agents
  (agent_id, user_id, namespace, handle, display_name, description, capabilities_json,
   discoverable, inbox, created_at, last_seen_at, deleted_at)
SELECT agent_id, user_id, namespace, handle, display_name, description, capabilities_json,
       discoverable, inbox, created_at, last_seen_at, deleted_at
FROM agents_data;

INSERT INTO agent_tokens
  (token_id, token_hash, agent_id, user_id, namespace, permissions_json,
   created_at, last_used_at, expires_at, revoked_at)
SELECT token_id, token_hash, agent_id, user_id, namespace, permissions_json,
       created_at, last_used_at, expires_at, revoked_at
FROM agent_tokens_data;

INSERT INTO notifications
  (notification_id, receiver_agent_id, sender_agent_id, sender_handle, sender_namespace,
   summary, payload_json, payload_visibility, status, first_contact_required,
   created_at, delivered_at, acked_at)
SELECT notification_id, receiver_agent_id, sender_agent_id, sender_handle, sender_namespace,
       summary, payload_json, payload_visibility, status, first_contact_required,
       created_at, delivered_at, acked_at
FROM notifications_data;

INSERT INTO auth_exchanges
  (exchange_request_id, client_kind, client_version, loopback_redirect_uri, code_challenge,
   state_hash, created_at, expires_at, code_hash, code_issued_at, user_id, used_at)
SELECT exchange_request_id, client_kind, client_version, loopback_redirect_uri, code_challenge,
       state_hash, created_at, expires_at, code_hash, code_issued_at, user_id, used_at
FROM auth_exchanges_data;

DROP TABLE human_sessions_data;
DROP TABLE agents_data;
DROP TABLE agent_tokens_data;
DROP TABLE notifications_data;
DROP TABLE auth_exchanges_data;

CREATE INDEX IF NOT EXISTS idx_agents_discoverable ON agents(discoverable, namespace, deleted_at);
CREATE INDEX IF NOT EXISTS idx_notifications_receiver_status ON notifications(receiver_agent_id, status, created_at);
CREATE INDEX IF NOT EXISTS idx_auth_exchanges_expires ON auth_exchanges(expires_at, used_at);
