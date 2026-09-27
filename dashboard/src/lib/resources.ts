// Typed resource functions over the Kinetix admin API. Each returns the
// dashboard's camelCase view model (see mappers.ts).

import { api } from './api';
import {
  mapAccount,
  mapAlias,
  mapAudit,
  mapRoute,
  mapKey,
  mapMetrics,
  mapModel,
  mapProvider,
  mapRequest,
  mapLiveRequest,
} from './mappers';
import { Account, AuditLog, Route, ModelAlias, ModelConfig, Provider, ProxyMetrics, RequestLog, VirtualKey, LiveRequest } from '../types';

export interface CreateKeyInput {
  name: string;
  owner: string;
  tag: string;
  allowed_models: string[];
  rpm_limit?: number | null;
  tpm_limit?: number | null;
  daily_budget?: number | null;
  monthly_budget?: number | null;
}

export interface DiscoveredReasoningCapability {
  mode?: 'toggle' | 'manual_budget' | 'level' | 'adaptive' | null;
  levels: string[];
  default?: string | null;
  can_disable: boolean;
  upstream_format: string;
}

export interface DiscoveredThinkingMap {
  levels: Record<string, unknown>;
  mode?: 'manual_budget' | 'level' | 'adaptive' | null;
  budget_field?: string | null;
  level_field?: string | null;
}

export interface DiscoveredModel {
  id: string;
  display_name?: string | null;
  context_window?: number | null;
  max_output_tokens?: number | null;
  capabilities?: {
    text?: boolean | null;
    reasoning?: boolean | null;
    vision?: boolean | null;
    tool_calling?: boolean | null;
    structured_output?: boolean | null;
  } | null;
  reasoning_capability?: DiscoveredReasoningCapability | null;
  thinking_map?: DiscoveredThinkingMap | null;
  transport?: string | null;
  transport_source?: string | null;
  capability_sources?: Record<string, string | null> | null;
  modalities?: {
    input?: string[] | null;
    output?: string[] | null;
  } | null;
  prices?: {
    input_per_1m?: number | null;
    output_per_1m?: number | null;
    cached_per_1m?: number | null;
    cache_write_per_1m?: number | null;
    thinking_per_1m?: number | null;
  } | null;
  price_sources?: Record<string, string | null> | null;
  raw_metadata?: unknown | null;
  raw_metadata_truncated?: boolean;
  canonical_identity?: {
    status: 'resolved' | 'ambiguous' | 'unresolved';
    upstream_model_id: string;
    canonical_model_id?: string | null;
    match?: string | null;
    source?: string | null;
    candidates?: string[];
  } | null;
  canonical_model_id?: string | null;
  canonical_match?: string | null;
  provider_variant?: {
    kind: 'reasoning_tier' | 'provider_alias' | 'thinking_variant';
    id: string;
    reasoning_level?: string | null;
    fixed: boolean;
  } | null;
  opaque_state?: {
    kind: 'gemini_thought_signature';
    family: string;
    encoding_version: number;
    placeholder_strategy?: 'gemini3_skip_validator' | null;
  } | null;
  model_type?: string | null;
  execution_supported?: boolean;
  catalog?: {
    canonical?: {
      source?: string | null;
      reference?: string | null;
      canonical_model_id?: string | null;
      url?: string | null;
      model_type?: string | null;
      max_input_tokens?: number | null;
      metadata?: unknown;
    } | null;
    provider?: {
      source?: string | null;
      reference?: string | null;
      provider_id?: string | null;
      model_id?: string | null;
      url?: string | null;
      model_type?: string | null;
      max_input_tokens?: number | null;
      metadata?: unknown;
    } | null;
  } | null;
  reconciliation?: ModelReconciliation | null;
  already_imported: boolean;
}

export interface ReconciliationDiff {
  field: string;
  configured: unknown;
  observed: unknown;
  source?: unknown;
}

export interface ModelReconciliation {
  status: 'new' | 'unchanged' | 'changed' | 'ignored' | 'missing' | 'deprecated' | 'accepted' | string;
  checked_at?: string | null;
  last_success_at?: string | null;
  diff?: ReconciliationDiff[];
  pinned_fields?: string[];
}

export interface ModelLifecycleSettings {
  reconciliation_interval_secs: number;
  pricing_sync_interval_secs: number;
  jitter_secs: number;
  probe_freshness_secs: number;
}

export interface LifecycleLaneStatus {
  last_attempt?: string | null;
  last_success?: string | null;
  last_failure?: string | null;
  last_error?: string | null;
}

export interface ProviderLifecycleStatus {
  reconciliation: LifecycleLaneStatus;
  pricing_sync: LifecycleLaneStatus;
}

export interface CachedDiscoveryResponse {
  models: DiscoveredModel[];
  disappeared: unknown[];
  lifecycle: ProviderLifecycleStatus;
}

export interface CapabilityProbeResult {
  status: 'supported' | 'unsupported' | 'inconclusive';
  reason?: string;
  transport?: string;
  scope?: {
    provider_id: string;
    account_id: string;
    model_id: string;
    transport: string;
  };
  evidence?: {
    status: string;
    verified_at?: string;
    fresh_until?: string;
    estimated_max_cost_usd?: number;
    detail?: string | null;
    scope?: {
      provider_id: string;
      account_id: string;
      model_id: string;
      transport: string;
    };
  };
}

export interface TestResult {
  ok: boolean;
  status: number;
  latency_ms?: number;
  error?: string;
  response_preview?: string;
}

export interface ExportFile {
  name: string;
  day: string;
  kind: string;
  bytes: number;
}

export interface UsageDay {
  day: string;
  requests: number;
  tokens: number;
}

export interface PluginCapability {
  capability: string;
  name: string;
}

export interface PluginCatalogEntry {
  id: string;
  name: string;
  description: string;
  publisher: string;
  official: boolean;
  homepage: string;
  latest_version: string;
  artifact_name: string;
  capabilities: string[];
  installable: boolean;
  install_ready?: boolean;
  trust_status?: 'trusted' | 'unavailable' | 'discovery_only' | string;
  installed?: boolean;
  installed_version?: string | null;
  update_available?: boolean;
  distribution?: {
    url: string;
    sha256: string;
    publisher_key_id: string;
    allowed_hosts: string[];
  } | null;
  note?: string;
}

export interface PluginCatalogResponse {
  schema_version: number;
  plugins: PluginCatalogEntry[];
}

export interface PluginIntegrationProvider {
  base_url: string;
  wire_format: string;
  auth_scheme: string;
  custom_header_name?: string | null;
  custom_param_name?: string | null;
  extra_headers: Record<string, string>;
  timeout_ms: number;
  capability_mode: string;
  models_path?: string | null;
  follow_redirects: boolean;
  credential_hosts: string[];
}

export interface PluginIntegration {
  id: string;
  name: string;
  description: string;
  provider_adapter?: string | null;
  credential_strategy?: string | null;
  auth_flow?: string | null;
  credential_mode?: 'manual' | 'auth_flow' | 'none' | null;
  model_source?: string | null;
  provider?: PluginIntegrationProvider | null;
}

export interface PluginUiAction {
  id: string;
  label: string;
  kind: 'auth' | string;
  integration: string;
  description: string;
}

export interface PluginUiSetting {
  key: string;
  label: string;
  kind: 'text' | 'secret' | 'boolean' | 'select' | string;
  description: string;
  required: boolean;
  options: string[];
  default?: string | null;
}

export interface PluginUi {
  actions: PluginUiAction[];
  settings: PluginUiSetting[];
}

export interface PluginSettingState extends PluginUiSetting {
  configured: boolean;
  value: string | boolean | null;
}

export interface PluginSettingsResponse {
  id: string;
  settings: PluginSettingState[];
}

export interface PluginPermissions {
  network_hosts: string[];
  credential_scopes: string[];
  credential_read: boolean;
}

export interface PluginLimits {
  memory: string;
  wall_time_ms: number;
  max_outbound_requests: number;
  max_http_body: string;
  storage: string;
}

export interface PluginSummary {
  id: string;
  name: string;
  version: string;
  plugin_api_major: number;
  sha256: string;
  signature: string;
  status: string;
  provides: PluginCapability[];
  integrations: PluginIntegration[];
  ui: PluginUi;
  permissions: PluginPermissions;
  limits: PluginLimits;
  routing_facts_mode: 'pure' | 'cached' | string;
  routing_facts_refresh_ms: number;
}

export interface PluginPermissionGrant {
  plugin_id?: string;
  permission: string;
  value_json: string;
  approved_at?: string;
}

export interface PluginPermissionResponse {
  id: string;
  requested: PluginPermissions;
  approved: PluginPermissionGrant[];
}

export interface PluginPackage {
  plugin_id: string;
  version: string;
  package_sha256: string;
  package_path: string;
  signature: string;
  source: string;
  installed_at: string;
}

export interface PluginPermissionListDiff {
  added: string[];
  removed: string[];
}

export interface PluginPermissionBoolDiff {
  from: boolean;
  to: boolean;
  changed: boolean;
}

export interface PluginPermissionDiff {
  network_hosts: PluginPermissionListDiff;
  credential_scopes: PluginPermissionListDiff;
  credential_read: PluginPermissionBoolDiff;
}

export interface PluginRollbackPreview {
  id: string;
  current_version: string;
  target_version: string;
  package_sha256: string;
  signature: string;
  source: string;
  permissions: PluginPermissions;
  permission_diff: PluginPermissionDiff;
  provides: PluginCapability[];
}

export interface PluginCatalogPreview {
  id: string;
  name: string;
  current_version: string | null;
  target_version: string;
  sha256: string;
  signature: 'verified' | string;
  source: string;
  permissions: PluginPermissions;
  permission_diff: PluginPermissionDiff;
  provides: PluginCapability[];
}

export interface PluginDetail extends PluginSummary {
  permissions_approved?: PluginPermissionGrant[];
  runtime?: Record<string, unknown> | null;
  packages?: PluginPackage[];
}

export interface PluginInstallInput {
  package_base64?: string;
  url?: string;
  sha256?: string;
  trusted_keys?: string[];
  allow_untrusted_signature?: boolean;
}

export interface PluginInstallResult {
  id: string;
  version: string;
  sha256: string;
  signature: string;
  provides: PluginCapability[];
  enabled: boolean;
  note?: string;
}

export const Kinetix = {
  // --- session -------------------------------------------------------------
  me: () => api.get<{ authenticated: boolean; user: string }>('/admin/api/me'),
  login: (password: string) => api.post<{ ok: boolean; user: string }>('/admin/api/login', { password }),
  logout: () => api.post<{ ok: boolean }>('/admin/api/logout'),
  changePassword: (current_password: string, new_password: string) =>
    api.post<{ ok: boolean; note: string }>('/admin/api/password', { current_password, new_password }),

  publicBaseUrl: () =>
    api.get<{ public_base_url: string; source: 'dashboard' | 'environment'; environment_default: string }>(
      '/admin/api/settings/public-base-url',
    ),
  updatePublicBaseUrl: (public_base_url: string) =>
    api.put<{ ok: boolean; public_base_url: string; source: 'dashboard' }>(
      '/admin/api/settings/public-base-url',
      { public_base_url },
    ),

  modelLifecycleSettings: () =>
    api.get<ModelLifecycleSettings>('/admin/api/settings/model-lifecycle'),
  updateModelLifecycleSettings: (body: Partial<ModelLifecycleSettings>) =>
    api.put<ModelLifecycleSettings & { ok: boolean }>('/admin/api/settings/model-lifecycle', body),

  // --- usage exports -------------------------------------------------------
  async exports(): Promise<{ dir: string; retention_days: number; files: ExportFile[]; days: UsageDay[] }> {
    return api.get('/admin/api/exports');
  },
  exportDay: (day?: string) => api.post<{ ok: boolean; day: string; jsonl: string; csv: string }>('/admin/api/exports', { day: day ?? null }),
  deleteExport: (name: string) => api.del(`/admin/api/exports/${encodeURIComponent(name)}`),

  // --- overview ------------------------------------------------------------
  async overview(): Promise<ProxyMetrics> {
    return mapMetrics(await api.get('/admin/api/overview'));
  },

  // --- virtual keys --------------------------------------------------------
  async keys(): Promise<VirtualKey[]> {
    const r = await api.get<{ keys: any[] }>('/admin/api/keys');
    return r.keys.map(mapKey);
  },
  async createKey(body: CreateKeyInput): Promise<{ key: VirtualKey; fullKey: string }> {
    const r = await api.post<{ key: any; full_key: string }>('/admin/api/keys', body);
    return { key: mapKey(r.key), fullKey: r.full_key };
  },
  updateKey: (id: string, body: Record<string, unknown>) => api.put(`/admin/api/keys/${id}`, body),
  deleteKey: (id: string) => api.del(`/admin/api/keys/${id}`),

  // --- providers -----------------------------------------------------------
  async providers(): Promise<Provider[]> {
    const r = await api.get<{ providers: any[] }>('/admin/api/providers');
    return r.providers.map(mapProvider);
  },
  createProvider: (body: Record<string, unknown>) => api.post('/admin/api/providers', body),
  async getProvider(id: string): Promise<Provider> {
    return mapProvider(await api.get(`/admin/api/providers/${id}`));
  },
  updateProvider: (id: string, body: Record<string, unknown>) => api.put(`/admin/api/providers/${id}`, body),
  deleteProvider: (id: string) => api.del(`/admin/api/providers/${id}`),
  validateProvider: (body: Record<string, unknown>) =>
    api.post<{ valid: boolean; problems: string[]; warnings: string[]; outbound_security: string }>(
      '/admin/api/validate/provider',
      body,
    ),
  cachedDiscovery: (providerId: string) =>
    api.get<CachedDiscoveryResponse>(`/admin/api/providers/${providerId}/discover`),
  async discover(providerId: string): Promise<DiscoveredModel[]> {
    const r = await api.post<{ models: DiscoveredModel[] }>(`/admin/api/providers/${providerId}/discover`);
    return r.models;
  },
  reconcileProvider: (providerId: string) =>
    api.post<{ models: DiscoveredModel[]; disappeared: unknown[] }>(
      `/admin/api/providers/${providerId}/reconcile`,
      {},
    ),
  syncProviderPricing: (providerId: string) =>
    api.post<{ ok: boolean; updated: string[]; skipped_manual: string[] }>(
      `/admin/api/providers/${providerId}/pricing/sync`,
      {},
    ),
  test: (providerId: string, model: string) =>
    api.post<TestResult>(`/admin/api/providers/${providerId}/test`, { model }),

  // --- models --------------------------------------------------------------
  async models(): Promise<ModelConfig[]> {
    const r = await api.get<{ models: any[] }>('/admin/api/models');
    return r.models.map(mapModel);
  },
  createModel: (providerId: string, body: Record<string, unknown>) =>
    api.post(`/admin/api/providers/${providerId}/models`, body),
  updateModel: (id: string, body: Record<string, unknown>) => api.put(`/admin/api/models/${id}`, body),
  reconcileModel: (id: string, action: 'accept' | 'ignore' | 'pin', fields: string[] = []) =>
    api.put<{ ok: boolean }>(`/admin/api/models/${id}/reconciliation`, { action, fields }),
  probeModel: (
    id: string,
    capability: string,
    value?: unknown,
    max_cost_usd?: number,
    account_id?: string,
    transport?: string,
  ) =>
    api.post<CapabilityProbeResult>(`/admin/api/models/${id}/probe`, {
      capability,
      ...(value === undefined ? {} : { value }),
      ...(max_cost_usd === undefined ? {} : { max_cost_usd }),
      ...(account_id === undefined ? {} : { account_id }),
      ...(transport === undefined ? {} : { transport }),
    }),
  deleteModel: (id: string) => api.del(`/admin/api/models/${id}`),

  // --- accounts ------------------------------------------------------------
  async validateModel(body: Record<string, unknown>) {
    return api.post<{ valid: boolean; problems: string[]; warnings: string[] }>(
      '/admin/api/validate/model',
      body,
    );
  },
  async accounts(): Promise<Account[]> {
    const r = await api.get<{ accounts: any[] }>('/admin/api/accounts');
    return r.accounts.map(mapAccount);
  },
  async validateAccount(body: Record<string, unknown>) {
    return api.post<{ valid: boolean; problems: string[] }>('/admin/api/validate/account', body);
  },
  createAccount: (body: Record<string, unknown>) => api.post('/admin/api/accounts', body),
  startProviderCredentialEnrollment: (providerId: string) =>
    api.post<{
      authorize_url: string;
      redirect_uri: string;
      state: string;
      expires_in_secs: number;
      manual_callback_supported: boolean;
    }>(
      `/admin/api/providers/${encodeURIComponent(providerId)}/credential-enrollment/start`,
      {},
    ),
  updateAccount: (id: string, body: Record<string, unknown>) => api.put(`/admin/api/accounts/${id}`, body),
  deleteAccount: (id: string) => api.del(`/admin/api/accounts/${id}`),
  resetAccount: (id: string) => api.post(`/admin/api/accounts/${id}/reset`),
  testAccount: (id: string, model?: string) =>
    api.post<TestResult>(`/admin/api/accounts/${id}/test`, model ? { model } : {}),

  // --- routes --------------------------------------------------------------
  async routes(): Promise<Route[]> {
    const r = await api.get<{ routes: any[] }>('/admin/api/routes');
    return r.routes.map(mapRoute);
  },
  createRoute: (body: Record<string, unknown>) => api.post('/admin/api/routes', body),
  updateRoute: (id: string, body: Record<string, unknown>) => api.put(`/admin/api/routes/${id}`, body),
  deleteRoute: (id: string) => api.del(`/admin/api/routes/${id}`),
  dryRunRoute: (model: string, descriptor: Record<string, unknown>) =>
    api.post<any>('/admin/api/routes/dry-run', { model, ...descriptor }),

  // --- aliases -------------------------------------------------------------
  async aliases(): Promise<ModelAlias[]> {
    const r = await api.get<{ aliases: any[] }>('/admin/api/aliases');
    return r.aliases.map(mapAlias);
  },
  createAlias: (body: Record<string, unknown>) => api.post('/admin/api/aliases', body),
  deleteAlias: (id: string) => api.del(`/admin/api/aliases/${id}`),

  // --- usage / requests ----------------------------------------------------
  async requests(limit = 200): Promise<RequestLog[]> {
    const r = await api.get<{ usage: any[] }>(`/admin/api/usage?limit=${limit}`);
    return r.usage.map(mapRequest);
  },

  // Live in-flight view (FR-8.3): metadata-only snapshot of requests currently
  // being served plus a short finished tail.
  async liveRequests(): Promise<LiveRequest[]> {
    const r = await api.get<{ live: any[] }>('/admin/api/requests/live');
    return r.live.map(mapLiveRequest);
  },

  // --- plugins -------------------------------------------------------------
  async plugins(): Promise<PluginSummary[]> {
    const r = await api.get<{ plugins: PluginSummary[] }>('/admin/api/plugins');
    return r.plugins;
  },
  pluginCatalog: (params?: { q?: string; capability?: string; refresh?: boolean }) => {
    const sp = new URLSearchParams();
    if (params?.q) sp.set('q', params.q);
    if (params?.capability) sp.set('capability', params.capability);
    if (params?.refresh) sp.set('refresh', 'true');
    const qs = sp.toString();
    return api.get<PluginCatalogResponse>(`/admin/api/plugins/catalog${qs ? `?${qs}` : ''}`);
  },
  refreshPluginCatalog: () =>
    api.post<{ schema_version: number; count: number; refreshed: boolean }>(
      '/admin/api/plugins/catalog/refresh',
      {},
    ),
  previewCatalogPlugin: (id: string) =>
    api.get<PluginCatalogPreview>(
      `/admin/api/plugins/catalog/${encodeURIComponent(id)}/preview`,
    ),
  installCatalogPlugin: (id: string) =>
    api.post<PluginInstallResult>(
      `/admin/api/plugins/catalog/${encodeURIComponent(id)}/install`,
    ),
  plugin: (id: string) =>
    api.get<PluginDetail>(`/admin/api/plugins/${encodeURIComponent(id)}`),
  pluginPermissions: (id: string) =>
    api.get<PluginPermissionResponse>(
      `/admin/api/plugins/${encodeURIComponent(id)}/permissions`,
    ),
  pluginSettings: (id: string) =>
    api.get<PluginSettingsResponse>(
      `/admin/api/plugins/${encodeURIComponent(id)}/settings`,
    ),
  updatePluginSettings: (id: string, values: Record<string, unknown>) =>
    api.put<PluginSettingsResponse>(
      `/admin/api/plugins/${encodeURIComponent(id)}/settings`,
      { values },
    ),
  installPlugin: (body: PluginInstallInput) =>
    api.post<PluginInstallResult>('/admin/api/plugins/install', body),
  setupPluginIntegrationProvider: (pluginId: string, integrationId: string) =>
    api.post<{ id: string; name: string; created: boolean }>(
      `/admin/api/plugins/${encodeURIComponent(pluginId)}/integrations/${encodeURIComponent(integrationId)}/provider`,
    ),
  startPluginAuth: (plugin_id: string, flow_name: string, provider_id: string) =>
    api.post<{
      authorize_url: string;
      redirect_uri: string;
      state: string;
      expires_in_secs: number;
      manual_callback_supported: boolean;
    }>(
      '/admin/api/plugins/auth/start',
      { plugin_id, flow_name, provider_id },
    ),
  completePluginAuth: (callback_url: string) =>
    api.post<{ ok: boolean; result: string; provider_id?: string | null }>(
      '/admin/api/plugins/auth/complete',
      { callback_url },
    ),
  pluginAuthStatus: (state: string) =>
    api.get<{ result: string; provider_id?: string | null }>(
      `/admin/api/plugins/auth/status?state=${encodeURIComponent(state)}`,
    ),
  approvePluginPermissions: (id: string) =>
    api.post<{ ok: boolean; id: string; approved: PluginPermissionGrant[] }>(
      `/admin/api/plugins/${encodeURIComponent(id)}/permissions/approve`,
      {},
    ),
  revokePluginPermission: (id: string, permission: string) =>
    api.post<{ ok: boolean; id: string; revoked: string; enabled?: boolean }>(
      `/admin/api/plugins/${encodeURIComponent(id)}/permissions/revoke`,
      { permission },
    ),
  enablePlugin: (id: string) =>
    api.post<{ ok: boolean; id: string; enabled: boolean }>(
      `/admin/api/plugins/${encodeURIComponent(id)}/enable`,
    ),
  disablePlugin: (id: string) =>
    api.post<{ ok: boolean; id: string; enabled: boolean }>(
      `/admin/api/plugins/${encodeURIComponent(id)}/disable`,
    ),
  validatePlugin: (id: string) =>
    api.post<{ ok: boolean; id: string; provides: PluginCapability[] }>(
      `/admin/api/plugins/${encodeURIComponent(id)}/validate`,
    ),
  previewPluginRollback: (id: string, sha256: string) =>
    api.get<PluginRollbackPreview>(
      `/admin/api/plugins/${encodeURIComponent(id)}/packages/${encodeURIComponent(sha256)}/preview`,
    ),
  rollbackPlugin: (id: string, sha256: string) =>
    api.post<PluginInstallResult>(
      `/admin/api/plugins/${encodeURIComponent(id)}/rollback`,
      { sha256 },
    ),
  reinstallPluginPackage: (id: string, sha256: string) =>
    api.post<PluginInstallResult>(
      `/admin/api/plugins/${encodeURIComponent(id)}/packages/${encodeURIComponent(sha256)}/reinstall`,
    ),
  removePlugin: (id: string) =>
    api.del<{ ok: boolean; id: string }>(
      `/admin/api/plugins/${encodeURIComponent(id)}`,
    ),

  // --- audit ---------------------------------------------------------------
  async audit(limit = 200): Promise<AuditLog[]> {
    const r = await api.get<{ audit: any[] }>(`/admin/api/audit?limit=${limit}`);
    return r.audit.map(mapAudit);
  },
};
