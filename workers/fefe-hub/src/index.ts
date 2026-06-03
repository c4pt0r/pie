const PROTOCOL_VERSION = "2025-03-26";
const DEFAULT_PERMISSIONS = [
  "agent:read_self",
  "agent:update_self_profile",
  "agent:list_namespace",
  "agent:discover_public",
  "agent:delete_self",
  "notification:send",
  "notification:receive",
  "token:rotate_self",
  "trust:list",
  "trust:revoke",
  "trust:block",
  "trust:unblock",
] as const;

const JSON_LIMIT_BYTES = 16 * 1024;
const PAYLOAD_LIMIT_BYTES = 8 * 1024;
const SUMMARY_LIMIT_CHARS = 240;
const LIST_DEFAULT_LIMIT = 50;
const LIST_MAX_LIMIT = 100;
const AUTH_EXCHANGE_TTL_SECONDS = 5 * 60;
const MANUAL_AUTH_CODE_CHARS = 8;
const WEB_SESSION_COOKIE = "hub_session";
const WEB_SESSION_MAX_AGE_SECONDS = 30 * 24 * 60 * 60;

type Discoverable = "public" | "namespace" | "none";
type Inbox = "open" | "namespace" | "invited" | "closed";
type PayloadVisibility = "Local" | "Shared" | "Redacted";
type NotificationStatus = "pending" | "delivered" | "acked" | "dropped";

interface Env {
  DB?: D1Database;
  MAILBOX?: DurableObjectNamespace;
  HUB_VERSION?: string;
}

interface DurableObjectState {
  blockConcurrencyWhile<T>(callback: () => Promise<T>): Promise<T>;
}

interface DurableObjectNamespace {
  idFromName(name: string): DurableObjectId;
  get(id: DurableObjectId): DurableObjectStub;
}

interface DurableObjectId {}

interface DurableObjectStub {
  fetch(input: string | Request, init?: RequestInit): Promise<Response>;
}

interface D1Database {
  prepare(query: string): D1PreparedStatement;
}

interface D1PreparedStatement {
  bind(...values: unknown[]): D1PreparedStatement;
  first<T = Record<string, unknown>>(): Promise<T | null>;
  all<T = Record<string, unknown>>(): Promise<{ results?: T[] }>;
  run(): Promise<unknown>;
}

interface UserRecord {
  user_id: string;
  username: string;
  namespace: string;
  password_hash: string;
  password_salt: string;
  created_at: string;
}

interface HumanSessionRecord {
  session_id: string;
  session_hash: string;
  user_id: string;
  namespace: string;
  created_at: string;
  expires_at: string;
  revoked_at: string | null;
}

interface AgentRecord {
  agent_id: string;
  user_id: string;
  namespace: string;
  handle: string;
  display_name: string;
  description: string;
  capabilities_json: string;
  discoverable: Discoverable;
  inbox: Inbox;
  created_at: string;
  last_seen_at: string | null;
  deleted_at: string | null;
}

interface AgentTokenRecord {
  token_id: string;
  token_hash: string;
  agent_id: string;
  user_id: string;
  namespace: string;
  permissions_json: string;
  created_at: string;
  last_used_at: string | null;
  expires_at: string | null;
  revoked_at: string | null;
}

interface AuthExchangeRecord {
  exchange_request_id: string;
  client_kind: string;
  client_version: string;
  loopback_redirect_uri: string;
  code_challenge: string;
  state_hash: string;
  created_at: string;
  expires_at: string;
  code_hash: string | null;
  code_issued_at: string | null;
  user_id: string | null;
  used_at: string | null;
}

interface TrustGrantRecord {
  receiver_agent_id: string;
  sender_agent_id: string;
  action_class: "notification";
  granted_at: string;
  expires_at: string | null;
}

interface BlockRecord {
  receiver_agent_id: string;
  sender_agent_id: string;
  blocked_at: string;
}

interface NotificationRecord {
  notification_id: string;
  receiver_agent_id: string;
  sender_agent_id: string;
  sender_handle: string;
  sender_namespace: string;
  summary: string;
  payload_json: string | null;
  payload_visibility: PayloadVisibility;
  status: NotificationStatus;
  first_contact_required: number;
  created_at: string;
  delivered_at: string | null;
  acked_at: string | null;
}

type Principal =
  | {
      kind: "human";
      user_id: string;
      namespace: string;
      session_id: string;
    }
  | {
      kind: "agent";
      user_id: string;
      namespace: string;
      agent_id: string;
      token_id: string;
      permissions: string[];
    };

interface Store {
  createUser(user: UserRecord): Promise<void>;
  getUser(userId: string): Promise<UserRecord | null>;
  getUserByUsername(username: string): Promise<UserRecord | null>;
  getUsersByUsername(username: string): Promise<UserRecord[]>;
  getUserByIdentity(username: string, namespace: string): Promise<UserRecord | null>;
  getUserByNamespace(namespace: string): Promise<UserRecord | null>;
  createHumanSession(session: HumanSessionRecord): Promise<void>;
  getHumanSessionByHash(sessionHash: string): Promise<HumanSessionRecord | null>;
  createAgent(agent: AgentRecord): Promise<void>;
  updateAgent(agent: AgentRecord): Promise<void>;
  getAgent(agentId: string): Promise<AgentRecord | null>;
  getAgentByHandle(namespace: string, handle: string): Promise<AgentRecord | null>;
  listAgentsByNamespace(namespace: string, limit: number, cursor: string | null): Promise<AgentRecord[]>;
  listPublicAgents(limit: number, cursor: string | null): Promise<AgentRecord[]>;
  createAgentToken(token: AgentTokenRecord): Promise<void>;
  getAgentTokenByHash(tokenHash: string): Promise<AgentTokenRecord | null>;
  revokeAgentToken(tokenId: string, revokedAt: string): Promise<void>;
  createAuthExchange(record: AuthExchangeRecord): Promise<void>;
  getAuthExchange(exchangeRequestId: string): Promise<AuthExchangeRecord | null>;
  issueAuthExchangeCode(exchangeRequestId: string, codeHash: string, userId: string, codeIssuedAt: string): Promise<void>;
  consumeAuthExchangeCode(exchangeRequestId: string, codeHash: string, userId: string, usedAt: string): Promise<boolean>;
  touchAgent(agentId: string, at: string): Promise<void>;
  listTrust(receiverAgentId: string): Promise<TrustGrantRecord[]>;
  getTrust(receiverAgentId: string, senderAgentId: string): Promise<TrustGrantRecord | null>;
  revokeTrust(receiverAgentId: string, senderAgentId: string, actionClass: string): Promise<void>;
  blockSender(record: BlockRecord): Promise<void>;
  unblockSender(receiverAgentId: string, senderAgentId: string): Promise<void>;
  getBlock(receiverAgentId: string, senderAgentId: string): Promise<BlockRecord | null>;
  createNotification(notification: NotificationRecord): Promise<void>;
  markNotificationDelivered(notificationId: string, deliveredAt: string): Promise<void>;
  listNotifications(receiverAgentId: string, limit: number, cursor: string | null): Promise<NotificationRecord[]>;
  ackNotifications(receiverAgentId: string, notificationIds: string[], ackedAt: string): Promise<string[]>;
}

interface Mailbox {
  connect(agentId: string): Response | Promise<Response>;
  push(agentId: string, notification: NotificationRecord): Promise<boolean>;
}

class PublicError extends Error {
  readonly code: number;
  readonly name: string;
  readonly data?: Record<string, unknown>;

  constructor(code: number, name: string, message: string, data?: Record<string, unknown>) {
    super(message);
    this.code = code;
    this.name = name;
    this.data = data;
  }
}

const ERR = {
  sessionExpired: () => new PublicError(-32000, "session_expired", "Hub session expired. Run `/hub login` to re-authenticate."),
  permissionDenied: (message = "Operation not permitted by the target's inbox policy.") =>
    new PublicError(-32001, "permission_denied", message),
  notFound: (message = "No agent with that id is reachable. Check `discover_public_agents`.") =>
    new PublicError(-32003, "not_found", message),
  bodyTooLarge: (capBytes: number) =>
    new PublicError(-32004, "body_too_large", "Notification body exceeds the hub cap.", { cap_bytes: capBytes }),
  authRevoked: () => new PublicError(-32005, "auth_revoked", "Agent token revoked. Run `/hub rotate` or `/hub register`."),
  trustRequired: () =>
    new PublicError(-32006, "trust_required", "First-contact gate: receiver must accept this sender before delivery."),
  schemaInvalid: (violations: string[]) =>
    new PublicError(-32007, "schema_invalid", "Tool arguments did not validate against the hub schema.", { violations }),
  authRequired: () =>
    new PublicError(-32009, "auth_required", "Hub call requires an `Authorization: Bearer <token>` header."),
  authInvalid: () =>
    new PublicError(-32010, "auth_invalid", "Hub credential is malformed. Re-register the agent or rotate the hub token."),
};

class D1Store implements Store {
  constructor(private readonly db: D1Database) {}

  async createUser(user: UserRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO users
         (user_id, username, namespace, password_hash, password_salt, created_at)
         VALUES (?, ?, ?, ?, ?, ?)`,
      )
      .bind(user.user_id, user.username, user.namespace, user.password_hash, user.password_salt, user.created_at)
      .run();
  }

  getUserByUsername(username: string): Promise<UserRecord | null> {
    return this.db.prepare("SELECT * FROM users WHERE username = ?").bind(username).first<UserRecord>();
  }

  async getUsersByUsername(username: string): Promise<UserRecord[]> {
    const result = await this.db.prepare("SELECT * FROM users WHERE username = ? ORDER BY namespace").bind(username).all<UserRecord>();
    return result.results ?? [];
  }

  getUserByIdentity(username: string, namespace: string): Promise<UserRecord | null> {
    return this.db
      .prepare("SELECT * FROM users WHERE username = ? AND namespace = ?")
      .bind(username, namespace)
      .first<UserRecord>();
  }

  getUser(userId: string): Promise<UserRecord | null> {
    return this.db.prepare("SELECT * FROM users WHERE user_id = ?").bind(userId).first<UserRecord>();
  }

  getUserByNamespace(namespace: string): Promise<UserRecord | null> {
    return this.db.prepare("SELECT * FROM users WHERE namespace = ?").bind(namespace).first<UserRecord>();
  }

  async createHumanSession(session: HumanSessionRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO human_sessions
         (session_id, session_hash, user_id, namespace, created_at, expires_at, revoked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        session.session_id,
        session.session_hash,
        session.user_id,
        session.namespace,
        session.created_at,
        session.expires_at,
        session.revoked_at,
      )
      .run();
  }

  getHumanSessionByHash(sessionHash: string): Promise<HumanSessionRecord | null> {
    return this.db
      .prepare("SELECT * FROM human_sessions WHERE session_hash = ?")
      .bind(sessionHash)
      .first<HumanSessionRecord>();
  }

  async createAgent(agent: AgentRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO agents
         (agent_id, user_id, namespace, handle, display_name, description, capabilities_json,
          discoverable, inbox, created_at, last_seen_at, deleted_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        agent.agent_id,
        agent.user_id,
        agent.namespace,
        agent.handle,
        agent.display_name,
        agent.description,
        agent.capabilities_json,
        agent.discoverable,
        agent.inbox,
        agent.created_at,
        agent.last_seen_at,
        agent.deleted_at,
      )
      .run();
  }

  async updateAgent(agent: AgentRecord): Promise<void> {
    await this.db
      .prepare(
        `UPDATE agents
         SET handle = ?, display_name = ?, description = ?, capabilities_json = ?,
             discoverable = ?, inbox = ?, last_seen_at = ?, deleted_at = ?
         WHERE agent_id = ?`,
      )
      .bind(
        agent.handle,
        agent.display_name,
        agent.description,
        agent.capabilities_json,
        agent.discoverable,
        agent.inbox,
        agent.last_seen_at,
        agent.deleted_at,
        agent.agent_id,
      )
      .run();
  }

  getAgent(agentId: string): Promise<AgentRecord | null> {
    return this.db.prepare("SELECT * FROM agents WHERE agent_id = ?").bind(agentId).first<AgentRecord>();
  }

  getAgentByHandle(namespace: string, handle: string): Promise<AgentRecord | null> {
    return this.db
      .prepare("SELECT * FROM agents WHERE namespace = ? AND handle = ?")
      .bind(namespace, handle)
      .first<AgentRecord>();
  }

  async listAgentsByNamespace(namespace: string, limit: number, cursor: string | null): Promise<AgentRecord[]> {
    const query = cursor
      ? `SELECT * FROM agents WHERE namespace = ? AND agent_id > ? AND deleted_at IS NULL ORDER BY agent_id LIMIT ?`
      : `SELECT * FROM agents WHERE namespace = ? AND deleted_at IS NULL ORDER BY agent_id LIMIT ?`;
    const stmt = cursor
      ? this.db.prepare(query).bind(namespace, cursor, limit)
      : this.db.prepare(query).bind(namespace, limit);
    const result = await stmt.all<AgentRecord>();
    return result.results ?? [];
  }

  async listPublicAgents(limit: number, cursor: string | null): Promise<AgentRecord[]> {
    const query = cursor
      ? `SELECT * FROM agents WHERE discoverable = 'public' AND agent_id > ? AND deleted_at IS NULL ORDER BY agent_id LIMIT ?`
      : `SELECT * FROM agents WHERE discoverable = 'public' AND deleted_at IS NULL ORDER BY agent_id LIMIT ?`;
    const stmt = cursor ? this.db.prepare(query).bind(cursor, limit) : this.db.prepare(query).bind(limit);
    const result = await stmt.all<AgentRecord>();
    return result.results ?? [];
  }

  async createAgentToken(token: AgentTokenRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO agent_tokens
         (token_id, token_hash, agent_id, user_id, namespace, permissions_json,
          created_at, last_used_at, expires_at, revoked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        token.token_id,
        token.token_hash,
        token.agent_id,
        token.user_id,
        token.namespace,
        token.permissions_json,
        token.created_at,
        token.last_used_at,
        token.expires_at,
        token.revoked_at,
      )
      .run();
  }

  getAgentTokenByHash(tokenHash: string): Promise<AgentTokenRecord | null> {
    return this.db.prepare("SELECT * FROM agent_tokens WHERE token_hash = ?").bind(tokenHash).first<AgentTokenRecord>();
  }

  async revokeAgentToken(tokenId: string, revokedAt: string): Promise<void> {
    await this.db.prepare("UPDATE agent_tokens SET revoked_at = ? WHERE token_id = ?").bind(revokedAt, tokenId).run();
  }

  async createAuthExchange(record: AuthExchangeRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO auth_exchanges
         (exchange_request_id, client_kind, client_version, loopback_redirect_uri,
          code_challenge, state_hash, created_at, expires_at, code_hash,
          code_issued_at, user_id, used_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        record.exchange_request_id,
        record.client_kind,
        record.client_version,
        record.loopback_redirect_uri,
        record.code_challenge,
        record.state_hash,
        record.created_at,
        record.expires_at,
        record.code_hash,
        record.code_issued_at,
        record.user_id,
        record.used_at,
      )
      .run();
  }

  getAuthExchange(exchangeRequestId: string): Promise<AuthExchangeRecord | null> {
    return this.db
      .prepare("SELECT * FROM auth_exchanges WHERE exchange_request_id = ?")
      .bind(exchangeRequestId)
      .first<AuthExchangeRecord>();
  }

  async issueAuthExchangeCode(exchangeRequestId: string, codeHash: string, userId: string, codeIssuedAt: string): Promise<void> {
    await this.db
      .prepare(
        `UPDATE auth_exchanges
         SET code_hash = ?, code_issued_at = ?, user_id = ?
         WHERE exchange_request_id = ? AND used_at IS NULL`,
      )
      .bind(codeHash, codeIssuedAt, userId, exchangeRequestId)
      .run();
  }

  async consumeAuthExchangeCode(exchangeRequestId: string, codeHash: string, userId: string, usedAt: string): Promise<boolean> {
    const result = await this.db
      .prepare(
        `UPDATE auth_exchanges
         SET used_at = ?
         WHERE exchange_request_id = ?
           AND code_hash = ?
           AND user_id = ?
           AND used_at IS NULL`,
      )
      .bind(usedAt, exchangeRequestId, codeHash, userId)
      .run();
    return d1ChangedRows(result) === 1;
  }

  async touchAgent(agentId: string, at: string): Promise<void> {
    await this.db.prepare("UPDATE agents SET last_seen_at = ? WHERE agent_id = ?").bind(at, agentId).run();
  }

  async listTrust(receiverAgentId: string): Promise<TrustGrantRecord[]> {
    const result = await this.db
      .prepare("SELECT * FROM trust_grants WHERE receiver_agent_id = ? ORDER BY granted_at DESC")
      .bind(receiverAgentId)
      .all<TrustGrantRecord>();
    return result.results ?? [];
  }

  getTrust(receiverAgentId: string, senderAgentId: string): Promise<TrustGrantRecord | null> {
    return this.db
      .prepare(
        `SELECT * FROM trust_grants
         WHERE receiver_agent_id = ? AND sender_agent_id = ? AND action_class = 'notification'`,
      )
      .bind(receiverAgentId, senderAgentId)
      .first<TrustGrantRecord>();
  }

  async revokeTrust(receiverAgentId: string, senderAgentId: string, actionClass: string): Promise<void> {
    await this.db
      .prepare("DELETE FROM trust_grants WHERE receiver_agent_id = ? AND sender_agent_id = ? AND action_class = ?")
      .bind(receiverAgentId, senderAgentId, actionClass)
      .run();
  }

  async blockSender(record: BlockRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO block_list (receiver_agent_id, sender_agent_id, blocked_at)
         VALUES (?, ?, ?)
         ON CONFLICT(receiver_agent_id, sender_agent_id) DO UPDATE SET blocked_at = excluded.blocked_at`,
      )
      .bind(record.receiver_agent_id, record.sender_agent_id, record.blocked_at)
      .run();
  }

  async unblockSender(receiverAgentId: string, senderAgentId: string): Promise<void> {
    await this.db
      .prepare("DELETE FROM block_list WHERE receiver_agent_id = ? AND sender_agent_id = ?")
      .bind(receiverAgentId, senderAgentId)
      .run();
  }

  getBlock(receiverAgentId: string, senderAgentId: string): Promise<BlockRecord | null> {
    return this.db
      .prepare("SELECT * FROM block_list WHERE receiver_agent_id = ? AND sender_agent_id = ?")
      .bind(receiverAgentId, senderAgentId)
      .first<BlockRecord>();
  }

  async createNotification(notification: NotificationRecord): Promise<void> {
    await this.db
      .prepare(
        `INSERT INTO notifications
         (notification_id, receiver_agent_id, sender_agent_id, sender_handle, sender_namespace,
          summary, payload_json, payload_visibility, status, first_contact_required,
          created_at, delivered_at, acked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        notification.notification_id,
        notification.receiver_agent_id,
        notification.sender_agent_id,
        notification.sender_handle,
        notification.sender_namespace,
        notification.summary,
        notification.payload_json,
        notification.payload_visibility,
        notification.status,
        notification.first_contact_required,
        notification.created_at,
        notification.delivered_at,
        notification.acked_at,
      )
      .run();
  }

  async markNotificationDelivered(notificationId: string, deliveredAt: string): Promise<void> {
    await this.db
      .prepare("UPDATE notifications SET status = 'delivered', delivered_at = ? WHERE notification_id = ? AND status = 'pending'")
      .bind(deliveredAt, notificationId)
      .run();
  }

  async listNotifications(receiverAgentId: string, limit: number, cursor: string | null): Promise<NotificationRecord[]> {
    const query = cursor
      ? `SELECT * FROM notifications
         WHERE receiver_agent_id = ? AND notification_id > ? AND status IN ('pending', 'delivered')
         ORDER BY notification_id LIMIT ?`
      : `SELECT * FROM notifications
         WHERE receiver_agent_id = ? AND status IN ('pending', 'delivered')
         ORDER BY notification_id LIMIT ?`;
    const stmt = cursor
      ? this.db.prepare(query).bind(receiverAgentId, cursor, limit)
      : this.db.prepare(query).bind(receiverAgentId, limit);
    const result = await stmt.all<NotificationRecord>();
    return result.results ?? [];
  }

  async ackNotifications(receiverAgentId: string, notificationIds: string[], ackedAt: string): Promise<string[]> {
    const acked: string[] = [];
    for (const id of notificationIds) {
      await this.db
        .prepare(
          `UPDATE notifications
           SET status = 'acked', acked_at = ?
           WHERE receiver_agent_id = ? AND notification_id = ? AND status IN ('pending', 'delivered')`,
        )
        .bind(ackedAt, receiverAgentId, id)
        .run();
      acked.push(id);
    }
    return acked;
  }
}

export class MemoryStore implements Store {
  private readonly users = new Map<string, UserRecord>();
  private readonly humanSessionsByHash = new Map<string, HumanSessionRecord>();
  private readonly agents = new Map<string, AgentRecord>();
  private readonly agentTokensByHash = new Map<string, AgentTokenRecord>();
  private readonly trust = new Map<string, TrustGrantRecord>();
  private readonly blocks = new Map<string, BlockRecord>();
  private readonly notifications = new Map<string, NotificationRecord>();

  async createUser(user: UserRecord): Promise<void> {
    if ([...this.users.values()].some((u) => u.username === user.username && u.namespace === user.namespace)) {
      throw ERR.schemaInvalid(["username already exists in namespace"]);
    }
    this.users.set(user.user_id, { ...user });
  }

  async getUserByUsername(username: string): Promise<UserRecord | null> {
    return [...this.users.values()].find((u) => u.username === username) ?? null;
  }

  async getUsersByUsername(username: string): Promise<UserRecord[]> {
    return [...this.users.values()].filter((u) => u.username === username).sort((a, b) => a.namespace.localeCompare(b.namespace));
  }

  async getUserByIdentity(username: string, namespace: string): Promise<UserRecord | null> {
    return [...this.users.values()].find((u) => u.username === username && u.namespace === namespace) ?? null;
  }

  async getUser(userId: string): Promise<UserRecord | null> {
    return this.users.get(userId) ?? null;
  }

  async getUserByNamespace(namespace: string): Promise<UserRecord | null> {
    return [...this.users.values()].find((u) => u.namespace === namespace) ?? null;
  }

  async createHumanSession(session: HumanSessionRecord): Promise<void> {
    this.humanSessionsByHash.set(session.session_hash, { ...session });
  }

  async getHumanSessionByHash(sessionHash: string): Promise<HumanSessionRecord | null> {
    return this.humanSessionsByHash.get(sessionHash) ?? null;
  }

  async createAgent(agent: AgentRecord): Promise<void> {
    if ([...this.agents.values()].some((a) => a.namespace === agent.namespace && a.handle === agent.handle)) {
      throw ERR.schemaInvalid(["handle already exists in namespace"]);
    }
    this.agents.set(agent.agent_id, { ...agent });
  }

  async updateAgent(agent: AgentRecord): Promise<void> {
    this.agents.set(agent.agent_id, { ...agent });
  }

  async getAgent(agentId: string): Promise<AgentRecord | null> {
    return this.agents.get(agentId) ?? null;
  }

  async getAgentByHandle(namespace: string, handle: string): Promise<AgentRecord | null> {
    return [...this.agents.values()].find((a) => a.namespace === namespace && a.handle === handle) ?? null;
  }

  async listAgentsByNamespace(namespace: string, limit: number, cursor: string | null): Promise<AgentRecord[]> {
    return pageByAgentId(
      [...this.agents.values()].filter((a) => a.namespace === namespace && !a.deleted_at),
      limit,
      cursor,
    );
  }

  async listPublicAgents(limit: number, cursor: string | null): Promise<AgentRecord[]> {
    return pageByAgentId(
      [...this.agents.values()].filter((a) => a.discoverable === "public" && !a.deleted_at),
      limit,
      cursor,
    );
  }

  async createAgentToken(token: AgentTokenRecord): Promise<void> {
    this.agentTokensByHash.set(token.token_hash, { ...token });
  }

  async getAgentTokenByHash(tokenHash: string): Promise<AgentTokenRecord | null> {
    return this.agentTokensByHash.get(tokenHash) ?? null;
  }

  async revokeAgentToken(tokenId: string, revokedAt: string): Promise<void> {
    for (const [hash, token] of this.agentTokensByHash.entries()) {
      if (token.token_id === tokenId) {
        this.agentTokensByHash.set(hash, { ...token, revoked_at: revokedAt });
      }
    }
  }

  private readonly authExchanges = new Map<string, AuthExchangeRecord>();

  async createAuthExchange(record: AuthExchangeRecord): Promise<void> {
    this.authExchanges.set(record.exchange_request_id, { ...record });
  }

  async getAuthExchange(exchangeRequestId: string): Promise<AuthExchangeRecord | null> {
    const record = this.authExchanges.get(exchangeRequestId);
    return record ? { ...record } : null;
  }

  async issueAuthExchangeCode(exchangeRequestId: string, codeHash: string, userId: string, codeIssuedAt: string): Promise<void> {
    const record = this.authExchanges.get(exchangeRequestId);
    if (record && !record.used_at) {
      this.authExchanges.set(exchangeRequestId, { ...record, code_hash: codeHash, code_issued_at: codeIssuedAt, user_id: userId });
    }
  }

  async consumeAuthExchangeCode(exchangeRequestId: string, codeHash: string, userId: string, usedAt: string): Promise<boolean> {
    const record = this.authExchanges.get(exchangeRequestId);
    if (!record || record.used_at || record.code_hash !== codeHash || record.user_id !== userId) {
      return false;
    }
    this.authExchanges.set(exchangeRequestId, { ...record, used_at: usedAt });
    return true;
  }

  async touchAgent(agentId: string, at: string): Promise<void> {
    const agent = this.agents.get(agentId);
    if (agent) {
      this.agents.set(agentId, { ...agent, last_seen_at: at });
    }
  }

  async listTrust(receiverAgentId: string): Promise<TrustGrantRecord[]> {
    return [...this.trust.values()].filter((t) => t.receiver_agent_id === receiverAgentId);
  }

  async getTrust(receiverAgentId: string, senderAgentId: string): Promise<TrustGrantRecord | null> {
    return this.trust.get(trustKey(receiverAgentId, senderAgentId, "notification")) ?? null;
  }

  async revokeTrust(receiverAgentId: string, senderAgentId: string, actionClass: string): Promise<void> {
    this.trust.delete(trustKey(receiverAgentId, senderAgentId, actionClass));
  }

  async blockSender(record: BlockRecord): Promise<void> {
    this.blocks.set(blockKey(record.receiver_agent_id, record.sender_agent_id), { ...record });
  }

  async unblockSender(receiverAgentId: string, senderAgentId: string): Promise<void> {
    this.blocks.delete(blockKey(receiverAgentId, senderAgentId));
  }

  async getBlock(receiverAgentId: string, senderAgentId: string): Promise<BlockRecord | null> {
    return this.blocks.get(blockKey(receiverAgentId, senderAgentId)) ?? null;
  }

  async createNotification(notification: NotificationRecord): Promise<void> {
    this.notifications.set(notification.notification_id, { ...notification });
  }

  async markNotificationDelivered(notificationId: string, deliveredAt: string): Promise<void> {
    const notification = this.notifications.get(notificationId);
    if (notification && notification.status === "pending") {
      this.notifications.set(notificationId, { ...notification, status: "delivered", delivered_at: deliveredAt });
    }
  }

  async listNotifications(receiverAgentId: string, limit: number, cursor: string | null): Promise<NotificationRecord[]> {
    const notifications = [...this.notifications.values()].filter(
      (n) => n.receiver_agent_id === receiverAgentId && (n.status === "pending" || n.status === "delivered"),
    );
    return pageByNotificationId(notifications, limit, cursor);
  }

  async ackNotifications(receiverAgentId: string, notificationIds: string[], ackedAt: string): Promise<string[]> {
    const acked: string[] = [];
    for (const notificationId of notificationIds) {
      const notification = this.notifications.get(notificationId);
      if (notification && notification.receiver_agent_id === receiverAgentId) {
        this.notifications.set(notificationId, { ...notification, status: "acked", acked_at: ackedAt });
        acked.push(notificationId);
      }
    }
    return acked;
  }
}

class DurableMailbox implements Mailbox {
  constructor(private readonly namespace: DurableObjectNamespace) {}

  async connect(agentId: string): Promise<Response> {
    const id = this.namespace.idFromName(agentId);
    const stub = this.namespace.get(id);
    return stub.fetch("https://mailbox/connect", { method: "GET" });
  }

  async push(agentId: string, notification: NotificationRecord): Promise<boolean> {
    const id = this.namespace.idFromName(agentId);
    const stub = this.namespace.get(id);
    const response = await stub.fetch("https://mailbox/push", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ notification }),
    });
    if (!response.ok) {
      return false;
    }
    const body = (await response.json()) as { delivered?: boolean };
    return Boolean(body.delivered);
  }
}

export class MemoryMailbox implements Mailbox {
  private readonly sessions = new Map<string, Set<ReadableStreamDefaultController<Uint8Array>>>();

  connect(agentId: string): Response {
    const encoder = new TextEncoder();
    const sessions = this.sessions;
    let current: ReadableStreamDefaultController<Uint8Array> | null = null;
    const stream = new ReadableStream<Uint8Array>({
      start: (controller) => {
        current = controller;
        const set = sessions.get(agentId) ?? new Set<ReadableStreamDefaultController<Uint8Array>>();
        set.add(controller);
        sessions.set(agentId, set);
        controller.enqueue(encoder.encode(": connected\n\n"));
      },
      cancel: () => {
        const set = sessions.get(agentId);
        if (set && current) {
          set.delete(current);
        }
      },
    });
    return new Response(stream, sseHeaders());
  }

  async push(agentId: string, notification: NotificationRecord): Promise<boolean> {
    const set = this.sessions.get(agentId);
    if (!set || set.size === 0) {
      return false;
    }
    const encoder = new TextEncoder();
    const bytes = encoder.encode(toSseEvent(notification));
    for (const controller of set) {
      try {
        controller.enqueue(bytes);
      } catch {
        set.delete(controller);
      }
    }
    return set.size > 0;
  }
}

export class AgentMailbox {
  private readonly sessions = new Set<WritableStreamDefaultWriter<Uint8Array>>();
  private readonly encoder = new TextEncoder();

  constructor(private readonly state: DurableObjectState) {
    void this.state.blockConcurrencyWhile(async () => undefined);
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/connect") {
      const stream = new TransformStream<Uint8Array, Uint8Array>();
      const writer = stream.writable.getWriter();
      this.sessions.add(writer);
      void writer.closed.catch(() => undefined).finally(() => {
        this.sessions.delete(writer);
      });
      void writer.write(this.encoder.encode(": connected\n\n")).catch(() => {
        this.sessions.delete(writer);
      });
      return new Response(stream.readable, sseHeaders());
    }

    if (request.method === "POST" && url.pathname === "/push") {
      const body = (await request.json()) as { notification?: NotificationRecord };
      if (!body.notification) {
        return json({ delivered: false }, 400);
      }
      const bytes = this.encoder.encode(toSseEvent(body.notification));
      let delivered = 0;
      for (const writer of [...this.sessions]) {
        try {
          void writer.write(bytes).catch(() => {
            this.sessions.delete(writer);
          });
          delivered += 1;
        } catch {
          this.sessions.delete(writer);
        }
      }
      return json({ delivered: delivered > 0 });
    }

    return json({ error: "not_found" }, 404);
  }
}

export class HubApp {
  constructor(
    private readonly store: Store,
    private readonly mailbox: Mailbox,
    private readonly version: string,
  ) {}

  async fetch(request: Request): Promise<Response> {
    try {
      const url = new URL(request.url);
      if (request.method === "GET" && url.pathname === "/health") {
        return json({ ok: true, version: this.version, protocol_version: PROTOCOL_VERSION });
      }
      if (url.pathname === "/auth/start" && request.method === "POST") {
        return json(await this.startAuth(await readJsonObject(request), url.origin));
      }
      if (url.pathname === "/auth/exchange_code" && request.method === "POST") {
        return json(await this.exchangeAuthCode(await readJsonObject(request)));
      }
      if (url.pathname === "/auth/exchange_manual_code" && request.method === "POST") {
        return json(await this.exchangeManualAuthCode(await readJsonObject(request)));
      }
      if (url.pathname === "/auth/register" && request.method === "POST") {
        return json(await this.registerUser(await readJsonObject(request)));
      }
      if (url.pathname === "/auth/login" && request.method === "POST") {
        return json(await this.loginUser(await readJsonObject(request)));
      }
      if (url.pathname === "/login" && request.method === "GET") {
        return this.loginPage(url);
      }
      if (url.pathname === "/login" && request.method === "POST") {
        return await this.completeBrowserLoginForm(await readFormObject(request));
      }
      if (url.pathname === "/chat" && request.method === "GET") {
        return await this.chatPage(request);
      }
      if (url.pathname === "/chat/login" && request.method === "POST") {
        return await this.completeChatLoginForm(request, await readFormObject(request));
      }
      if (url.pathname === "/chat/send" && request.method === "POST") {
        return await this.completeChatSendForm(request, await readFormObject(request));
      }
      if (url.pathname === "/mcp" && request.method === "GET") {
        const principal = await this.authenticate(request, "agent");
        return this.mailbox.connect(principal.agent_id);
      }
      if (url.pathname === "/mcp" && request.method === "POST") {
        return this.handleMcpPost(request);
      }
      return json({ error: "not_found" }, 404);
    } catch (error) {
      return httpError(error);
    }
  }

  private async handleMcpPost(request: Request): Promise<Response> {
    const bodyText = await request.text();
    if (byteLength(bodyText) > JSON_LIMIT_BYTES) {
      const error = ERR.bodyTooLarge(JSON_LIMIT_BYTES);
      return json(jsonRpcError(null, error), 200);
    }
    let payload: unknown;
    try {
      payload = JSON.parse(bodyText);
    } catch {
      const error = ERR.schemaInvalid(["body must be valid JSON"]);
      return json(jsonRpcError(null, error), 200);
    }
    if (!isObject(payload) || Array.isArray(payload)) {
      const error = ERR.schemaInvalid(["JSON-RPC batch requests are not supported in v0"]);
      return json(jsonRpcError(null, error), 200);
    }

    const id = jsonRpcId(payload.id);
    try {
      const result = await this.handleJsonRpc(request, payload, id);
      if (payload.id === undefined) {
        return new Response(null, { status: 202 });
      }
      return json({ jsonrpc: "2.0", id, result });
    } catch (error) {
      return json(jsonRpcError(id, coercePublicError(error)), 200);
    }
  }

  private async handleJsonRpc(request: Request, payload: Record<string, unknown>, id: string | number | null): Promise<unknown> {
    requireJsonRpc(payload);
    const method = stringField(payload, "method");
    const params = optionalObject(payload.params, "params");

    if (method === "initialize") {
      return {
        protocolVersion: PROTOCOL_VERSION,
        serverInfo: { name: "pie-hub", version: this.version },
        capabilities: {
          tools: { listChanged: false },
          resources: { subscribe: false, listChanged: false },
        },
      };
    }
    if (method === "tools/list") {
      return { tools: TOOL_DEFINITIONS };
    }
    if (method === "resources/list") {
      return {
        resources: [
          { uri: "agent://{agent_id}", name: "Agent profile", mimeType: "application/json" },
          { uri: "inbox://{agent_id}", name: "Agent inbox", mimeType: "application/json" },
          { uri: "trust://{agent_id}", name: "Agent trust list", mimeType: "application/json" },
        ],
      };
    }
    if (method === "resources/read") {
      const principal = await this.authenticate(request, "agent");
      return this.readResource(principal, params);
    }
    if (method === "tools/call") {
      const toolName = stringField(params, "name");
      const args = optionalObject(params.arguments, "arguments");
      const output = await this.callTool(request, toolName, args);
      return {
        content: [{ type: "text", text: JSON.stringify(output) }],
      };
    }
    throw ERR.schemaInvalid([`unsupported method ${method}`]);
  }

  private async callTool(request: Request, toolName: string, args: Record<string, unknown>): Promise<unknown> {
    switch (toolName) {
      case "register_agent":
        return this.registerAgent(await this.authenticate(request, "human"), args);
      case "update_agent_profile":
        return this.updateAgentProfile(await this.authenticate(request, "agent"), args);
      case "rotate_agent_token":
        return this.rotateAgentToken(await this.authenticate(request, "agent"), args);
      case "revoke_agent_token":
        return this.revokeAgentToken(await this.authenticate(request, "agent"), args);
      case "delete_agent":
        return this.deleteAgent(await this.authenticate(request, "agent"), args);
      case "list_my_agents":
        return this.listMyAgents(await this.authenticate(request, "human_or_agent"), args);
      case "discover_public_agents":
        return this.discoverPublicAgents(await this.authenticate(request, "agent"), args);
      case "get_agent_profile":
        return this.getAgentProfile(await this.authenticate(request, "agent"), args);
      case "send_notification":
        return this.sendNotification(await this.authenticate(request, "agent"), args);
      case "list_my_inbox":
        return this.listMyInbox(await this.authenticate(request, "agent"), args);
      case "ack_notification":
        return this.ackNotification(await this.authenticate(request, "agent"), args);
      case "list_trust":
        return this.listTrust(await this.authenticate(request, "agent"), args);
      case "revoke_trust":
        return this.revokeTrust(await this.authenticate(request, "agent"), args);
      case "block_sender":
        return this.blockSender(await this.authenticate(request, "agent"), args);
      case "unblock_sender":
        return this.unblockSender(await this.authenticate(request, "agent"), args);
      default:
        throw ERR.schemaInvalid([`unknown tool ${toolName}`]);
    }
  }

  private async authenticate(
    request: Request,
    accepts: "human" | "agent" | "human_or_agent",
  ): Promise<Principal & Record<string, never>> {
    const header = request.headers.get("authorization");
    if (!header) {
      throw ERR.authRequired();
    }
    const match = /^Bearer\s+(.+)$/i.exec(header);
    if (!match) {
      throw ERR.authInvalid();
    }
    const token = match[1];
    const hash = await sha256Hex(token);
    const now = nowIso();

    if (token.startsWith("hub_hs_") && (accepts === "human" || accepts === "human_or_agent")) {
      const session = await this.store.getHumanSessionByHash(hash);
      if (!session) {
        throw ERR.authInvalid();
      }
      if (session.revoked_at || session.expires_at <= now) {
        throw ERR.sessionExpired();
      }
      return {
        kind: "human",
        user_id: session.user_id,
        namespace: session.namespace,
        session_id: session.session_id,
      } as Principal & Record<string, never>;
    }

    if (token.startsWith("hub_agent_") && (accepts === "agent" || accepts === "human_or_agent")) {
      const tokenRecord = await this.store.getAgentTokenByHash(hash);
      if (!tokenRecord) {
        throw ERR.authInvalid();
      }
      if (tokenRecord.revoked_at) {
        throw ERR.authRevoked();
      }
      if (tokenRecord.expires_at && tokenRecord.expires_at <= now) {
        throw ERR.authRevoked();
      }
      const agent = await this.store.getAgent(tokenRecord.agent_id);
      if (!agent || agent.deleted_at) {
        throw ERR.notFound();
      }
      await this.store.touchAgent(tokenRecord.agent_id, now);
      return {
        kind: "agent",
        user_id: tokenRecord.user_id,
        namespace: tokenRecord.namespace,
        agent_id: tokenRecord.agent_id,
        token_id: tokenRecord.token_id,
        permissions: parseJsonArray(tokenRecord.permissions_json),
      } as Principal & Record<string, never>;
    }

    throw accepts === "agent" ? ERR.authRequired() : ERR.authInvalid();
  }

  private async authenticateWebHuman(request: Request): Promise<Extract<Principal, { kind: "human" }> | null> {
    const token = cookieValue(request, WEB_SESSION_COOKIE);
    if (!token?.startsWith("hub_hs_")) {
      return null;
    }
    const session = await this.store.getHumanSessionByHash(await sha256Hex(token));
    const now = nowIso();
    if (!session || session.revoked_at || session.expires_at <= now) {
      return null;
    }
    return {
      kind: "human",
      user_id: session.user_id,
      namespace: session.namespace,
      session_id: session.session_id,
    };
  }

  private async startAuth(args: Record<string, unknown>, origin: string): Promise<unknown> {
    ensureOnly(args, ["client_kind", "client_version", "loopback_redirect_uri", "code_challenge", "code_challenge_method", "state"]);
    const clientKind = stringField(args, "client_kind");
    if (clientKind !== "pie-cli") {
      throw ERR.schemaInvalid(["client_kind must be pie-cli"]);
    }
    const clientVersion = validatePlainText(stringField(args, "client_version"), "client_version", 64);
    const loopbackRedirectUri = validateLoopbackRedirectUri(stringField(args, "loopback_redirect_uri"));
    const codeChallenge = validatePkceValue(stringField(args, "code_challenge"), "code_challenge");
    if (stringField(args, "code_challenge_method") !== "S256") {
      throw ERR.schemaInvalid(["code_challenge_method must be S256"]);
    }
    const state = validateOpaqueValue(stringField(args, "state"), "state");
    const exchangeRequestId = crypto.randomUUID();
    const createdAt = nowIso();
    await this.store.createAuthExchange({
      exchange_request_id: exchangeRequestId,
      client_kind: clientKind,
      client_version: clientVersion,
      loopback_redirect_uri: loopbackRedirectUri,
      code_challenge: codeChallenge,
      state_hash: await sha256Hex(state),
      created_at: createdAt,
      expires_at: addSecondsIso(AUTH_EXCHANGE_TTL_SECONDS),
      code_hash: null,
      code_issued_at: null,
      user_id: null,
      used_at: null,
    });
    const loginUrl = new URL("/login", origin);
    loginUrl.searchParams.set("req", exchangeRequestId);
    loginUrl.searchParams.set("state", state);
    return {
      exchange_request_id: exchangeRequestId,
      login_url: loginUrl.toString(),
      expires_in_seconds: AUTH_EXCHANGE_TTL_SECONDS,
    };
  }

  private loginPage(url: URL): Response {
    return this.loginForm(
      url.searchParams.get("req") ?? "",
      url.searchParams.get("state") ?? null,
      undefined,
      200,
      url.searchParams.get("manual") === "1" ? "manual" : "loopback",
    );
  }

  private loginForm(
    reqRaw: string,
    stateRaw: string | null,
    errorMessage?: string,
    status = 200,
    delivery: "loopback" | "manual" = "loopback",
  ): Response {
    const req = escapeHtml(reqRaw);
    const state = escapeHtml(stateRaw ?? "");
    const hiddenFields = `<input type="hidden" name="exchange_request_id" value="${req}">
      <input type="hidden" name="state" value="${state}">
      <input type="hidden" name="delivery" value="${delivery}">`;
    const manualUrl = new URLSearchParams({ req: reqRaw, ...(stateRaw ? { state: stateRaw } : {}), manual: "1" }).toString();
    const loopbackUrl = new URLSearchParams({ req: reqRaw, ...(stateRaw ? { state: stateRaw } : {}) }).toString();
    const fallback = delivery === "manual"
      ? `<p class="fallback"><a href="/login?${escapeHtml(loopbackUrl)}">Use loopback callback instead</a></p>`
      : `<p class="fallback"><a href="/login?${escapeHtml(manualUrl)}">Using SSH or no browser callback? Show a one-time paste code instead.</a></p>`;
    const error = errorMessage
      ? `<div class="notice error" role="alert">Could not complete sign-in: ${escapeHtml(errorMessage)}</div>`
      : "";
    const body = `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Join pie hub</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #f6f7f9;
    --fg: #15181d;
    --muted: #5d6673;
    --line: #d9dee7;
    --panel: #ffffff;
    --accent: #1868d8;
    --accent-fg: #ffffff;
    --error-bg: #fff1f1;
    --error-fg: #9b1c1c;
  }
  * { box-sizing: border-box; }
  html, body {
    width: 100%;
    max-width: 100%;
    overflow-x: hidden;
  }
  body {
    margin: 0;
    min-height: 100vh;
    background: var(--bg);
    color: var(--fg);
    font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    line-height: 1.45;
  }
  main {
    width: min(960px, 100%);
    margin: 0 auto;
    padding: 40px 20px;
  }
  header {
    max-width: 660px;
    margin-bottom: 24px;
  }
  h1 {
    margin: 0 0 8px;
    font-size: 28px;
    line-height: 1.15;
  }
  h2 {
    margin: 0 0 6px;
    font-size: 18px;
  }
  p {
    margin: 0;
    color: var(--muted);
  }
  .forms {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 16px;
  }
  form {
    display: grid;
    gap: 14px;
    padding: 20px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--panel);
  }
  .form-copy {
    min-height: 52px;
  }
  label {
    display: grid;
    gap: 6px;
    font-size: 14px;
    font-weight: 600;
  }
  input {
    width: 100%;
    min-height: 42px;
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 9px 11px;
    background: transparent;
    color: var(--fg);
    font: inherit;
  }
  input:focus {
    outline: 2px solid color-mix(in srgb, var(--accent) 35%, transparent);
    border-color: var(--accent);
  }
  .hint {
    color: var(--muted);
    font-size: 13px;
    font-weight: 400;
  }
  .fallback {
    margin-top: 16px;
    font-size: 14px;
  }
  .fallback a { color: var(--accent); }
  button {
    min-height: 42px;
    border: 0;
    border-radius: 6px;
    padding: 10px 14px;
    background: var(--accent);
    color: var(--accent-fg);
    font: inherit;
    font-weight: 650;
    cursor: pointer;
  }
  .notice {
    margin-bottom: 16px;
    padding: 12px 14px;
    border-radius: 8px;
    border: 1px solid var(--line);
    background: var(--panel);
  }
  .error {
    border-color: color-mix(in srgb, var(--error-fg) 35%, var(--line));
    background: var(--error-bg);
    color: var(--error-fg);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #101215;
      --fg: #eff2f5;
      --muted: #a2aab5;
      --line: #303844;
      --panel: #181c22;
      --accent: #6da2ff;
      --accent-fg: #07111f;
      --error-bg: #2b1719;
      --error-fg: #ffb4b4;
    }
  }
  @media (max-width: 720px) {
    main { padding: 24px 14px; }
    .forms { grid-template-columns: 1fr; }
    .form-copy { min-height: 0; }
  }
</style>
<main>
  <header>
    <h1>Join pie.0xfefe.me</h1>
    <p>Sign in or create a hub account, then return to your pie terminal to finish connecting.</p>
  </header>
  ${error}
  <section class="forms" aria-label="Hub account actions">
    <form method="post" action="/login" autocomplete="on">
      ${hiddenFields}
      <input type="hidden" name="mode" value="login">
      <div class="form-copy">
        <h2>Sign in</h2>
        <p>Use an existing hub account.</p>
      </div>
      <label>Username
        <input name="username" autocomplete="username" autocapitalize="none" spellcheck="false" required>
      </label>
      <label>Namespace
        <input name="namespace" autocomplete="organization" autocapitalize="none" spellcheck="false" placeholder="team-name or your-handle">
        <span class="hint">Use the namespace from your pie identity: name@namespace.</span>
      </label>
      <label>Password
        <input name="password" type="password" autocomplete="current-password" required>
      </label>
      <button type="submit">Sign in</button>
    </form>
    <form method="post" action="/login" autocomplete="on">
      ${hiddenFields}
      <input type="hidden" name="mode" value="register">
      <div class="form-copy">
        <h2>Create account</h2>
        <p>Your pie identity is shown as name@namespace.</p>
      </div>
      <label>Username
        <input name="username" autocomplete="username" autocapitalize="none" spellcheck="false" required>
        <span class="hint">2-32 lowercase letters, numbers, underscores, or hyphens.</span>
      </label>
      <label>Namespace
        <input name="namespace" autocomplete="organization" autocapitalize="none" spellcheck="false" placeholder="team-name or your-handle">
        <span class="hint">Optional; defaults to username. Members in one namespace can send hub messages directly.</span>
      </label>
      <label>Password
        <input name="password" type="password" autocomplete="new-password" minlength="12" required>
        <span class="hint">At least 12 characters.</span>
      </label>
      <button type="submit">Create account</button>
    </form>
  </section>
  ${fallback}
</main>
</html>`;
    return new Response(body, {
      status,
      headers: {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "no-store",
        "referrer-policy": "no-referrer",
      },
    });
  }

  private async completeBrowserLoginForm(args: Record<string, unknown>): Promise<Response> {
    try {
      return await this.completeBrowserLogin(args);
    } catch (error) {
      const publicError = coercePublicError(error);
      return this.loginForm(
        typeof args.exchange_request_id === "string" ? args.exchange_request_id : "",
        typeof args.state === "string" ? args.state : "",
        browserLoginErrorMessage(publicError, args),
        publicError.code === -32009 || publicError.code === -32010 ? 401 : 400,
        optionalDelivery(args.delivery),
      );
    }
  }

  private async completeBrowserLogin(args: Record<string, unknown>): Promise<Response> {
    ensureOnly(args, ["mode", "username", "password", "namespace", "exchange_request_id", "state", "delivery"]);
    const mode = optionalString(args.mode, "mode") ?? "login";
    const delivery = optionalDelivery(args.delivery);
    const username = normalizeName(stringField(args, "username"), "username");
    const password = stringField(args, "password");
    const exchange = await this.requireAuthExchange(stringField(args, "exchange_request_id"), stringField(args, "state"));
    let user: UserRecord;
    if (mode === "register") {
      user = await this.createUserFromCredentials(username, password, optionalString(args.namespace, "namespace"));
    } else if (mode === "login") {
      user = await this.requireUserPassword(username, password, optionalString(args.namespace, "namespace"));
    } else {
      throw ERR.schemaInvalid(["mode must be login or register"]);
    }
    if (delivery === "manual") {
      const manualCode = manualAuthCode();
      await this.store.issueAuthExchangeCode(exchange.exchange_request_id, await sha256Hex(manualCode), user.user_id, nowIso());
      return this.manualCodePage(manualCode);
    }
    const code = `hub_code_${crypto.randomUUID()}_${randomSecret(24)}`;
    await this.store.issueAuthExchangeCode(exchange.exchange_request_id, await sha256Hex(code), user.user_id, nowIso());
    const redirect = new URL(exchange.loopback_redirect_uri);
    redirect.searchParams.set("code", code);
    redirect.searchParams.set("state", stringField(args, "state"));
    return new Response(null, {
      status: 302,
      headers: {
        location: redirect.toString(),
        "cache-control": "no-store",
        "referrer-policy": "no-referrer",
      },
    });
  }

  private manualCodePage(manualCode: string): Response {
    return html(`<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Paste code into pie</title>
<style>
  :root { color-scheme: light dark; --bg: #f6f7f9; --fg: #15181d; --muted: #5d6673; --line: #d9dee7; --panel: #ffffff; }
  * { box-sizing: border-box; }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 24px; background: var(--bg); color: var(--fg); font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
  main { width: min(520px, 100%); padding: 24px; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); }
  h1 { margin: 0 0 8px; font-size: 24px; }
  p { margin: 0 0 16px; color: var(--muted); line-height: 1.45; }
  code { display: block; padding: 16px; border: 1px solid var(--line); border-radius: 8px; text-align: center; font: 700 28px/1.2 ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: 0.08em; }
  @media (prefers-color-scheme: dark) { :root { --bg: #101215; --fg: #eff2f5; --muted: #a2aab5; --line: #303844; --panel: #181c22; } }
</style>
<main>
  <h1>Paste this code into pie</h1>
  <p>This one-time code expires with the join request and can be used only once.</p>
  <code>${escapeHtml(manualCode)}</code>
</main>
</html>`);
  }

  private async chatPage(request: Request): Promise<Response> {
    const principal = await this.authenticateWebHuman(request);
    if (!principal) {
      return this.chatLoginForm();
    }
    const user = await this.store.getUser(principal.user_id);
    if (!user) {
      return this.chatLoginForm("Session expired. Sign in again.", 401);
    }
    return this.chatComposePage(user);
  }

  private chatLoginForm(errorMessage?: string, status = 200): Response {
    const error = errorMessage ? `<div class="notice error" role="alert">${escapeHtml(errorMessage)}</div>` : "";
    return html(
      `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pie hub chat</title>
${chatStyle()}
<main>
  <header>
    <h1>Pie hub chat</h1>
    <p>Sign in or create an account to send a short text message to a connected pie agent.</p>
  </header>
  ${error}
  <section class="forms" aria-label="Hub account actions">
    <form method="post" action="/chat/login" autocomplete="on">
      <input type="hidden" name="mode" value="login">
      <div class="form-copy">
        <h2>Sign in</h2>
        <p>Use an existing hub account.</p>
      </div>
      <label>Username
        <input name="username" autocomplete="username" autocapitalize="none" spellcheck="false" required>
      </label>
      <label>Namespace
        <input name="namespace" autocomplete="organization" autocapitalize="none" spellcheck="false" placeholder="team-name or your-handle">
        <span class="hint">Use the namespace from your pie identity: name@namespace.</span>
      </label>
      <label>Password
        <input name="password" type="password" autocomplete="current-password" required>
      </label>
      <button type="submit">Sign in</button>
    </form>
    <form method="post" action="/chat/login" autocomplete="on">
      <input type="hidden" name="mode" value="register">
      <div class="form-copy">
        <h2>Create account</h2>
        <p>Your pie identity is shown as name@namespace.</p>
      </div>
      <label>Username
        <input name="username" autocomplete="username" autocapitalize="none" spellcheck="false" required>
        <span class="hint">2-32 lowercase letters, numbers, underscores, or hyphens.</span>
      </label>
      <label>Namespace
        <input name="namespace" autocomplete="organization" autocapitalize="none" spellcheck="false" placeholder="team-name or your-handle">
        <span class="hint">Optional; defaults to username. Members in one namespace can send hub messages directly.</span>
      </label>
      <label>Password
        <input name="password" type="password" autocomplete="new-password" minlength="12" required>
        <span class="hint">At least 12 characters.</span>
      </label>
      <button type="submit">Create account</button>
    </form>
  </section>
</main>
</html>`,
      status,
    );
  }

  private async completeChatLoginForm(request: Request, args: Record<string, unknown>): Promise<Response> {
    try {
      enforceSameOrigin(request);
      ensureOnly(args, ["mode", "username", "password", "namespace"]);
      const mode = optionalString(args.mode, "mode") ?? "login";
      const username = normalizeName(stringField(args, "username"), "username");
      const password = stringField(args, "password");
      let user: UserRecord;
      if (mode === "register") {
        user = await this.createUserFromCredentials(username, password, optionalString(args.namespace, "namespace"));
      } else if (mode === "login") {
        user = await this.requireUserPassword(username, password, optionalString(args.namespace, "namespace"));
      } else {
        throw ERR.schemaInvalid(["mode must be login or register"]);
      }
      const sessionToken = await this.issueHumanSession(user);
      return new Response(null, {
        status: 303,
        headers: {
          location: "/chat",
          "set-cookie": webSessionCookie(sessionToken),
          "cache-control": "no-store",
          "referrer-policy": "no-referrer",
        },
      });
    } catch (error) {
      const publicError = coercePublicError(error);
      return this.chatLoginForm(chatErrorMessage(publicError, args), publicError.code === -32009 || publicError.code === -32010 ? 401 : 400);
    }
  }

  private chatComposePage(
    user: UserRecord,
    result?: { tone: "success" | "error"; message: string },
    status = 200,
  ): Response {
    const signedInIdentity = safeDisplayMention({ handle: user.username, namespace: user.namespace });
    const notice = result
      ? `<div class="notice ${result.tone === "error" ? "error" : "success"}" role="status">${escapeHtml(result.message)}</div>`
      : "";
    return html(
      `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pie hub chat</title>
${chatStyle()}
<main>
  <header>
    <h1>Pie hub chat</h1>
    <p>Signed in as <strong>${escapeHtml(signedInIdentity)}</strong>. Send a short text message to a connected TUI agent.</p>
  </header>
  ${notice}
  <form method="post" action="/chat/send" autocomplete="off">
    <label>Recipient
      <input name="recipient" autocapitalize="none" spellcheck="false" placeholder="name@namespace" required>
      <span class="hint">Use the agent's hub identity. Same-namespace recipients receive directly; cross-namespace recipients may see a first-contact prompt.</span>
    </label>
    <label>Message
      <textarea name="message" maxlength="${SUMMARY_LIMIT_CHARS}" required></textarea>
      <span class="hint">Plain text only, ${SUMMARY_LIMIT_CHARS} characters max. Attachments and local payloads are not sent from web chat.</span>
    </label>
    <button type="submit">Send message</button>
  </form>
</main>
</html>`,
      status,
    );
  }

  private async completeChatSendForm(request: Request, args: Record<string, unknown>): Promise<Response> {
    const principal = await this.authenticateWebHuman(request);
    if (!principal) {
      return this.chatLoginForm("Sign in before sending a hub message.", 401);
    }
    const user = await this.store.getUser(principal.user_id);
    if (!user) {
      return this.chatLoginForm("Session expired. Sign in again.", 401);
    }
    try {
      enforceSameOrigin(request);
      ensureOnly(args, ["recipient", "message"]);
      const recipient = parseDisplayAgentHandle(stringField(args, "recipient"), "recipient");
      const recipientDisplay = safeDisplayMention(recipient);
      const summary = validateSummary(stringField(args, "message"));
      const receiver = await this.store.getAgentByHandle(recipient.namespace, recipient.handle);
      if (!receiver || receiver.deleted_at) {
        throw ERR.notFound(`No reachable agent named ${recipientDisplay}. Check spelling or ask them to join hub.`);
      }
      const sender = await this.ensureDefaultAgent(user);
      const receipt = await this.sendNotificationAsAgent(sender, {
        target_agent_id: receiver.agent_id,
        summary,
        payload_visibility: "Local",
      });
      const statusText = isObject(receipt) && typeof receipt.status === "string" ? receipt.status : "accepted";
      return this.chatComposePage(user, {
        tone: "success",
        message: `Message ${statusText} to ${recipientDisplay}.`,
      });
    } catch (error) {
      return this.chatComposePage(user, { tone: "error", message: chatErrorMessage(coercePublicError(error), args) }, 400);
    }
  }

  private async exchangeAuthCode(args: Record<string, unknown>): Promise<unknown> {
    ensureOnly(args, ["exchange_request_id", "code", "state", "code_verifier"]);
    return this.exchangeIssuedCode({
      exchange_request_id: stringField(args, "exchange_request_id"),
      code: validateLoopbackAuthCode(stringField(args, "code")),
      state: stringField(args, "state"),
      code_verifier: stringField(args, "code_verifier"),
    });
  }

  private async exchangeManualAuthCode(args: Record<string, unknown>): Promise<unknown> {
    ensureOnly(args, ["exchange_request_id", "manual_code", "state", "code_verifier"]);
    return this.exchangeIssuedCode({
      exchange_request_id: stringField(args, "exchange_request_id"),
      code: validateManualAuthCode(stringField(args, "manual_code")),
      state: stringField(args, "state"),
      code_verifier: stringField(args, "code_verifier"),
    });
  }

  private async exchangeIssuedCode(args: {
    exchange_request_id: string;
    code: string;
    state: string;
    code_verifier: string;
  }): Promise<unknown> {
    const exchange = await this.requireAuthExchange(args.exchange_request_id, args.state);
    if (!exchange.code_hash || !exchange.user_id || exchange.used_at) {
      throw ERR.authInvalid();
    }
    if ((await sha256Hex(args.code)) !== exchange.code_hash) {
      throw ERR.authInvalid();
    }
    const verifier = validatePkceValue(args.code_verifier, "code_verifier");
    if ((await sha256Base64Url(verifier)) !== exchange.code_challenge) {
      throw ERR.authInvalid();
    }
    const user = await this.store.getUser(exchange.user_id);
    if (!user) {
      throw ERR.authInvalid();
    }
    const consumed = await this.store.consumeAuthExchangeCode(exchange.exchange_request_id, exchange.code_hash, exchange.user_id, nowIso());
    if (!consumed) {
      throw ERR.authInvalid();
    }
    return this.issueJoinCredential(user);
  }

  private async issueJoinCredential(user: UserRecord): Promise<unknown> {
    const agent = await this.ensureDefaultAgent(user);
    const hubToken = await this.issueAgentToken(agent, DEFAULT_PERMISSIONS.slice());
    return {
      agent_id: agent.agent_id,
      handle: agent.handle,
      namespace: agent.namespace,
      hub_token: hubToken,
      expires_at: null,
      profile: {
        display_name: agent.display_name,
        description: agent.description.length === 0 ? null : agent.description,
        capabilities: parseJsonArray(agent.capabilities_json),
      },
      visibility: {
        discoverable: agent.discoverable,
        inbox: agent.inbox,
      },
    };
  }

  private async registerUser(args: Record<string, unknown>): Promise<unknown> {
    ensureOnly(args, ["username", "password", "namespace"]);
    const user = await this.createUserFromCredentials(
      stringField(args, "username"),
      stringField(args, "password"),
      optionalString(args.namespace, "namespace"),
    );
    return {
      user_id: user.user_id,
      username: user.username,
      namespace: user.namespace,
      session_token: await this.issueHumanSession(user),
    };
  }

  private async loginUser(args: Record<string, unknown>): Promise<unknown> {
    ensureOnly(args, ["username", "password", "namespace"]);
    const user = await this.requireUserPassword(
      stringField(args, "username"),
      stringField(args, "password"),
      optionalString(args.namespace, "namespace"),
    );
    return {
      user_id: user.user_id,
      username: user.username,
      namespace: user.namespace,
      session_token: await this.issueHumanSession(user),
    };
  }

  private async createUserFromCredentials(usernameRaw: string, password: string, namespaceRaw: string | null): Promise<UserRecord> {
    let phase = "validate";
    try {
      const username = normalizeName(usernameRaw, "username");
      const namespace = normalizeName(namespaceRaw ?? username, "namespace");
      if (password.length < 12) {
        throw ERR.schemaInvalid(["password must be at least 12 characters"]);
      }
      phase = "lookup_username";
      if (await this.store.getUserByIdentity(username, namespace)) {
        throw ERR.schemaInvalid(["username already exists in namespace"]);
      }
      phase = "hash_password";
      const salt = randomSecret(18);
      const passwordHash = await pbkdf2Hash(password, salt);
      const user: UserRecord = {
        user_id: crypto.randomUUID(),
        username,
        namespace,
        password_hash: passwordHash,
        password_salt: salt,
        created_at: nowIso(),
      };
      phase = "insert_user";
      await this.store.createUser(user);
      return user;
    } catch (error) {
      if (error instanceof PublicError) throw error;
      throw new PublicError(-32603, "internal", "Hub is temporarily unavailable. Retry with backoff.", { phase });
    }
  }

  private async requireUserPassword(usernameRaw: string, password: string, namespaceRaw: string | null): Promise<UserRecord> {
    const username = normalizeName(usernameRaw, "username");
    const namespace = namespaceRaw ? normalizeName(namespaceRaw, "namespace") : null;
    let user: UserRecord | null;
    if (namespace) {
      user = await this.store.getUserByIdentity(username, namespace);
    } else {
      const users = await this.store.getUsersByUsername(username);
      if (users.length > 1) {
        throw ERR.schemaInvalid(["namespace is required for this username"]);
      }
      user = users[0] ?? null;
    }
    if (!user) {
      throw ERR.authInvalid();
    }
    const expected = await pbkdf2Hash(password, user.password_salt);
    if (!timingSafeEqual(expected, user.password_hash)) {
      throw ERR.authInvalid();
    }
    return user;
  }

  private async requireAuthExchange(exchangeRequestId: string, stateRaw: string): Promise<AuthExchangeRecord> {
    const state = validateOpaqueValue(stateRaw, "state");
    const exchange = await this.store.getAuthExchange(validateUuid(exchangeRequestId, "exchange_request_id"));
    if (!exchange || exchange.used_at || exchange.expires_at <= nowIso()) {
      throw ERR.authInvalid();
    }
    if ((await sha256Hex(state)) !== exchange.state_hash) {
      throw ERR.authInvalid();
    }
    return exchange;
  }

  private async issueHumanSession(user: UserRecord): Promise<string> {
    const sessionId = crypto.randomUUID();
    const token = `hub_hs_${sessionId}_${randomSecret(32)}`;
    const createdAt = nowIso();
    await this.store.createHumanSession({
      session_id: sessionId,
      session_hash: await sha256Hex(token),
      user_id: user.user_id,
      namespace: user.namespace,
      created_at: createdAt,
      expires_at: addDaysIso(30),
      revoked_at: null,
    });
    return token;
  }

  private async ensureDefaultAgent(user: UserRecord): Promise<AgentRecord> {
    const handle = normalizeName(user.username, "handle");
    const existing = await this.store.getAgentByHandle(user.namespace, handle);
    if (existing) {
      if (existing.deleted_at) {
        const restored = { ...existing, deleted_at: null, last_seen_at: nowIso() };
        await this.store.updateAgent(restored);
        return restored;
      }
      await this.store.touchAgent(existing.agent_id, nowIso());
      return { ...existing, last_seen_at: nowIso() };
    }
    const createdAt = nowIso();
    const agent: AgentRecord = {
      agent_id: crypto.randomUUID(),
      user_id: user.user_id,
      namespace: user.namespace,
      handle,
      display_name: user.username,
      description: "",
      capabilities_json: JSON.stringify([]),
      discoverable: "public",
      inbox: "namespace",
      created_at: createdAt,
      last_seen_at: null,
      deleted_at: null,
    };
    await this.store.createAgent(agent);
    return agent;
  }

  private async registerAgent(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertHuman(principal);
    ensureOnly(args, ["handle", "display_name", "description", "capabilities", "discoverable", "inbox"]);
    const createdAt = nowIso();
    const capabilities = validateCapabilities(arrayField(args, "capabilities"));
    const agent: AgentRecord = {
      agent_id: crypto.randomUUID(),
      user_id: principal.user_id,
      namespace: principal.namespace,
      handle: normalizeName(stringField(args, "handle"), "handle"),
      display_name: validatePlainText(stringField(args, "display_name"), "display_name", 48),
      description: validatePlainText(stringField(args, "description"), "description", 200),
      capabilities_json: JSON.stringify(capabilities),
      discoverable: discoverableValue(optionalString(args.discoverable, "discoverable") ?? "public"),
      inbox: inboxValue(optionalString(args.inbox, "inbox") ?? "namespace"),
      created_at: createdAt,
      last_seen_at: null,
      deleted_at: null,
    };
    await this.store.createAgent(agent);
    const hubToken = await this.issueAgentToken(agent, DEFAULT_PERMISSIONS.slice());
    return {
      agent: detailProfile(agent),
      hub_token: hubToken,
      token_note: "Store this token locally; the hub stores only a hash and will not show it again.",
    };
  }

  private async issueAgentToken(agent: AgentRecord, permissions: readonly string[]): Promise<string> {
    const tokenId = crypto.randomUUID();
    const token = `hub_agent_${tokenId}_${randomSecret(32)}`;
    await this.store.createAgentToken({
      token_id: tokenId,
      token_hash: await sha256Hex(token),
      agent_id: agent.agent_id,
      user_id: agent.user_id,
      namespace: agent.namespace,
      permissions_json: JSON.stringify(permissions),
      created_at: nowIso(),
      last_used_at: null,
      expires_at: null,
      revoked_at: null,
    });
    return token;
  }

  private async updateAgentProfile(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "agent:update_self_profile");
    ensureOnly(args, ["handle", "display_name", "description", "capabilities", "discoverable", "inbox"]);
    const agent = await this.requireSelfAgent(principal);
    const updated: AgentRecord = {
      ...agent,
      handle: args.handle === undefined ? agent.handle : normalizeName(stringField(args, "handle"), "handle"),
      display_name:
        args.display_name === undefined ? agent.display_name : validatePlainText(stringField(args, "display_name"), "display_name", 48),
      description:
        args.description === undefined ? agent.description : validatePlainText(stringField(args, "description"), "description", 200),
      capabilities_json:
        args.capabilities === undefined ? agent.capabilities_json : JSON.stringify(validateCapabilities(arrayField(args, "capabilities"))),
      discoverable:
        args.discoverable === undefined ? agent.discoverable : discoverableValue(stringField(args, "discoverable")),
      inbox: args.inbox === undefined ? agent.inbox : inboxValue(stringField(args, "inbox")),
      last_seen_at: nowIso(),
    };
    await this.store.updateAgent(updated);
    return { agent: detailProfile(updated) };
  }

  private async rotateAgentToken(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "token:rotate_self");
    ensureOnly(args, []);
    await this.store.revokeAgentToken(principal.token_id, nowIso());
    const agent = await this.requireSelfAgent(principal);
    return { hub_token: await this.issueAgentToken(agent, DEFAULT_PERMISSIONS.slice()) };
  }

  private async revokeAgentToken(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "token:rotate_self");
    ensureOnly(args, []);
    await this.store.revokeAgentToken(principal.token_id, nowIso());
    await this.mailbox.push(principal.agent_id, revokedNotification(principal.agent_id));
    return { revoked: true };
  }

  private async deleteAgent(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "agent:delete_self");
    ensureOnly(args, []);
    const agent = await this.requireSelfAgent(principal);
    await this.store.updateAgent({ ...agent, deleted_at: nowIso() });
    return { deleted: true };
  }

  private async listMyAgents(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    ensureOnly(args, ["cursor", "limit"]);
    if (principal.kind === "agent") {
      requirePermission(principal, "agent:list_namespace");
    }
    const limit = limitValue(args.limit);
    const agents = await this.store.listAgentsByNamespace(principal.namespace, limit + 1, optionalString(args.cursor, "cursor"));
    return pageResult(agents, limit, detailProfile);
  }

  private async discoverPublicAgents(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "agent:discover_public");
    ensureOnly(args, ["cursor", "limit"]);
    const limit = limitValue(args.limit);
    const agents = await this.store.listPublicAgents(limit + 1, optionalString(args.cursor, "cursor"));
    return pageResult(agents, limit, listProfile);
  }

  private async getAgentProfile(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    ensureOnly(args, ["agent_id", "agent_handle"]);
    const agent = await this.resolveAgent(args);
    if (!agent || agent.deleted_at) {
      throw ERR.notFound();
    }
    if (agent.agent_id !== principal.agent_id) {
      requirePermission(principal, "agent:discover_public");
      if (agent.discoverable !== "public" && agent.namespace !== principal.namespace) {
        throw ERR.notFound();
      }
    } else {
      requirePermission(principal, "agent:read_self");
    }
    return { agent: detailProfile(agent) };
  }

  private async sendNotification(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "notification:send");
    const sender = await this.requireSelfAgent(principal);
    return this.sendNotificationAsAgent(sender, args);
  }

  private async sendNotificationAsAgent(sender: AgentRecord, args: Record<string, unknown>): Promise<unknown> {
    ensureOnly(args, ["target_agent_id", "summary", "payload", "payload_visibility"]);
    const targetId = uuidField(args, "target_agent_id");
    const summary = validateSummary(stringField(args, "summary"));
    const payload = args.payload === undefined ? null : args.payload;
    const payloadJson = payload === null ? null : JSON.stringify(payload);
    if (payloadJson && byteLength(payloadJson) > PAYLOAD_LIMIT_BYTES) {
      throw ERR.bodyTooLarge(PAYLOAD_LIMIT_BYTES);
    }
    const payloadVisibility = payloadVisibilityValue(optionalString(args.payload_visibility, "payload_visibility") ?? "Local");
    const persistedPayloadJson = payloadVisibility === "Shared" ? payloadJson : null;
    const receiver = await this.store.getAgent(targetId);
    if (!receiver || receiver.deleted_at) {
      throw ERR.notFound();
    }
    if (receiver.inbox === "closed") {
      throw ERR.permissionDenied();
    }
    const blocked = await this.store.getBlock(receiver.agent_id, sender.agent_id);
    if (blocked) {
      return { status: "accepted", delivery: "not_disclosed" };
    }
    const sameNamespace = receiver.namespace === sender.namespace;
    const trust = await this.store.getTrust(receiver.agent_id, sender.agent_id);
    const hasTrust = trust && (!trust.expires_at || trust.expires_at > nowIso());
    if (!sameNamespace && receiver.inbox === "namespace" && !hasTrust) {
      throw ERR.permissionDenied();
    }
    const firstContactRequired = !sameNamespace && !hasTrust && (receiver.inbox === "open" || receiver.inbox === "invited");
    const now = nowIso();
    const notification: NotificationRecord = {
      notification_id: crypto.randomUUID(),
      receiver_agent_id: receiver.agent_id,
      sender_agent_id: sender.agent_id,
      sender_handle: sender.handle,
      sender_namespace: sender.namespace,
      summary,
      payload_json: persistedPayloadJson,
      payload_visibility: payloadVisibility,
      status: "pending",
      first_contact_required: firstContactRequired ? 1 : 0,
      created_at: now,
      delivered_at: null,
      acked_at: null,
    };
    await this.store.createNotification(notification);
    const delivered = await this.mailbox.push(receiver.agent_id, notification);
    if (delivered) {
      await this.store.markNotificationDelivered(notification.notification_id, nowIso());
    }
    return {
      notification_id: notification.notification_id,
      status: delivered ? "delivered" : "queued",
      first_contact_required: firstContactRequired,
    };
  }

  private async listMyInbox(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "notification:receive");
    ensureOnly(args, ["cursor", "limit"]);
    const limit = limitValue(args.limit);
    const notifications = await this.store.listNotifications(principal.agent_id, limit + 1, optionalString(args.cursor, "cursor"));
    return pageResult(notifications, limit, inboxItem);
  }

  private async ackNotification(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "notification:receive");
    ensureOnly(args, ["notification_ids"]);
    const ids = arrayField(args, "notification_ids").map((id, index) => {
      if (typeof id !== "string") {
        throw ERR.schemaInvalid([`notification_ids[${index}] must be a string`]);
      }
      return id;
    });
    return { acked_notification_ids: await this.store.ackNotifications(principal.agent_id, ids, nowIso()) };
  }

  private async listTrust(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "trust:list");
    ensureOnly(args, []);
    const entries = await this.store.listTrust(principal.agent_id);
    return {
      entries: entries.map((entry) => ({
        sender_agent_id: entry.sender_agent_id,
        action_class: entry.action_class,
        granted_at: entry.granted_at,
        expires_at: entry.expires_at,
      })),
    };
  }

  private async revokeTrust(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "trust:revoke");
    ensureOnly(args, ["sender_agent_id", "action_class"]);
    const senderAgentId = uuidField(args, "sender_agent_id");
    const actionClass = optionalString(args.action_class, "action_class") ?? "notification";
    if (actionClass !== "notification") {
      throw ERR.schemaInvalid(["action_class must be notification"]);
    }
    await this.store.revokeTrust(principal.agent_id, senderAgentId, actionClass);
    return { revoked: true };
  }

  private async blockSender(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "trust:block");
    ensureOnly(args, ["sender_agent_id"]);
    const senderAgentId = uuidField(args, "sender_agent_id");
    await this.store.blockSender({
      receiver_agent_id: principal.agent_id,
      sender_agent_id: senderAgentId,
      blocked_at: nowIso(),
    });
    return { blocked: true };
  }

  private async unblockSender(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    requirePermission(principal, "trust:unblock");
    ensureOnly(args, ["sender_agent_id"]);
    await this.store.unblockSender(principal.agent_id, uuidField(args, "sender_agent_id"));
    return { unblocked: true };
  }

  private async readResource(principal: Principal, args: Record<string, unknown>): Promise<unknown> {
    assertAgent(principal);
    const uri = stringField(args, "uri");
    if (uri.startsWith("agent://")) {
      const agentId = uri.slice("agent://".length);
      return { contents: [{ uri, mimeType: "application/json", text: JSON.stringify(await this.getAgentProfile(principal, { agent_id: agentId })) }] };
    }
    if (uri.startsWith("inbox://")) {
      const agentId = uri.slice("inbox://".length);
      if (agentId !== principal.agent_id) {
        throw ERR.permissionDenied();
      }
      return { contents: [{ uri, mimeType: "application/json", text: JSON.stringify(await this.listMyInbox(principal, {})) }] };
    }
    if (uri.startsWith("trust://")) {
      const agentId = uri.slice("trust://".length);
      if (agentId !== principal.agent_id) {
        throw ERR.permissionDenied();
      }
      return { contents: [{ uri, mimeType: "application/json", text: JSON.stringify(await this.listTrust(principal, {})) }] };
    }
    throw ERR.notFound("Resource is not reachable.");
  }

  private async resolveAgent(args: Record<string, unknown>): Promise<AgentRecord | null> {
    if (args.agent_id !== undefined) {
      return this.store.getAgent(uuidField(args, "agent_id"));
    }
    if (args.agent_handle !== undefined) {
      const parsed = parseAgentHandle(stringField(args, "agent_handle"));
      return this.store.getAgentByHandle(parsed.namespace, parsed.handle);
    }
    throw ERR.schemaInvalid(["agent_id or agent_handle is required"]);
  }

  private async requireSelfAgent(principal: Principal): Promise<AgentRecord> {
    assertAgent(principal);
    const agent = await this.store.getAgent(principal.agent_id);
    if (!agent || agent.deleted_at) {
      throw ERR.notFound();
    }
    return agent;
  }
}

export function createApp(env: Env): HubApp {
  const store = env.DB ? new D1Store(env.DB) : new MemoryStore();
  const mailbox = env.MAILBOX ? new DurableMailbox(env.MAILBOX) : new MemoryMailbox();
  return new HubApp(store, mailbox, env.HUB_VERSION ?? "0.1.0");
}

export function createTestApp(store = new MemoryStore(), mailbox = new MemoryMailbox()): HubApp {
  return new HubApp(store, mailbox, "test");
}

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return createApp(env).fetch(request);
  },
};

function revokedNotification(agentId: string): NotificationRecord {
  const at = nowIso();
  return {
    notification_id: crypto.randomUUID(),
    receiver_agent_id: agentId,
    sender_agent_id: agentId,
    sender_handle: "pie-hub",
    sender_namespace: "system",
    summary: "Agent token revoked.",
    payload_json: JSON.stringify({ revoked_at: at, reason: "revoked" }),
    payload_visibility: "Local",
    status: "delivered",
    first_contact_required: 0,
    created_at: at,
    delivered_at: at,
    acked_at: null,
  };
}

function toSseEvent(notification: NotificationRecord): string {
  const data = {
    jsonrpc: "2.0",
    method: notification.sender_namespace === "system" ? "notifications/agent_revoked" : "notifications/agent_message",
    params:
      notification.sender_namespace === "system"
        ? {
            agent_id: notification.receiver_agent_id,
            revoked_at: notification.created_at,
            reason: "revoked",
            _meta: {
              pie_dedup_key: notification.notification_id,
              pie_summary: notification.summary,
            },
          }
        : {
            notification_id: notification.notification_id,
            agent_id: notification.sender_agent_id,
            handle: notification.sender_handle,
            namespace: notification.sender_namespace,
            sender: `@${notification.sender_handle}@${notification.sender_namespace}`,
            payload_visibility: notification.payload_visibility,
            first_contact_required: notification.first_contact_required === 1,
            payload: notification.payload_visibility === "Shared" ? safeJsonParse(notification.payload_json) : undefined,
            _meta: {
              pie_dedup_key: notification.notification_id,
              pie_summary: notification.summary,
              receiver_agent_id: notification.receiver_agent_id,
              sender_agent_id: notification.sender_agent_id,
              action_class: "notification",
            },
          },
  };
  return `id: ${notification.notification_id}\nevent: message\ndata: ${JSON.stringify(data)}\n\n`;
}

const TOOL_DEFINITIONS = [
  tool("register_agent", "Register a new agent in the caller's namespace. Returns the hub token once.", {
    handle: "Namespace-local handle, lowercase letters, digits, '_' or '-'.",
    display_name: "Human-readable display name, no markdown or URLs.",
    description: "Plain text sender profile description, no markdown or URLs.",
    capabilities: "Short lowercase capability names.",
    discoverable: "Visibility for discovery: public, namespace, or none.",
    inbox: "Notification inbox policy: open, namespace, invited, or closed.",
  }),
  tool("update_agent_profile", "Update this agent's profile and visibility settings.", {
    handle: "Optional replacement handle.",
    display_name: "Optional replacement display name.",
    description: "Optional replacement profile description.",
    capabilities: "Optional replacement capabilities list.",
    discoverable: "Optional discovery setting.",
    inbox: "Optional inbox setting.",
  }),
  tool("rotate_agent_token", "Rotate this agent token and return the new token once.", {}),
  tool("revoke_agent_token", "Revoke the current agent token.", {}),
  tool("delete_agent", "Soft-delete this agent.", {}),
  tool("list_my_agents", "List agents in the caller's namespace.", {
    cursor: "Opaque cursor returned by the previous page.",
    limit: "Maximum rows to return.",
  }),
  tool("discover_public_agents", "Discover public cross-namespace agents.", {
    cursor: "Opaque cursor returned by the previous page.",
    limit: "Maximum rows to return.",
  }),
  tool("get_agent_profile", "Fetch an agent profile by UUID or handle.", {
    agent_id: "Hub-issued immutable UUID.",
    agent_handle: "Display handle in @handle@namespace form.",
  }),
  tool("send_notification", "Send a bounded notification to a target agent UUID.", {
    target_agent_id: "Receiver agent UUID.",
    summary: "User-visible bounded summary, at most 240 characters.",
    payload: "Optional JSON payload, capped at 8 KiB.",
    payload_visibility: "Local by default; Shared allows payload to reach the receiver wire frame.",
  }),
  tool("list_my_inbox", "List pending or delivered notifications for this agent.", {
    cursor: "Opaque cursor returned by the previous page.",
    limit: "Maximum rows to return.",
  }),
  tool("ack_notification", "Acknowledge delivered notifications.", {
    notification_ids: "Notification ids to acknowledge.",
  }),
  tool("list_trust", "List this receiver's trust grants.", {}),
  tool("revoke_trust", "Revoke a notification trust grant.", {
    sender_agent_id: "Sender agent UUID.",
    action_class: "Must be notification in v0.",
  }),
  tool("block_sender", "Block a sender agent UUID.", {
    sender_agent_id: "Sender agent UUID.",
  }),
  tool("unblock_sender", "Unblock a sender agent UUID.", {
    sender_agent_id: "Sender agent UUID.",
  }),
];

function tool(name: string, description: string, properties: Record<string, string>): Record<string, unknown> {
  const schemaProperties: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(properties)) {
    schemaProperties[key] = { type: ["string", "number", "array", "object", "boolean", "null"], description: value };
  }
  return {
    name,
    description,
    inputSchema: {
      type: "object",
      additionalProperties: false,
      properties: schemaProperties,
    },
  };
}

function requireJsonRpc(payload: Record<string, unknown>): void {
  if (payload.jsonrpc !== "2.0") {
    throw ERR.schemaInvalid(["jsonrpc must be 2.0"]);
  }
  stringField(payload, "method");
}

function jsonRpcId(value: unknown): string | number | null {
  if (value === undefined || value === null) return null;
  if (typeof value === "string" || typeof value === "number") return value;
  throw ERR.schemaInvalid(["id must be string, number, or null"]);
}

function jsonRpcError(id: string | number | null, error: PublicError): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    id,
    error: {
      code: error.code,
      message: error.message,
      data: {
        name: error.name,
        ...(error.data ?? {}),
      },
    },
  };
}

function coercePublicError(error: unknown): PublicError {
  if (error instanceof PublicError) return error;
  return new PublicError(-32603, "internal", "Hub is temporarily unavailable. Retry with backoff.");
}

function browserLoginErrorMessage(error: PublicError, args: Record<string, unknown>): string {
  if (error.name === "auth_invalid" && typeof args.password === "string") {
    return "Invalid username or password.";
  }
  const violations = error.data?.violations;
  if (Array.isArray(violations)) {
    const safeViolations = violations
      .filter((violation): violation is string => typeof violation === "string")
      .map((violation) => truncate(violation, 120))
      .slice(0, 3);
    if (safeViolations.length > 0) {
      return safeViolations.join("; ");
    }
  }
  return truncate(error.message, 160);
}

function chatErrorMessage(error: PublicError, args: Record<string, unknown>): string {
  if (error.name === "auth_invalid" && typeof args.password === "string") {
    return "Invalid username or password.";
  }
  if (error.name === "permission_denied") {
    if (error.message.startsWith("Web chat form submission")) {
      return error.message;
    }
    return "Could not send message: the recipient is not accepting hub messages from this sender.";
  }
  if (error.name === "not_found") {
    return truncate(error.message, 160);
  }
  const violations = error.data?.violations;
  if (Array.isArray(violations)) {
    const safeViolations = violations
      .filter((violation): violation is string => typeof violation === "string")
      .map((violation) => truncate(violation, 120))
      .slice(0, 3);
    if (safeViolations.length > 0) {
      return safeViolations.join("; ");
    }
  }
  return truncate(error.message, 160);
}

function httpError(error: unknown): Response {
  const publicError = coercePublicError(error);
  const status = publicError.code === -32009 || publicError.code === -32010 ? 401 : 400;
  return json({ error: publicError.name, message: publicError.message, data: publicError.data ?? {} }, status);
}

function assertHuman(principal: Principal): asserts principal is Extract<Principal, { kind: "human" }> {
  if (principal.kind !== "human") {
    throw ERR.authInvalid();
  }
}

function assertAgent(principal: Principal): asserts principal is Extract<Principal, { kind: "agent" }> {
  if (principal.kind !== "agent") {
    throw ERR.authInvalid();
  }
}

function requirePermission(principal: Extract<Principal, { kind: "agent" }>, permission: string): void {
  if (!principal.permissions.includes(permission)) {
    throw ERR.permissionDenied("The authenticated agent token is missing the required permission.");
  }
}

async function readJsonObject(request: Request): Promise<Record<string, unknown>> {
  const text = await request.text();
  if (byteLength(text) > JSON_LIMIT_BYTES) {
    throw ERR.bodyTooLarge(JSON_LIMIT_BYTES);
  }
  const parsed = JSON.parse(text) as unknown;
  if (!isObject(parsed) || Array.isArray(parsed)) {
    throw ERR.schemaInvalid(["body must be a JSON object"]);
  }
  return parsed;
}

async function readFormObject(request: Request): Promise<Record<string, unknown>> {
  const text = await request.text();
  if (byteLength(text) > JSON_LIMIT_BYTES) {
    throw ERR.bodyTooLarge(JSON_LIMIT_BYTES);
  }
  const params = new URLSearchParams(text);
  const out: Record<string, string> = {};
  params.forEach((value, key) => {
    out[key] = value;
  });
  return out;
}

function stringField(obj: Record<string, unknown>, field: string): string {
  const value = obj[field];
  if (typeof value !== "string" || value.length === 0) {
    throw ERR.schemaInvalid([`${field} must be a non-empty string`]);
  }
  return value;
}

function optionalString(value: unknown, field: string): string | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || value.length === 0) {
    throw ERR.schemaInvalid([`${field} must be a non-empty string when provided`]);
  }
  return value;
}

function optionalDelivery(value: unknown): "loopback" | "manual" {
  if (value === undefined || value === null || value === "loopback") return "loopback";
  if (value === "manual") return "manual";
  throw ERR.schemaInvalid(["delivery must be loopback or manual"]);
}

function optionalObject(value: unknown, field: string): Record<string, unknown> {
  if (value === undefined || value === null) return {};
  if (!isObject(value) || Array.isArray(value)) {
    throw ERR.schemaInvalid([`${field} must be an object`]);
  }
  return value;
}

function arrayField(obj: Record<string, unknown>, field: string): unknown[] {
  const value = obj[field];
  if (!Array.isArray(value)) {
    throw ERR.schemaInvalid([`${field} must be an array`]);
  }
  return value;
}

function uuidField(obj: Record<string, unknown>, field: string): string {
  const value = stringField(obj, field);
  return validateUuid(value, field);
}

function validateUuid(value: string, field: string): string {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value)) {
    throw ERR.schemaInvalid([`${field} must be a UUID`]);
  }
  return value;
}

function ensureOnly(obj: Record<string, unknown>, allowed: string[]): void {
  const allowedSet = new Set(allowed);
  const extra = Object.keys(obj).filter((key) => !allowedSet.has(key));
  if (extra.length > 0) {
    throw ERR.schemaInvalid(extra.map((key) => `${key} is not allowed`));
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function d1ChangedRows(result: unknown): number {
  if (!isObject(result) || !isObject(result.meta)) {
    return 0;
  }
  return typeof result.meta.changes === "number" ? result.meta.changes : 0;
}

function normalizeName(value: string, field: string): string {
  const normalized = value.trim().toLowerCase();
  if (!/^[a-z0-9_-]{2,32}$/.test(normalized)) {
    throw ERR.schemaInvalid([`${field} must match [a-z0-9_-]{2,32}`]);
  }
  return normalized;
}

function validatePlainText(value: string, field: string, maxChars: number): string {
  const trimmed = value.trim();
  if (trimmed.length > maxChars) {
    throw ERR.schemaInvalid([`${field} must be at most ${maxChars} characters`]);
  }
  if (/https?:\/\/|\[[^\]]+\]\([^)]+\)/i.test(trimmed)) {
    throw ERR.schemaInvalid([`${field} must be plain text without URLs or markdown links`]);
  }
  return trimmed;
}

function validateCapabilities(values: unknown[]): string[] {
  if (values.length > 8) {
    throw ERR.schemaInvalid(["capabilities must contain at most 8 entries"]);
  }
  return values.map((value, index) => {
    if (typeof value !== "string" || !/^[a-z0-9-]{1,32}$/.test(value)) {
      throw ERR.schemaInvalid([`capabilities[${index}] must be lowercase kebab-case`]);
    }
    return value;
  });
}

function validateSummary(value: string): string {
  const trimmed = value.trim();
  if (trimmed.length === 0 || trimmed.length > SUMMARY_LIMIT_CHARS) {
    throw ERR.schemaInvalid([`summary must be 1-${SUMMARY_LIMIT_CHARS} characters`]);
  }
  return trimmed;
}

function validateOpaqueValue(value: string, field: string): string {
  if (!/^[A-Za-z0-9._~-]{8,256}$/.test(value)) {
    throw ERR.schemaInvalid([`${field} must be an opaque URL-safe value`]);
  }
  return value;
}

function validateLoopbackAuthCode(value: string): string {
  const code = validateOpaqueValue(value, "code");
  if (!code.startsWith("hub_code_")) {
    throw ERR.schemaInvalid(["code must be a browser loopback auth code"]);
  }
  return code;
}

function validateManualAuthCode(value: string): string {
  const compact = value.trim().toUpperCase().replace(/[^A-Z0-9]/g, "");
  if (!/^[A-Z2-9]{8}$/.test(compact)) {
    throw ERR.schemaInvalid(["manual_code must be an 8-character one-time code"]);
  }
  return `${compact.slice(0, 4)}-${compact.slice(4)}`;
}

function validatePkceValue(value: string, field: string): string {
  if (!/^[A-Za-z0-9._~-]{43,128}$/.test(value)) {
    throw ERR.schemaInvalid([`${field} must be a PKCE base64url value`]);
  }
  return value;
}

function validateLoopbackRedirectUri(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw ERR.schemaInvalid(["loopback_redirect_uri must be a URL"]);
  }
  if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || url.pathname !== "/callback") {
    throw ERR.schemaInvalid(["loopback_redirect_uri must be http://127.0.0.1:<port>/callback"]);
  }
  const port = Number(url.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535 || url.search || url.hash) {
    throw ERR.schemaInvalid(["loopback_redirect_uri must include only a loopback port and /callback path"]);
  }
  return url.toString();
}

function discoverableValue(value: string): Discoverable {
  if (value === "public" || value === "namespace" || value === "none") return value;
  throw ERR.schemaInvalid(["discoverable must be public, namespace, or none"]);
}

function inboxValue(value: string): Inbox {
  if (value === "open" || value === "namespace" || value === "invited" || value === "closed") return value;
  throw ERR.schemaInvalid(["inbox must be open, namespace, invited, or closed"]);
}

function payloadVisibilityValue(value: string): PayloadVisibility {
  if (value === "Local" || value === "Shared" || value === "Redacted") return value;
  throw ERR.schemaInvalid(["payload_visibility must be Local, Shared, or Redacted"]);
}

function limitValue(value: unknown): number {
  if (value === undefined || value === null) return LIST_DEFAULT_LIMIT;
  if (typeof value !== "number" || !Number.isInteger(value) || value < 1 || value > LIST_MAX_LIMIT) {
    throw ERR.schemaInvalid([`limit must be an integer between 1 and ${LIST_MAX_LIMIT}`]);
  }
  return value;
}

function parseAgentHandle(value: string): { handle: string; namespace: string } {
  const match = /^@([a-z0-9_-]{2,32})@([a-z0-9_-]{2,32})$/.exec(value);
  if (!match) {
    throw ERR.schemaInvalid(["agent_handle must be @handle@namespace"]);
  }
  return { handle: match[1], namespace: match[2] };
}

function parseDisplayAgentHandle(value: string, field: string): { handle: string; namespace: string } {
  const match = /^@?([a-z0-9_-]{2,32})@([a-z0-9_-]{2,32})$/.exec(value.trim().toLowerCase());
  if (!match) {
    throw ERR.schemaInvalid([`${field} must be name@namespace`]);
  }
  return { handle: match[1], namespace: match[2] };
}

function safeDisplayMention(identity: { handle: string; namespace: string }): string {
  return `${safeDisplayMentionPart(identity.handle, "redacted-agent")}@${safeDisplayMentionPart(identity.namespace, "redacted-namespace")}`;
}

function safeDisplayMentionPart(value: string, fallback: string): string {
  return isSecretLikeDisplayPart(value) ? fallback : value;
}

function isSecretLikeDisplayPart(value: string): boolean {
  const lower = value.toLowerCase();
  return (
    lower.startsWith("hub_agent") ||
    lower.startsWith("hub_hs") ||
    lower.startsWith("hub_code") ||
    lower.startsWith("sk-") ||
    lower.startsWith("gho_") ||
    lower.startsWith("xoxb-") ||
    lower === "authorization" ||
    lower === "bearer"
  );
}

function parseJsonArray(jsonText: string): string[] {
  const value = JSON.parse(jsonText) as unknown;
  if (!Array.isArray(value)) return [];
  return value.filter((item): item is string => typeof item === "string");
}

function listProfile(agent: AgentRecord): Record<string, unknown> {
  return {
    agent_id: agent.agent_id,
    handle: agent.handle,
    namespace: agent.namespace,
    display_name: agent.display_name,
    capabilities: parseJsonArray(agent.capabilities_json),
    discoverable: agent.discoverable,
    inbox: agent.inbox,
  };
}

function detailProfile(agent: AgentRecord): Record<string, unknown> {
  return {
    ...listProfile(agent),
    description: agent.description,
    created_at: agent.created_at,
    last_seen_at: agent.last_seen_at,
    deleted_at: agent.deleted_at,
  };
}

function inboxItem(notification: NotificationRecord): Record<string, unknown> {
  return {
    notification_id: notification.notification_id,
    sender_agent_id: notification.sender_agent_id,
    sender: `@${notification.sender_handle}@${notification.sender_namespace}`,
    summary: notification.summary,
    payload_visibility: notification.payload_visibility,
    first_contact_required: notification.first_contact_required === 1,
    status: notification.status,
    created_at: notification.created_at,
    delivered_at: notification.delivered_at,
  };
}

function pageResult<T>(items: T[], limit: number, map: (item: T) => Record<string, unknown>): Record<string, unknown> {
  const page = items.slice(0, limit);
  return {
    items: page.map(map),
    next_cursor: items.length > limit ? cursorFor(items[limit - 1]) : null,
  };
}

function cursorFor(item: unknown): string | null {
  if (isObject(item)) {
    if (typeof item.agent_id === "string") return item.agent_id;
    if (typeof item.notification_id === "string") return item.notification_id;
  }
  return null;
}

function pageByAgentId(items: AgentRecord[], limit: number, cursor: string | null): AgentRecord[] {
  return items
    .sort((a, b) => a.agent_id.localeCompare(b.agent_id))
    .filter((a) => !cursor || a.agent_id > cursor)
    .slice(0, limit);
}

function pageByNotificationId(items: NotificationRecord[], limit: number, cursor: string | null): NotificationRecord[] {
  return items
    .sort((a, b) => a.notification_id.localeCompare(b.notification_id))
    .filter((n) => !cursor || n.notification_id > cursor)
    .slice(0, limit);
}

function trustKey(receiverAgentId: string, senderAgentId: string, actionClass: string): string {
  return `${receiverAgentId}:${senderAgentId}:${actionClass}`;
}

function blockKey(receiverAgentId: string, senderAgentId: string): string {
  return `${receiverAgentId}:${senderAgentId}`;
}

async function sha256Hex(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function sha256Base64Url(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  const bytes = String.fromCharCode(...new Uint8Array(digest));
  return btoa(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
}

async function pbkdf2Hash(password: string, salt: string): Promise<string> {
  return `sha256:${await sha256Hex(`pie-fefe-password:${salt}:${password}`)}`;
}

function timingSafeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}

function randomSecret(bytes: number): string {
  const raw = new Uint8Array(bytes);
  crypto.getRandomValues(raw);
  return [...raw].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function manualAuthCode(): string {
  const alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
  const raw = new Uint8Array(MANUAL_AUTH_CODE_CHARS);
  crypto.getRandomValues(raw);
  const compact = [...raw].map((byte) => alphabet[byte % alphabet.length]).join("");
  return `${compact.slice(0, 4)}-${compact.slice(4)}`;
}

function nowIso(): string {
  return new Date().toISOString();
}

function addDaysIso(days: number): string {
  return new Date(Date.now() + days * 24 * 60 * 60 * 1000).toISOString();
}

function addSecondsIso(seconds: number): string {
  return new Date(Date.now() + seconds * 1000).toISOString();
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (char) => {
    switch (char) {
      case "&":
        return "&amp;";
      case "<":
        return "&lt;";
      case ">":
        return "&gt;";
      case '"':
        return "&quot;";
      default:
        return "&#39;";
    }
  });
}

function truncate(value: string, maxChars: number): string {
  return value.length <= maxChars ? value : `${value.slice(0, maxChars - 1)}…`;
}

function byteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function safeJsonParse(value: string | null): unknown {
  if (!value) return undefined;
  try {
    return JSON.parse(value) as unknown;
  } catch {
    return undefined;
  }
}

function cookieValue(request: Request, name: string): string | null {
  const cookie = request.headers.get("cookie");
  if (!cookie) return null;
  for (const part of cookie.split(";")) {
    const [rawKey, ...rawValue] = part.trim().split("=");
    if (rawKey === name) {
      return rawValue.join("=");
    }
  }
  return null;
}

function webSessionCookie(token: string): string {
  return `${WEB_SESSION_COOKIE}=${token}; HttpOnly; Secure; SameSite=Lax; Path=/chat; Max-Age=${WEB_SESSION_MAX_AGE_SECONDS}`;
}

function enforceSameOrigin(request: Request): void {
  const origin = request.headers.get("origin");
  if (!origin || origin !== new URL(request.url).origin) {
    throw ERR.permissionDenied("Web chat form submission must come from this hub page.");
  }
}

function html(body: string, status = 200): Response {
  return new Response(body, {
    status,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      "referrer-policy": "no-referrer",
    },
  });
}

function chatStyle(): string {
  return `<style>
  :root {
    color-scheme: light dark;
    --bg: #f6f7f9;
    --fg: #15181d;
    --muted: #5d6673;
    --line: #d9dee7;
    --panel: #ffffff;
    --accent: #1868d8;
    --accent-fg: #ffffff;
    --error-bg: #fff1f1;
    --error-fg: #9b1c1c;
    --success-bg: #eef9f1;
    --success-fg: #17692f;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    min-height: 100vh;
    background: var(--bg);
    color: var(--fg);
    font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    line-height: 1.45;
  }
  main {
    width: min(880px, 100%);
    max-width: 100%;
    margin: 0 auto;
    padding: 40px 20px;
    overflow-x: hidden;
  }
  header {
    width: 100%;
    max-width: 680px;
    margin-bottom: 24px;
  }
  h1 {
    margin: 0 0 8px;
    font-size: 28px;
    line-height: 1.15;
  }
  h2 {
    margin: 0 0 6px;
    font-size: 18px;
  }
  p {
    margin: 0;
    color: var(--muted);
  }
  h1, h2, p, label, .hint, .notice {
    overflow-wrap: anywhere;
  }
  .forms {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 16px;
    min-width: 0;
  }
  form {
    display: grid;
    gap: 14px;
    padding: 20px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--panel);
    min-width: 0;
  }
  .form-copy {
    min-height: 52px;
  }
  label {
    display: grid;
    gap: 6px;
    font-size: 14px;
    font-weight: 600;
  }
  input, textarea {
    width: 100%;
    min-width: 0;
    min-height: 42px;
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 9px 11px;
    background: transparent;
    color: var(--fg);
    font: inherit;
  }
  textarea {
    min-height: 116px;
    resize: vertical;
  }
  input:focus, textarea:focus {
    outline: 2px solid color-mix(in srgb, var(--accent) 35%, transparent);
    border-color: var(--accent);
  }
  .hint {
    color: var(--muted);
    font-size: 13px;
    font-weight: 400;
  }
  button {
    min-height: 42px;
    border: 0;
    border-radius: 6px;
    padding: 10px 14px;
    background: var(--accent);
    color: var(--accent-fg);
    font: inherit;
    font-weight: 650;
    cursor: pointer;
  }
  .notice {
    margin-bottom: 16px;
    padding: 12px 14px;
    border-radius: 8px;
    border: 1px solid var(--line);
    background: var(--panel);
  }
  .error {
    border-color: color-mix(in srgb, var(--error-fg) 35%, var(--line));
    background: var(--error-bg);
    color: var(--error-fg);
  }
  .success {
    border-color: color-mix(in srgb, var(--success-fg) 35%, var(--line));
    background: var(--success-bg);
    color: var(--success-fg);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #101215;
      --fg: #eff2f5;
      --muted: #a2aab5;
      --line: #303844;
      --panel: #181c22;
      --accent: #6da2ff;
      --accent-fg: #07111f;
      --error-bg: #2b1719;
      --error-fg: #ffb4b4;
      --success-bg: #12251a;
      --success-fg: #8de5a5;
    }
  }
  @media (max-width: 900px) {
    main {
      width: min(100%, 390px);
      max-width: min(100%, 390px);
      margin: 0;
      padding: 24px 14px;
    }
    .forms { grid-template-columns: 1fr; }
    .form-copy { min-height: 0; }
    form {
      width: 100%;
      max-width: 100%;
      padding: 16px;
    }
    input, textarea, button {
      max-width: 100%;
    }
  }
</style>`;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}

function sseHeaders(): ResponseInit {
  return {
    headers: {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-store",
      "x-accel-buffering": "no",
    },
  };
}
