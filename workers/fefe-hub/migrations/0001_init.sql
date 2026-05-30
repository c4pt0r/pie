CREATE TABLE IF NOT EXISTS users (
  user_id TEXT PRIMARY KEY,
  username TEXT NOT NULL UNIQUE,
  namespace TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  password_salt TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS human_sessions (
  session_id TEXT PRIMARY KEY,
  session_hash TEXT NOT NULL UNIQUE,
  user_id TEXT NOT NULL,
  namespace TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  revoked_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(user_id)
);

CREATE TABLE IF NOT EXISTS agents (
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

CREATE TABLE IF NOT EXISTS agent_tokens (
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

CREATE TABLE IF NOT EXISTS trust_grants (
  receiver_agent_id TEXT NOT NULL,
  sender_agent_id TEXT NOT NULL,
  action_class TEXT NOT NULL,
  granted_at TEXT NOT NULL,
  expires_at TEXT,
  PRIMARY KEY(receiver_agent_id, sender_agent_id, action_class)
);

CREATE TABLE IF NOT EXISTS block_list (
  receiver_agent_id TEXT NOT NULL,
  sender_agent_id TEXT NOT NULL,
  blocked_at TEXT NOT NULL,
  PRIMARY KEY(receiver_agent_id, sender_agent_id)
);

CREATE TABLE IF NOT EXISTS notifications (
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

CREATE INDEX IF NOT EXISTS idx_agents_discoverable ON agents(discoverable, namespace, deleted_at);
CREATE INDEX IF NOT EXISTS idx_notifications_receiver_status ON notifications(receiver_agent_id, status, created_at);
