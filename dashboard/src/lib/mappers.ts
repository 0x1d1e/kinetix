// Mappers from the Kinetix admin API JSON (snake_case) to the dashboard's
// camelCase view model. Keeping the mapping in one place means the views stay
// unchanged from the original design draft.

import {
  Account,
  AuditLog,
  Route,
  ModelAlias,
  ModelConfig,
  ModelPrice,
  Provider,
  ProxyMetrics,
  RequestLog,
  VirtualKey,
} from '../types';

const num = (v: any, d = 0): number => (typeof v === 'number' && isFinite(v) ? v : d);
const optionalBool = (v: any): boolean | undefined =>
  typeof v === 'boolean' ? v : undefined;
const optionalNum = (v: any): number | null =>
  typeof v === 'number' && isFinite(v) ? v : null;
const str = (v: any, d = ''): string => (typeof v === 'string' ? v : d);

export function mapKey(j: any): VirtualKey {
  return {
    id: str(j.id),
    key: str(j.key_mask, 'sk-kinetix-••••••••'),
    name: str(j.name),
    owner: str(j.owner),
    tag: str(j.tag),
    allowedModels: Array.isArray(j.allowed_models) ? j.allowed_models : ['*'],
    allowedProviders: Array.isArray(j.allowed_providers) ? j.allowed_providers : [],
    rpmLimit: num(j.rpm_limit, 0),
    tpmLimit: num(j.tpm_limit, 0),
    dailyBudget: num(j.daily_budget, 0),
    monthlyBudget: num(j.monthly_budget, 0),
    currentDailySpend: num(j.current_daily_spend),
    currentMonthlySpend: num(j.current_monthly_spend),
    createdAt: str(j.created_at),
    expiresAt: j.expires_at ?? null,
    status: (j.status as VirtualKey['status']) || 'active',
    allowedIps: Array.isArray(j.allowed_ips) ? j.allowed_ips : [],
    totalRequests: num(j.total_requests),
    totalTokens: num(j.total_tokens),
  };
}

export function mapProvider(j: any): Provider {
  const healthy = num(j.healthy_accounts);
  const total = num(j.accounts_count);
  const status: Provider['status'] =
    j.credential_mode === 'none'
      ? 'healthy'
      : total === 0
        ? 'degraded'
        : healthy > 0
          ? 'healthy'
          : 'error';
  return {
    id: str(j.id),
    name: str(j.name),
    baseUrl: str(j.base_url),
    wireFormat: (j.wire_format as Provider['wireFormat']) || 'openai',
    authScheme: (j.auth_scheme as Provider['authScheme']) || 'bearer',
    customHeaderName: j.custom_header_name ?? undefined,
    customParamName: j.custom_param_name ?? undefined,
    status,
    modelsCount: num(j.models_count),
    accountsCount: num(j.accounts_count),
    extraHeaders: j.extra_headers || {},
    modelsPath: j.models_path ?? undefined,
    timeoutMs: num(j.timeout_ms, 120000),
    capabilityMode: (j.capability_mode as Provider['capabilityMode']) || 'permissive',
    followRedirects: !!j.follow_redirects,
    credentialHosts: str(j.credential_hosts),
    allowInsecureTls: !!j.allow_insecure_tls,
    wirePlugin: str(j.wire_plugin),
    credentialPlugin: str(j.credential_plugin),
    modelSourcePlugin: str(j.model_source_plugin),
    credentialMode:
      j.credential_mode === 'auth_flow' || j.credential_mode === 'none'
        ? j.credential_mode
        : 'manual',
    sourcePluginId: j.source_plugin_id ?? undefined,
    sourceIntegrationId: j.source_integration_id ?? undefined,
    credentialEnrollment: {
      mode:
        j.credential_enrollment?.mode === 'auth_flow' || j.credential_enrollment?.mode === 'none'
          ? j.credential_enrollment.mode
          : 'manual',
      actionLabel: j.credential_enrollment?.action_label ?? null,
      available: j.credential_enrollment?.available !== false,
    },
    lastPingMs: 0,
  };
}

export function mapModel(j: any): ModelConfig {
  const c = j.capabilities || {};
  const p = j.prices || {};
  const prices: ModelPrice = {
    inputPer1M: optionalNum(p.input_per_1m),
    outputPer1M: optionalNum(p.output_per_1m),
    cachedPer1M: optionalNum(p.cached_per_1m),
    cacheWritePer1M: optionalNum(p.cache_write_per_1m),
    thinkingPer1M: optionalNum(p.thinking_per_1m),
  };
  const thinkingMap =
    j.thinking_map && typeof j.thinking_map === 'object' ? j.thinking_map : {};
  const thinkingLevels =
    thinkingMap.levels &&
    typeof thinkingMap.levels === 'object' &&
    !Array.isArray(thinkingMap.levels)
      ? { ...thinkingMap.levels }
      : {};
  return {
    id: str(j.id),
    providerId: str(j.provider_id),
    providerName: str(j.provider_name),
    upstreamModelId: str(j.upstream_id),
    displayName: str(j.display_name),
    enabled: !!j.enabled,
    contextWindow: optionalNum(j.context_window),
    maxOutputTokens: optionalNum(j.max_output_tokens),
    capabilities: {
      text: optionalBool(c.text),
      vision: optionalBool(c.vision),
      reasoning: optionalBool(c.reasoning),
      toolCalling: optionalBool(c.tool_calling),
      audio: optionalBool(c.audio),
      structuredOutput: optionalBool(c.structured_output),
      continuationFamilies: Array.isArray(c.continuation_families)
        ? c.continuation_families.filter(
            (family: unknown): family is string => typeof family === 'string',
          )
        : [],
    },
    prices,
    parameters: (j.parameters && typeof j.parameters === 'object'
      ? j.parameters
      : {}) as ModelConfig['parameters'],
    transportOverride:
      typeof j.transport_override === 'string' ? j.transport_override : null,
    thinkingMap: {
      levels: thinkingLevels,
      mode:
        thinkingMap.mode === 'manual_budget' ||
        thinkingMap.mode === 'level' ||
        thinkingMap.mode === 'adaptive'
          ? thinkingMap.mode
          : undefined,
      budgetField:
        typeof thinkingMap.budget_field === 'string' && thinkingMap.budget_field
          ? thinkingMap.budget_field
          : undefined,
      levelField:
        typeof thinkingMap.level_field === 'string' && thinkingMap.level_field
          ? thinkingMap.level_field
          : undefined,
    },
    discovery:
      j.discovery && typeof j.discovery === 'object' && !Array.isArray(j.discovery)
        ? { ...j.discovery }
        : {},
  };
}

export function mapAccount(j: any): Account {
  return {
    id: str(j.id),
    providerId: str(j.provider_id),
    providerName: str(j.provider_name),
    label: str(j.label),
    keyMasked: str(j.key_mask),
    status:
      j.status === 'cooldown' ||
      j.status === 'exhausted' ||
      j.status === 'disabled' ||
      j.status === 'degraded'
        ? j.status
        : 'healthy',
    statusReason: str(j.status_reason, 'unknown'),
    statusChangedAt: j.status_changed_at ?? null,
    retryAt: j.retry_at ?? null,
    cooldownUntil: j.cooldown_until ?? null,
    quotaResetTime: j.quota_reset_at ?? null,
    quotaType: (j.quota_type as Account['quotaType']) || 'none',
    softQuotaSpendLimit: j.soft_quota_usd ?? undefined,
    currentSpend: 0,
    requestsCount: num(j.requests_count),
    tokensCount: num(j.tokens_count),
    priority: num(j.priority, 1),
    weight: num(j.weight, 1),
    lastError: j.last_error ?? undefined,
  };
}

export function mapRoute(j: any): Route {
  const t = j.fallback_triggers || {};
  const targets = Array.isArray(j.targets) ? j.targets : [];
  const allExhausted = false;
  return {
    id: str(j.id),
    name: str(j.name),
    description: str(j.description),
    selectionStrategy: (j.strategy as Route['selectionStrategy']) || 'priority',
    fallbackTriggers: {
      on429: t.on429 !== false,
      onQuota: t.onQuota !== false,
      on5xx: t.on5xx !== false,
      onTimeout: t.onTimeout !== false,
    },
    targets: targets.map((x: any, idx: number) => ({
      id: str(x.id, `t-${idx}`),
      accountId: x.account_id ?? '',
      accountLabel: str(x.account_label, '(auto)'),
      providerName: str(x.provider_id),
      modelId: str(x.model_id),
      modelDisplayName: str(x.model_display_name),
      priority: num(x.priority, idx + 1),
      weight: num(x.weight, 1),
    })),
    portabilityPolicy: (j.portability_policy as Route['portabilityPolicy']) || 'strip_with_warning',
    cacheAffinity: !!j.cache_affinity,
    stickyRouting: !!j.sticky_routing,
    totalHops: 0,
    status: allExhausted ? 'all_exhausted' : j.enabled === false ? 'degraded' : 'active',
  };
}

export function mapAlias(j: any): ModelAlias {
  return {
    id: str(j.id),
    aliasName: str(j.alias),
    targetType: (j.target_type as ModelAlias['targetType']) || 'model',
    targetId: str(j.target_id),
    targetDisplayName: str(j.target_display_name, '—'),
    description: str(j.description),
  };
}

export function mapLiveRequest(j: any): import('../types').LiveRequest {
  return {
    requestId: j.request_id,
    keyName: j.key_name,
    frontend: j.frontend,
    requestedModel: j.requested_model,
    routeName: j.route_name,
    phase: j.phase,
    commitState: j.commit_state,
    fallbackHops: j.fallback_hops ?? 0,
    retryCount: j.retry_count ?? 0,
    inputTokens: j.input_tokens,
    outputTokens: j.output_tokens,
    status: j.status,
    latencyMs: j.latency_ms ?? 0,
    ttftMs: j.ttft_ms,
    finished: !!j.finished,
  };
}

export function mapRequest(j: any): RequestLog {
  return {
    id: str(j.id),
    requestId: str(j.request_id),
    timestamp: str(j.timestamp),
    virtualKeyId: str(j.key_id),
    virtualKeyName: str(j.key_name),
    clientFormat: (j.client_format as RequestLog['clientFormat']) || 'openai',
    requestedModel: str(j.requested_model),
    effectiveTarget: str(j.effective_model),
    routeName: j.route_name ?? undefined,
    fallbackHops: num(j.fallback_hops),
    fallbackPath: Array.isArray(j.fallback_path) ? j.fallback_path : [],
    status: (j.status as RequestLog['status']) || 'success',
    statusCode: num(j.status_code, 200),
    latencyMs: num(j.latency_ms),
    ttftMs: num(j.ttft_ms),
    inputTokens: num(j.input_tokens),
    outputTokens: num(j.output_tokens),
    cachedTokens: num(j.cached_tokens),
    cacheWriteTokens: num(j.cache_write_tokens),
    thinkingTokens: num(j.thinking_tokens),
    costUsd: num(j.cost_usd),
    cacheStatus: (j.cache_status as RequestLog['cacheStatus']) || 'bypass',
    servingAccount: str(j.serving_account),
    servingProvider: str(j.serving_provider),
    opaqueRouteId: str(j.opaque_route_id),
    usageConfidence: (j.usage_confidence as RequestLog['usageConfidence']) || 'unknown',
    commitState: str(j.commit_state),
    retryCount: num(j.retry_count),
    promptPreview: str(j.error_message) || '',
    responsePreview: '',
  };
}

export function mapAudit(j: any): AuditLog {
  return {
    id: str(j.id),
    timestamp: str(j.timestamp),
    actor: str(j.actor),
    action: str(j.action),
    targetType: (j.target_type as AuditLog['targetType']) || 'system',
    targetId: str(j.target_id),
    targetName: str(j.target_name),
    details: str(j.details),
  };
}

export function mapMetrics(j: any): ProxyMetrics {
  return {
    activeStreams: num(j.active_streams),
    totalRequests: num(j.total_requests),
    totalTokens: num(j.total_tokens),
    totalSpendUsd: num(j.total_spend_usd),
    cacheHitRatio: 0,
    fallbackRate: num(j.fallback_rate),
    p50LatencyMs: num(j.avg_latency_ms),
    p99LatencyMs: num(j.avg_latency_ms),
    tunnelStatus: (j.tunnel_status as ProxyMetrics['tunnelStatus']) || 'connected',
  };
}

export const EMPTY_METRICS: ProxyMetrics = {
  activeStreams: 0,
  totalRequests: 0,
  totalTokens: 0,
  totalSpendUsd: 0,
  cacheHitRatio: 0,
  fallbackRate: 0,
  p50LatencyMs: 0,
  p99LatencyMs: 0,
  tunnelStatus: 'connected',
};
