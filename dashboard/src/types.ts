export type WireFormat = 'openai' | 'anthropic' | 'gemini' | 'plugin';

export interface VirtualKey {
  id: string;
  key: string;
  name: string;
  owner: string;
  tag: string;
  allowedModels: string[]; // ['*'] or list of model IDs / aliases / routes
  allowedProviders: string[]; // [] = no provider restriction (FR-12.19)
  rpmLimit: number;
  tpmLimit: number;
  maxConcurrentRequests: number | null;
  dailyBudget: number; // USD
  monthlyBudget: number; // USD
  currentDailySpend: number;
  currentMonthlySpend: number;
  createdAt: string;
  expiresAt: string | null;
  status: 'active' | 'disabled' | 'revoked';
  allowedIps?: string[];
  totalRequests: number;
  totalTokens: number;
}

export interface Provider {
  id: string;
  name: string;
  baseUrl: string;
  wireFormat: WireFormat;
  authScheme: 'bearer' | 'custom_header' | 'query_param';
  customHeaderName?: string;
  customParamName?: string;
  status: 'healthy' | 'degraded' | 'error';
  modelsCount: number;
  accountsCount: number;
  extraHeaders?: Record<string, string>;
  modelsPath?: string;
  timeoutMs: number;
  capabilityMode: 'permissive' | 'strict';
  followRedirects?: boolean;
  credentialHosts?: string;
  allowInsecureTls?: boolean;
  wirePlugin?: string;
  credentialPlugin?: string;
  modelSourcePlugin?: string;
  credentialMode: 'manual' | 'auth_flow' | 'none';
  sourcePluginId?: string;
  sourceIntegrationId?: string;
  credentialEnrollment: {
    mode: 'manual' | 'auth_flow' | 'none';
    actionLabel?: string | null;
    available: boolean;
  };
  /** Write-only: a credential supplied when adding/editing (never returned by the API). */
  apiKey?: string;
  accountLabel?: string;
  lastPingMs: number;
}

export interface Account {
  id: string;
  providerId: string;
  providerName: string;
  label: string;
  keyMasked: string;
  status: 'healthy' | 'cooldown' | 'exhausted' | 'disabled';
  cooldownUntil?: string | null;
  quotaResetTime?: string | null;
  quotaType: 'daily' | 'monthly' | 'rolling' | 'none';
  softQuotaSpendLimit?: number;
  currentSpend: number;
  requestsCount: number;
  tokensCount: number;
  priority: number;
  weight: number;
  lastError?: string;
}

export interface ModelCapability {
  text?: boolean;
  vision?: boolean;
  reasoning?: boolean;
  toolCalling?: boolean;
  audio?: boolean;
  structuredOutput?: boolean;
}

export interface ModelPrice {
  inputPer1M: number | null;
  outputPer1M: number | null;
  cachedPer1M: number | null;
  cacheWritePer1M: number | null;
  thinkingPer1M: number | null;
}

export interface ModelConfig {
  id: string;
  providerId: string;
  providerName: string;
  upstreamModelId: string;
  displayName: string;
  enabled: boolean;
  contextWindow: number | null;
  maxOutputTokens: number | null;
  capabilities: ModelCapability;
  prices: ModelPrice;
  parameters: {
    temperature?: { supported: boolean; min: number; max: number; default: number; policy: 'forward' | 'clamp' | 'reject' };
    top_p?: { supported: boolean; min: number; max: number; default: number; policy: 'forward' | 'clamp' | 'reject' };
    top_k?: { supported: boolean; min: number; max: number; default: number; policy: 'forward' | 'clamp' | 'reject' };
  };
  thinkingMap: {
    levels: Record<string, unknown>;
    mode?: 'manual_budget' | 'level' | 'adaptive';
    budgetField?: string;
    levelField?: string;
  };
  transportOverride?: string | null;
  discovery?: Record<string, unknown>;
}

export interface RouteTarget {
  id: string;
  accountId: string;
  accountLabel: string;
  providerName: string;
  modelId: string;
  modelDisplayName: string;
  priority: number;
  weight?: number;
}

export interface Route {
  id: string;
  name: string;
  description: string;
  selectionStrategy: 'priority' | 'round-robin' | 'weighted' | 'least-used' | 'adaptive';
  fallbackTriggers: {
    on429: boolean;
    onQuota: boolean;
    on5xx: boolean;
    onTimeout: boolean;
  };
  targets: RouteTarget[];
  portabilityPolicy: 'reject' | 'strip_with_warning';
  cacheAffinity: boolean;
  stickyRouting: boolean;
  maxConcurrentRequests: number | null;
  totalHops: number;
  status: 'active' | 'degraded' | 'all_exhausted';
}

export interface ModelAlias {
  id: string;
  aliasName: string;
  targetType: 'model' | 'route';
  targetId: string;
  targetDisplayName: string;
  description: string;
}

export interface RequestLog {
  id: string;
  requestId: string;
  timestamp: string;
  virtualKeyId: string;
  virtualKeyName: string;
  clientFormat: 'openai' | 'anthropic';
  requestedModel: string;
  effectiveTarget: string;
  routeName?: string;
  fallbackHops: number;
  fallbackPath: string[];
  status: 'success' | 'rate_limited' | 'quota_exhausted' | 'fallback_recovered' | 'client_error' | 'upstream_error';
  statusCode: number;
  latencyMs: number;
  ttftMs: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  cacheWriteTokens: number;
  thinkingTokens: number;
  costUsd: number;
  cacheStatus: 'hit' | 'miss' | 'bypass';
  servingAccount: string;
  servingProvider: string;
  opaqueRouteId: string;
  usageConfidence: 'provider_reported' | 'estimated' | 'unknown';
  commitState: string;
  retryCount: number;
  promptPreview: string;
  responsePreview: string;
}

export interface LiveRequest {
  requestId: string;
  keyName?: string | null;
  frontend: string;
  requestedModel: string;
  routeName?: string | null;
  phase: 'selecting' | 'streaming' | 'committed' | 'done';
  commitState: string;
  fallbackHops: number;
  retryCount: number;
  inputTokens?: number | null;
  outputTokens?: number | null;
  status: string;
  latencyMs: number;
  ttftMs?: number | null;
  finished: boolean;
}

export interface AuditLog {
  id: string;
  timestamp: string;
  actor: string;
  action: string;
  targetType: 'key' | 'provider' | 'account' | 'route' | 'alias' | 'model' | 'system';
  targetId: string;
  targetName: string;
  details: string;
}

export interface ProxyMetrics {
  activeStreams: number;
  totalRequests: number;
  totalTokens: number;
  totalSpendUsd: number;
  cacheHitRatio: number;
  fallbackRate: number;
  p50LatencyMs: number;
  p99LatencyMs: number;
  tunnelStatus: 'connected' | 'reconnecting' | 'error';
}
