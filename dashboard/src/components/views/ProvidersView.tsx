import React, { useEffect, useState } from 'react';
import { Server, Plus, RefreshCw, CheckCircle2, Globe, Cpu, Sliders, ExternalLink, HelpCircle, Trash2, X, Pencil, Search } from 'lucide-react';
import { Provider, ModelConfig, Account } from '../../types';
import { WobblyCard, SketchButton, SketchBadge } from '../HandDrawnElements';
import { DESIGN_TOKENS } from '../../lib/designSystem';
import { Kinetix, DiscoveredModel, ProviderLifecycleStatus } from '../../lib/resources';

/**
 * Defaults for newly configured manual models. Imported/discovered sparse
 * metadata remains null/undefined until the operator explicitly sets it.
 */
const DEFAULT_CONTEXT_WINDOW = 200000;
const DEFAULT_MAX_OUTPUT = 8192;
const CANONICAL_THINKING_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const;
type CanonicalThinkingLevel = (typeof CANONICAL_THINKING_LEVELS)[number];

const thinkingValueToInput = (value: unknown): string => {
  if (value === undefined) return '';
  return JSON.stringify(value) ?? '';
};

const parseThinkingInput = (raw: string): unknown => {
  const value = raw.trim();
  if (!value) return undefined;
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
};

const pluginManagedReasoning = (model: ModelConfig) => {
  const discovery = model.discovery as {
    reasoning_capability?: {
      levels?: string[];
      default?: string | null;
    } | null;
    provider_variant?: {
      kind?: string;
      id?: string;
      reasoning_level?: string | null;
      fixed?: boolean;
    } | null;
    capability_sources?: {
      reasoning?: string | null;
    } | null;
  } | undefined;
  const source = discovery?.capability_sources?.reasoning;
  if (!source?.includes('plugin_capabilities_json')) return null;
  return {
    capability: discovery?.reasoning_capability || null,
    variant: discovery?.provider_variant || null,
  };
};

const modelReconciliation = (model: ModelConfig) =>
  (model.discovery as {
    reconciliation?: {
      status?: string;
      checked_at?: string | null;
      last_success_at?: string | null;
      diff?: Array<{ field?: string; configured?: unknown; observed?: unknown; source?: unknown }>;
      pinned_fields?: string[];
    } | null;
  } | undefined)?.reconciliation || null;

const modelPricingSource = (model: ModelConfig) =>
  (model.discovery as {
    effective_pricing?: { source?: string | null; updated_at?: string | null } | null;
  } | undefined)?.effective_pricing?.source || 'untracked';

type ProbeEvidenceEntry = {
  status?: string;
  verified_at?: string;
  fresh_until?: string;
  scope?: {
    provider_id?: string;
    account_id?: string;
    model_id?: string;
    transport?: string;
  };
};

const modelProbeEvidence = (model: ModelConfig) =>
  (model.discovery as {
    probe_evidence?: Record<string, ProbeEvidenceEntry | ProbeEvidenceEntry[]> | null;
  } | undefined)?.probe_evidence || {};

const probeEvidenceEntries = (model: ModelConfig, key: string): ProbeEvidenceEntry[] => {
  const evidence = modelProbeEvidence(model)[key];
  if (!evidence) return [];
  return Array.isArray(evidence) ? evidence : [evidence];
};

const probeEvidenceSummary = (model: ModelConfig, key: string, accountId: string) => {
  const entries = probeEvidenceEntries(model, key).filter(
    (entry) => !accountId || entry.scope?.account_id === accountId,
  );
  if (!entries.length) return '—';
  return entries
    .map((entry) => `${entry.scope?.transport || 'unknown'}:${entry.status || '—'}`)
    .join('/');
};

const modelProbeTransport = (model: ModelConfig, provider: Provider) => {
  const discovery = model.discovery as {
    transport?: { format?: string } | null;
  } | undefined;
  if (model.transportOverride) return model.transportOverride;
  if (discovery?.transport?.format) return discovery.transport.format;
  if (provider.wireFormat === 'plugin') return provider.wirePlugin || 'plugin';
  return provider.wireFormat;
};

const modelReasoningLevels = (model: ModelConfig) => {
  const discovery = model.discovery as {
    reasoning_capability?: { levels?: string[]; can_disable?: boolean } | null;
  } | undefined;
  const discovered = discovery?.reasoning_capability?.levels || [];
  const mapped = Object.keys(model.thinkingMap?.levels || {});
  return Array.from(new Set([...discovered, ...mapped])).filter((level) => level && level !== 'off' && level !== 'default');
};

const formatDriftValue = (value: unknown) => {
  if (value === undefined) return 'undefined';
  const serialized = JSON.stringify(value, null, 2);
  return serialized === undefined ? String(value) : serialized;
};

const modelCanProbeReasoningDisable = (model: ModelConfig) => {
  const discovery = model.discovery as {
    reasoning_capability?: { can_disable?: boolean } | null;
  } | undefined;
  return discovery?.reasoning_capability?.can_disable === true
    || Object.prototype.hasOwnProperty.call(model.thinkingMap?.levels || {}, 'off');
};


interface ProvidersViewProps {
  providers: Provider[];
  models: ModelConfig[];
  onAddProvider: (provider: Provider) => Promise<void> | void;
  onUpdateProvider: (providerId: string, provider: Provider) => Promise<void>;
  onAddModel: (model: ModelConfig) => void;
  onUpdateModel: (model: ModelConfig) => void;
  onDeleteModel: (modelId: string) => void;
  onDeleteProvider: (providerId: string) => void;
  onRefresh?: () => void;
}

export const ProvidersView: React.FC<ProvidersViewProps> = ({
  providers,
  models,
  onAddProvider,
  onUpdateProvider,
  onAddModel,
  onUpdateModel,
  onDeleteModel,
  onDeleteProvider,
  onRefresh,
}) => {
  const [selectedProviderId, setSelectedProviderId] = useState<string>(providers[0]?.id || '');
  const [showAddProviderModal, setShowAddProviderModal] = useState(false);
  const [showAddModelModal, setShowAddModelModal] = useState(false);
  const [confirmDeleteModelId, setConfirmDeleteModelId] = useState<string | null>(null);
  const [confirmDeleteProviderId, setConfirmDeleteProviderId] = useState<string | null>(null);
  const [isDiscovering, setIsDiscovering] = useState(false);
  const [discoveryResults, setDiscoveryResults] = useState<DiscoveredModel[] | null>(null);
  const [discoverySearch, setDiscoverySearch] = useState('');
  const [providerSearch, setProviderSearch] = useState('');
  const [pingStatus, setPingStatus] = useState<Record<string, { ok: boolean; pingMs: number; error?: string }>>({});
  const [lifecycleBusy, setLifecycleBusy] = useState<string | null>(null);
  const [lifecycleNotice, setLifecycleNotice] = useState<string | null>(null);
  const [probeStatus, setProbeStatus] = useState<Record<string, string>>({});
  const [lifecycleSettings, setLifecycleSettings] = useState<{
    reconciliation_interval_secs: number;
    pricing_sync_interval_secs: number;
    jitter_secs: number;
    probe_freshness_secs: number;
  } | null>(null);
  const [savingLifecycleSettings, setSavingLifecycleSettings] = useState(false);
  const [providerLifecycle, setProviderLifecycle] = useState<ProviderLifecycleStatus | null>(null);
  const [reconciliationSelections, setReconciliationSelections] = useState<Record<string, string[]>>({});
  const [probeAccounts, setProbeAccounts] = useState<Account[]>([]);
  const [probeAccountByProvider, setProbeAccountByProvider] = useState<Record<string, string>>({});

  useEffect(() => {
    Kinetix.modelLifecycleSettings()
      .then(setLifecycleSettings)
      .catch(() => setLifecycleSettings(null));
  }, []);

  useEffect(() => {
    Kinetix.accounts()
      .then(setProbeAccounts)
      .catch(() => setProbeAccounts([]));
  }, []);


  // New Provider Form State
  const [name, setName] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [wireFormat, setWireFormat] = useState<'gemini' | 'openai' | 'anthropic' | 'plugin'>('gemini');
  const [authScheme, setAuthScheme] = useState<'bearer' | 'custom_header' | 'query_param'>('bearer');
  const [customHeader, setCustomHeader] = useState('');
  const [customParam, setCustomParam] = useState('');
  const [modelsPath, setModelsPath] = useState('/models');
  const [extraHeaders, setExtraHeaders] = useState('');
  const [credentialHosts, setCredentialHosts] = useState('');
  const [followRedirects, setFollowRedirects] = useState(false);
  const [allowInsecureTls, setAllowInsecureTls] = useState(false);
  const [wirePlugin, setWirePlugin] = useState('');
  const [credentialPlugin, setCredentialPlugin] = useState('');
  const [modelSourcePlugin, setModelSourcePlugin] = useState('');
  const [timeoutMs, setTimeoutMs] = useState(120000);
  const [capabilityMode, setCapabilityMode] = useState<'permissive' | 'strict'>('permissive');
  const [apiKey, setApiKey] = useState('');
  const [accountLabel, setAccountLabel] = useState('');
  const [validation, setValidation] = useState<{ valid: boolean; problems: string[]; warnings: string[] } | null>(null);
  const [validating, setValidating] = useState(false);
  const [editingProviderId, setEditingProviderId] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);

  /** Parse a "Header: value" per line textarea into an object. */
  const parseHeaders = (text: string): Record<string, string> => {
    const out: Record<string, string> = {};
    for (const line of text.split('\n')) {
      const idx = line.indexOf(':');
      if (idx > 0) {
        const k = line.slice(0, idx).trim();
        const v = line.slice(idx + 1).trim();
        if (k) out[k] = v;
      }
    }
    return out;
  };

  // New Custom Model Form State
  const [modelUpstreamId, setModelUpstreamId] = useState('');
  const [modelDisplayName, setModelDisplayName] = useState('');
  const [modelTransportOverride, setModelTransportOverride] = useState('');
  const [modelContextWindow, setModelContextWindow] = useState(DEFAULT_CONTEXT_WINDOW);
  const [modelMaxOutput, setModelMaxOutput] = useState(DEFAULT_MAX_OUTPUT);
  const [modelInputPrice, setModelInputPrice] = useState<number | null>(1.0);
  const [modelOutputPrice, setModelOutputPrice] = useState<number | null>(4.0);
  const [modelCachedPrice, setModelCachedPrice] = useState<number | null>(0);
  const [modelCacheWritePrice, setModelCacheWritePrice] = useState<number | null>(0);
  const [modelThinkingPrice, setModelThinkingPrice] = useState<number | null>(0);
  const [capText, setCapText] = useState<boolean | undefined>(true);
  const [capVision, setCapVision] = useState<boolean | undefined>(true);
  const [capReasoning, setCapReasoning] = useState<boolean | undefined>(false);
  const [capTools, setCapTools] = useState<boolean | undefined>(true);
  const [capStructuredOutput, setCapStructuredOutput] = useState<boolean | undefined>(false);
  const [modelThinkingOff, setModelThinkingOff] = useState('');
  const [modelThinkingMinimal, setModelThinkingMinimal] = useState('');
  const [modelThinkingLow, setModelThinkingLow] = useState('');
  const [modelThinkingMedium, setModelThinkingMedium] = useState('');
  const [modelThinkingHigh, setModelThinkingHigh] = useState('');
  const [modelThinkingXHigh, setModelThinkingXHigh] = useState('');
  const [modelThinkingMax, setModelThinkingMax] = useState('');
  const [modelThinkingMode, setModelThinkingMode] = useState<'' | 'manual_budget' | 'level' | 'adaptive'>('');
  const [modelThinkingBudgetField, setModelThinkingBudgetField] = useState('');
  const [modelThinkingLevelField, setModelThinkingLevelField] = useState('');
  const [modelThinkingExtraLevels, setModelThinkingExtraLevels] = useState<Record<string, unknown>>({});
  const [modelValidation, setModelValidation] = useState<{ valid: boolean; problems: string[]; warnings: string[] } | null>(null);
  const [validatingModel, setValidatingModel] = useState(false);
  const [editingModelId, setEditingModelId] = useState<string | null>(null);

  const providerQuery = providerSearch.trim().toLowerCase();
  const filteredProviders = providers.filter((p) => {
    if (!providerQuery) return true;
    return [p.id, p.name, p.baseUrl, p.wireFormat, p.authScheme]
      .some((value) => String(value ?? '').toLowerCase().includes(providerQuery));
  });
  const activeProvider =
    filteredProviders.find((p) => p.id === selectedProviderId) ||
    filteredProviders[0];
  const providerModels = models.filter((m) => m.providerId === activeProvider?.id);
  const probeAccountsForProvider = (providerId: string) =>
    probeAccounts.filter((account) => account.providerId === providerId);
  const selectedProbeAccountId = (providerId: string) => {
    const accounts = probeAccountsForProvider(providerId);
    const selected = probeAccountByProvider[providerId];
    if (selected && accounts.some((account) => account.id === selected)) return selected;
    return accounts.find((account) => account.status === 'healthy')?.id || accounts[0]?.id || '';
  };
  const selectedProbeAccount = (providerId: string) => {
    const accountId = selectedProbeAccountId(providerId);
    return probeAccounts.find((account) => account.id === accountId);
  };

  useEffect(() => {
    if (!activeProvider) {
      setDiscoveryResults(null);
      setProviderLifecycle(null);
      return;
    }
    let cancelled = false;
    Kinetix.cachedDiscovery(activeProvider.id)
      .then((cached) => {
        if (cancelled) return;
        setDiscoveryResults(cached.models);
        setProviderLifecycle(cached.lifecycle);
      })
      .catch(() => {
        if (!cancelled) setProviderLifecycle(null);
      });
    return () => {
      cancelled = true;
    };
  }, [activeProvider?.id]);

  // Fuzzy search over the discovered model list: case-insensitive, and every
  // whitespace-separated term must match as a subsequence of the model id.
  const discoveryMatches = (id: string, query: string) => {
    const q = query.trim().toLowerCase();
    if (!q) return true;
    const terms = q.split(/\s+/).filter(Boolean);
    return terms.every((term) => {
      const hay = id.toLowerCase();
      if (hay.includes(term)) return true;
      // subsequence match (typo/skip friendly)
      let i = 0;
      for (const ch of hay) {
        if (ch === term[i]) i++;
        if (i === term.length) return true;
      }
      return false;
    });
  };
  const filteredDiscovery = (discoveryResults ?? []).filter((m) =>
    discoveryMatches(m.id, discoverySearch),
  );

  const handleTestPing = async (providerId: string) => {
    const firstModel = models.find((m) => m.providerId === providerId)?.upstreamModelId;
    try {
      const r = await Kinetix.test(providerId, firstModel || 'test');
      setPingStatus((prev) => ({
        ...prev,
        [providerId]: { ok: r.ok, pingMs: r.latency_ms ?? 0, error: r.error },
      }));
    } catch (e) {
      setPingStatus((prev) => ({
        ...prev,
        [providerId]: { ok: false, pingMs: 0, error: e instanceof Error ? e.message : String(e) },
      }));
    }
  };

  const handleReconcileProvider = async () => {
    if (!activeProvider) return;
    setLifecycleBusy('reconcile');
    setLifecycleNotice(null);
    try {
      const result = await Kinetix.reconcileProvider(activeProvider.id);
      setDiscoveryResults(result.models);
      setLifecycleNotice(`Reconciled ${result.models.length} upstream model observations.`);
      onRefresh?.();
    } catch (error) {
      setLifecycleNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setLifecycleBusy(null);
    }
  };

  const handleSyncPricing = async () => {
    if (!activeProvider) return;
    setLifecycleBusy('pricing');
    setLifecycleNotice(null);
    try {
      const result = await Kinetix.syncProviderPricing(activeProvider.id);
      setLifecycleNotice(
        `Pricing sync updated ${result.updated.length}; preserved ${result.skipped_manual.length} manual models.`,
      );
      onRefresh?.();
    } catch (error) {
      setLifecycleNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setLifecycleBusy(null);
    }
  };

  const handleReconciliationAction = async (
    modelId: string,
    action: 'accept' | 'ignore' | 'pin',
    fields: string[] = [],
  ) => {
    setLifecycleBusy(`${action}:${modelId}`);
    try {
      await Kinetix.reconcileModel(modelId, action, fields);
      setLifecycleNotice(`Model drift ${action} completed.`);
      setReconciliationSelections((current) => ({ ...current, [modelId]: [] }));
      onRefresh?.();
    } catch (error) {
      setLifecycleNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setLifecycleBusy(null);
    }
  };

  const handleProbeModel = async (
    model: ModelConfig,
    capability: 'transport' | 'reasoning' | 'reasoning_disable' | 'tool_calling' | 'structured_output',
    value?: string,
  ) => {
    const key = `${model.id}:${capability}${value ? `:${value}` : ''}`;
    const accountId = selectedProbeAccountId(model.providerId);
    if (!accountId) {
      setProbeStatus((prev) => ({ ...prev, [key]: 'select an account' }));
      return;
    }
    setLifecycleBusy(`probe:${key}`);
    try {
      const result = await Kinetix.probeModel(model.id, capability, value, undefined, accountId);
      setProbeStatus((prev) => ({ ...prev, [key]: result.status }));
      onRefresh?.();
    } catch (error) {
      setProbeStatus((prev) => ({
        ...prev,
        [key]: error instanceof Error ? error.message : String(error),
      }));
    } finally {
      setLifecycleBusy(null);
    }
  };

  const handleSaveLifecycleSettings = async () => {
    if (!lifecycleSettings) return;
    setSavingLifecycleSettings(true);
    try {
      const saved = await Kinetix.updateModelLifecycleSettings(lifecycleSettings);
      setLifecycleSettings(saved);
      setLifecycleNotice('Lifecycle schedule saved.');
    } catch (error) {
      setLifecycleNotice(error instanceof Error ? error.message : String(error));
    } finally {
      setSavingLifecycleSettings(false);
    }
  };

  const handleFetchModelsDiscovery = async () => {
    if (!activeProvider) return;
    setIsDiscovering(true);
    setDiscoveryResults(null);
    setDiscoverySearch('');
    try {
      const found = await Kinetix.discover(activeProvider.id);
      setDiscoveryResults(found);
    } catch (e) {
      setDiscoveryResults([]);
      setPingStatus((prev) => ({
        ...prev,
        [activeProvider.id]: {
          ok: false,
          pingMs: 0,
          error: e instanceof Error ? e.message : String(e),
        },
      }));
    } finally {
      setIsDiscovering(false);
    }
  };

  const handleImportDiscoveredModel = (m: DiscoveredModel) => {
    if (m.execution_supported === false) return;
    const discoveredThinking = m.thinking_map;
    const discoveredPrices = m.prices;
    const newModel: ModelConfig = {
      id: '',
      providerId: activeProvider.id,
      providerName: activeProvider.name,
      upstreamModelId: m.id,
      displayName: m.display_name || m.id,
      enabled: true,
      contextWindow: m.context_window ?? null,
      maxOutputTokens: m.max_output_tokens ?? null,
      capabilities: {
        text: m.capabilities?.text ?? undefined,
        vision: m.capabilities?.vision ?? undefined,
        reasoning:
          m.capabilities?.reasoning ??
          (m.reasoning_capability ? true : undefined),
        toolCalling: m.capabilities?.tool_calling ?? undefined,
        structuredOutput: m.capabilities?.structured_output ?? undefined,
      },
      prices: {
        inputPer1M: discoveredPrices?.input_per_1m ?? null,
        outputPer1M: discoveredPrices?.output_per_1m ?? null,
        cachedPer1M: discoveredPrices?.cached_per_1m ?? null,
        cacheWritePer1M: discoveredPrices?.cache_write_per_1m ?? null,
        thinkingPer1M: discoveredPrices?.thinking_per_1m ?? null,
      },
      parameters: {},
      thinkingMap: discoveredThinking
        ? {
            levels: { ...discoveredThinking.levels },
            mode:
              discoveredThinking.mode === 'manual_budget' ||
              discoveredThinking.mode === 'level' ||
              discoveredThinking.mode === 'adaptive'
                ? discoveredThinking.mode
                : undefined,
            budgetField: discoveredThinking.budget_field || undefined,
            levelField: discoveredThinking.level_field || undefined,
          }
        : { levels: {} },
      discovery: {
        capabilities: m.capabilities || {},
        reasoning_capability: m.reasoning_capability || null,
        thinking_map: m.thinking_map || null,
        transport: m.transport ? { format: m.transport } : null,
        transport_source: m.transport_source || null,
        capability_sources: m.capability_sources || {},
        modalities: m.modalities || null,
        prices: m.prices || null,
        price_sources: m.price_sources || {},
        raw_metadata: m.raw_metadata ?? null,
        raw_metadata_truncated: m.raw_metadata_truncated ?? false,
        canonical_identity: m.canonical_identity || null,
        canonical_model_id: m.canonical_model_id || null,
        canonical_match: m.canonical_match || null,
        provider_variant: m.provider_variant || null,
        opaque_state: m.opaque_state || null,
        model_type: m.model_type || null,
        execution_supported: m.execution_supported ?? true,
        catalog: m.catalog || null,
        imported_from_discovery: true,
      },
    };

    onAddModel(newModel);
    setDiscoveryResults((prev) => prev?.filter((x) => x.id !== m.id) || null);
  };

  const headersToText = (h?: Record<string, string>): string =>
    h ? Object.entries(h).map(([k, v]) => `${k}: ${v}`).join('\n') : '';

  const resetProviderForm = () => {
    setEditingProviderId(null);
    setName('');
    setBaseUrl('');
    setWireFormat('gemini');
    setAuthScheme('bearer');
    setCustomHeader('');
    setCustomParam('');
    setModelsPath('/models');
    setExtraHeaders('');
    setCredentialHosts('');
    setFollowRedirects(false);
    setAllowInsecureTls(false);
    setWirePlugin('');
    setCredentialPlugin('');
    setModelSourcePlugin('');
    setTimeoutMs(120000);
    setCapabilityMode('permissive');
    setApiKey('');
    setAccountLabel('');
    setValidation(null);
  };

  const openEditProvider = (p: Provider) => {
    setEditingProviderId(p.id);
    setName(p.name);
    setBaseUrl(p.baseUrl);
    setWireFormat(p.wireFormat);
    setAuthScheme(p.authScheme);
    setCustomHeader(p.customHeaderName || '');
    setCustomParam(p.customParamName || '');
    setModelsPath(p.modelsPath || '/models');
    setExtraHeaders(headersToText(p.extraHeaders));
    setCredentialHosts(p.credentialHosts || '');
    setFollowRedirects(!!p.followRedirects);
    setAllowInsecureTls(!!p.allowInsecureTls);
    setWirePlugin(p.wirePlugin || '');
    setCredentialPlugin(p.credentialPlugin || '');
    setModelSourcePlugin(p.modelSourcePlugin || '');
    setTimeoutMs(p.timeoutMs || 120000);
    setCapabilityMode(p.capabilityMode || 'permissive');
    // Credentials are never returned by the API; leave the key field blank.
    setApiKey('');
    setAccountLabel('');
    setValidation(null);
    setShowAddProviderModal(true);
  };

  const providerBody = (includeKey = false) => ({
    name: name.trim(),
    base_url: baseUrl.trim(),
    wire_format: wireFormat,
    auth_scheme: authScheme,
    custom_header_name: authScheme === 'custom_header' ? customHeader.trim() || null : null,
    custom_param_name: authScheme === 'query_param' ? customParam.trim() || null : null,
    extra_headers: parseHeaders(extraHeaders),
    timeout_ms: timeoutMs,
    capability_mode: capabilityMode,
    models_path: modelsPath.trim() || null,
    credential_hosts: credentialHosts.trim(),
    follow_redirects: followRedirects,
    allow_insecure_tls: allowInsecureTls,
    wire_plugin: wirePlugin.trim(),
    credential_plugin: credentialPlugin.trim(),
    model_source_plugin: modelSourcePlugin.trim(),
    // Only sent when the admin actually typed a credential.
    ...(includeKey && apiKey.trim()
      ? { api_key: apiKey.trim(), account_label: accountLabel.trim() || null }
      : {}),
  });

  const handleValidateProvider = async () => {
    if (!name.trim() || !baseUrl.trim()) return;
    setValidating(true);
    try {
      const r = await Kinetix.validateProvider(providerBody(false));
      setValidation({ valid: r.valid, problems: r.problems || [], warnings: r.warnings || [] });
    } catch (e) {
      setValidation({ valid: false, problems: [(e as Error).message], warnings: [] });
    } finally {
      setValidating(false);
    }
  };

  const handleCreateProvider = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim() || !baseUrl.trim()) return;

    const existingProvider = editingProviderId
      ? providers.find((provider) => provider.id === editingProviderId)
      : undefined;
    const prov: Provider = {
      id: editingProviderId || '',
      name: name.trim(),
      baseUrl: baseUrl.trim(),
      wireFormat,
      authScheme,
      customHeaderName: authScheme === 'custom_header' ? customHeader : undefined,
      customParamName: authScheme === 'query_param' ? customParam : undefined,
      status: 'healthy',
      modelsCount: 0,
      accountsCount: 0,
      credentialMode: existingProvider?.credentialMode ?? 'manual',
      credentialEnrollment: existingProvider?.credentialEnrollment ?? {
        mode: 'manual',
        actionLabel: 'Add API Key',
        available: true,
      },
      extraHeaders: parseHeaders(extraHeaders),
      modelsPath: modelsPath.trim() || undefined,
      timeoutMs,
      capabilityMode,
      followRedirects,
      credentialHosts: credentialHosts.trim(),
      allowInsecureTls,
      wirePlugin: wirePlugin.trim(),
      credentialPlugin: credentialPlugin.trim(),
      modelSourcePlugin: modelSourcePlugin.trim(),
      lastPingMs: 0,
    };

    setIsSaving(true);
    try {
      if (editingProviderId) {
        // api_key is omitted unless a new one was typed (rotating the pool key).
        await onUpdateProvider(editingProviderId, {
          ...prov,
          ...(apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
          ...(accountLabel.trim() ? { accountLabel: accountLabel.trim() } : {}),
        });
      } else {
        await onAddProvider({
          ...prov,
          ...(apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
          ...(accountLabel.trim() ? { accountLabel: accountLabel.trim() } : {}),
        });
        setSelectedProviderId(prov.id);
      }
      setShowAddProviderModal(false);
      resetProviderForm();
    } finally {
      setIsSaving(false);
    }
  };

  const currentThinkingMap = (): ModelConfig['thinkingMap'] => {
    const levels = { ...modelThinkingExtraLevels };
    const inputs: Record<CanonicalThinkingLevel, string> = {
      off: modelThinkingOff,
      minimal: modelThinkingMinimal,
      low: modelThinkingLow,
      medium: modelThinkingMedium,
      high: modelThinkingHigh,
      xhigh: modelThinkingXHigh,
      max: modelThinkingMax,
    };
    for (const level of CANONICAL_THINKING_LEVELS) {
      const value = parseThinkingInput(inputs[level]);
      if (value === undefined) {
        delete levels[level];
      } else {
        levels[level] = value;
      }
    }
    return {
      levels,
      mode: modelThinkingMode || undefined,
      budgetField:
        modelThinkingMode === 'level' || modelThinkingMode === 'adaptive'
          ? undefined
          : modelThinkingBudgetField.trim() || undefined,
      levelField:
        modelThinkingMode === 'level' || modelThinkingMode === 'adaptive'
          ? modelThinkingLevelField.trim() || undefined
          : undefined,
    };
  };

  const thinkingLevelInputs = [
    ['off', modelThinkingOff, setModelThinkingOff],
    ['minimal', modelThinkingMinimal, setModelThinkingMinimal],
    ['low', modelThinkingLow, setModelThinkingLow],
    ['medium', modelThinkingMedium, setModelThinkingMedium],
    ['high', modelThinkingHigh, setModelThinkingHigh],
    ['xhigh', modelThinkingXHigh, setModelThinkingXHigh],
    ['max', modelThinkingMax, setModelThinkingMax],
  ] as const;

  /** Prefill the model form for editing an existing model. */
  const openEditModel = (m: ModelConfig) => {
    setEditingModelId(m.id);
    setModelUpstreamId(m.upstreamModelId);
    setModelDisplayName(m.displayName);
    setModelTransportOverride(m.transportOverride || '');
    setModelContextWindow(m.contextWindow ?? 0);
    setModelMaxOutput(m.maxOutputTokens ?? 0);
    setModelInputPrice(m.prices.inputPer1M);
    setModelOutputPrice(m.prices.outputPer1M);
    setModelCachedPrice(m.prices.cachedPer1M);
    setModelCacheWritePrice(m.prices.cacheWritePer1M);
    setModelThinkingPrice(m.prices.thinkingPer1M);
    setCapText(m.capabilities.text);
    setCapVision(m.capabilities.vision);
    setCapReasoning(m.capabilities.reasoning);
    setCapTools(m.capabilities.toolCalling);
    setCapStructuredOutput(m.capabilities.structuredOutput);
    const { off, minimal, low, medium, high, xhigh, max, ...extraLevels } = m.thinkingMap.levels;
    setModelThinkingOff(thinkingValueToInput(off));
    setModelThinkingMinimal(thinkingValueToInput(minimal));
    setModelThinkingLow(thinkingValueToInput(low));
    setModelThinkingMedium(thinkingValueToInput(medium));
    setModelThinkingHigh(thinkingValueToInput(high));
    setModelThinkingXHigh(thinkingValueToInput(xhigh));
    setModelThinkingMax(thinkingValueToInput(max));
    setModelThinkingMode(m.thinkingMap.mode || '');
    setModelThinkingBudgetField(m.thinkingMap.budgetField || '');
    setModelThinkingLevelField(m.thinkingMap.levelField || '');
    setModelThinkingExtraLevels(extraLevels);
    setModelValidation(null);
    setShowAddModelModal(true);
  };

  const resetModelForm = () => {
    setEditingModelId(null);
    setModelUpstreamId('');
    setModelDisplayName('');
    setModelTransportOverride('');
    setModelContextWindow(DEFAULT_CONTEXT_WINDOW);
    setModelMaxOutput(DEFAULT_MAX_OUTPUT);
    setModelInputPrice(1.0);
    setModelOutputPrice(4.0);
    setModelCachedPrice(0);
    setModelCacheWritePrice(0);
    setModelThinkingPrice(0);
    setCapText(true);
    setCapVision(true);
    setCapReasoning(false);
    setCapTools(true);
    setCapStructuredOutput(false);
    setModelThinkingOff('');
    setModelThinkingMinimal('');
    setModelThinkingLow('');
    setModelThinkingMedium('');
    setModelThinkingHigh('');
    setModelThinkingXHigh('');
    setModelThinkingMax('');
    setModelThinkingMode('');
    setModelThinkingBudgetField('');
    setModelThinkingLevelField('');
    setModelThinkingExtraLevels({});
    setModelValidation(null);
  };

  const normalizedPrice = (value: number | null): number | null =>
    typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;

  const handleCreateCustomModel = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!modelUpstreamId.trim()) return;

    const editingModel = editingModelId
      ? models.find((model) => model.id === editingModelId)
      : undefined;
    const newModel: ModelConfig = {
      id: editingModelId || '',
      providerId: activeProvider.id,
      providerName: activeProvider.name,
      upstreamModelId: modelUpstreamId.trim(),
      displayName: modelDisplayName.trim() || modelUpstreamId.trim(),
      transportOverride: modelTransportOverride.trim() || null,
      enabled: editingModel?.enabled ?? true,
      contextWindow:
        Number(modelContextWindow) || (editingModelId ? null : DEFAULT_CONTEXT_WINDOW),
      maxOutputTokens:
        Number(modelMaxOutput) || (editingModelId ? null : DEFAULT_MAX_OUTPUT),
      capabilities: {
        text: capText,
        vision: capVision,
        reasoning: capReasoning,
        toolCalling: capTools,
        audio: editingModelId ? editingModel?.capabilities.audio : false,
        structuredOutput: capStructuredOutput,
      },
      prices: {
        inputPer1M: normalizedPrice(modelInputPrice),
        outputPer1M: normalizedPrice(modelOutputPrice),
        cachedPer1M: normalizedPrice(modelCachedPrice),
        cacheWritePer1M: normalizedPrice(modelCacheWritePrice),
        thinkingPer1M: normalizedPrice(modelThinkingPrice),
      },
      parameters: {},
      thinkingMap: currentThinkingMap(),
    };

    if (editingModelId) {
      onUpdateModel(newModel);
    } else {
      onAddModel(newModel);
    }
    setShowAddModelModal(false);
    resetModelForm();
  };

  const modelBody = () => {
    const thinkingMap = currentThinkingMap();
    const editingModel = editingModelId
      ? models.find((model) => model.id === editingModelId)
      : undefined;
    return {
      upstream_id: modelUpstreamId.trim(),
      display_name: modelDisplayName.trim() || modelUpstreamId.trim(),
      transport_override: modelTransportOverride.trim() || null,
      enabled: true,
      context_window:
        Number(modelContextWindow) || (editingModelId ? null : DEFAULT_CONTEXT_WINDOW),
      max_output_tokens:
        Number(modelMaxOutput) || (editingModelId ? null : DEFAULT_MAX_OUTPUT),
      capabilities: {
        text: capText,
        vision: capVision,
        reasoning: capReasoning,
        tool_calling: capTools,
        audio: editingModelId ? editingModel?.capabilities.audio : false,
        structured_output: capStructuredOutput,
      },
      prices: {
        input_per_1m: normalizedPrice(modelInputPrice),
        output_per_1m: normalizedPrice(modelOutputPrice),
        cached_per_1m: normalizedPrice(modelCachedPrice),
        cache_write_per_1m: normalizedPrice(modelCacheWritePrice),
        thinking_per_1m: normalizedPrice(modelThinkingPrice),
      },
      thinking_map: {
        levels: thinkingMap.levels,
        mode: thinkingMap.mode || null,
        budget_field: thinkingMap.budgetField || null,
        level_field: thinkingMap.levelField || null,
      },
    };
  };

  const handleValidateModel = async () => {
    if (!modelUpstreamId.trim()) return;
    setValidatingModel(true);
    try {
      const r = await Kinetix.validateModel(modelBody());
      setModelValidation({ valid: r.valid, problems: r.problems || [], warnings: r.warnings || [] });
    } catch (e) {
      setModelValidation({ valid: false, problems: [(e as Error).message], warnings: [] });
    } finally {
      setValidatingModel(false);
    }
  };

  return (
    <div className="space-y-6">
      {/* Top Banner */}
      <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Upstream Providers & Models</span>
            <SketchBadge variant="yellow" rotation="-1deg">
              No Vendor Presets (FR-10)
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Configure upstream LLM APIs, fetch model lists, define parameter clamping, and map thinking controls.
          </p>
        </div>

        <SketchButton
          variant="primary"
          size="md"
          onClick={() => {
            resetProviderForm();
            setShowAddProviderModal(true);
          }}
          className="gap-2 font-heading font-bold"
        >
          <Plus className="w-5 h-5" />
          Add Upstream Provider
        </SketchButton>
      </div>

      {/* Main layout */}
      {providers.length === 0 ? (
        <WobblyCard decoration="tack" className="p-10 text-center bg-[var(--surface)]">
          <Server className="w-12 h-12 text-[var(--pen-blue)] mx-auto mb-3 opacity-60" />
          <h3 className="text-2xl font-heading font-bold text-[var(--ink)]">No Upstream Providers Configured</h3>
          <p className="text-base font-body text-[var(--ink)]/80 max-w-lg mx-auto mt-2 mb-6">
            Register your upstream LLM providers (e.g. Gemini, OpenAI, Anthropic, DeepSeek, or local Ollama). Kinetix proxies client calls and maps protocols automatically.
          </p>
          <SketchButton
            variant="primary"
            size="md"
            onClick={() => {
              resetProviderForm();
              setShowAddProviderModal(true);
            }}
            className="gap-2 font-heading font-bold"
          >
            <Plus className="w-5 h-5" />
            Add First Upstream Provider
          </SketchButton>
        </WobblyCard>
      ) : (
        <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
          {/* Left: Providers Selector */}
          <div className="space-y-4">
            <h3 className="text-xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
              <Server className="w-5 h-5 text-[var(--pen-blue)]" />
              Configured Upstreams ({filteredProviders.length}/{providers.length})
            </h3>

            <div className="relative">
              <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-[var(--ink)]/50" />
              <input
                type="search"
                value={providerSearch}
                onChange={(e) => setProviderSearch(e.target.value)}
                placeholder="Search providers…"
                className="w-full pl-9 pr-9 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
                style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
              />
              {providerSearch && (
                <button
                  type="button"
                  onClick={() => setProviderSearch('')}
                  className="absolute right-2 top-1/2 -translate-y-1/2 p-1 text-[var(--ink)]/60 hover:text-[var(--marker-red)] cursor-pointer"
                  title="Clear search"
                >
                  <X className="w-4 h-4" />
                </button>
              )}
            </div>

            {filteredProviders.length === 0 && (
              <div className="p-5 text-center bg-[var(--surface)] border-2 border-dashed border-[var(--ink)]/30 rounded">
                <p className="text-sm font-mono text-[var(--ink)]/70">No providers match “{providerSearch}”.</p>
                <button onClick={() => setProviderSearch('')} className="mt-2 text-xs font-heading font-bold text-[var(--pen-blue)] hover:underline cursor-pointer">
                  Clear search
                </button>
              </div>
            )}

            {filteredProviders.map((prov, idx) => {
              const isSelected = prov.id === activeProvider?.id;
              const tilt = idx % 2 === 0 ? '-rotate-0.5' : 'rotate-0.5';
              const ping = pingStatus[prov.id];

              return (
                <div
                  key={prov.id}
                  onClick={() => {
                    setSelectedProviderId(prov.id);
                    setDiscoveryResults(null);
                  }}
                  className={`p-4 border-2 border-[var(--ink)] cursor-pointer transition-all ${tilt} ${
                    isSelected
                      ? 'bg-[var(--postit)] sketch-shadow -translate-y-1 font-bold'
                      : 'bg-[var(--surface)] hover:bg-[var(--erased-soft)] sketch-shadow-sm'
                  }`}
                  style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                >
                  <div className="flex items-start justify-between gap-2">
                    <div>
                      <span className="font-mono text-xs px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded uppercase">
                        {prov.wireFormat} wire
                      </span>
                      <h4 className="font-heading text-lg mt-1 text-[var(--ink)]">{prov.name}</h4>
                      <p className="text-xs font-mono text-[var(--ink)]/70 truncate max-w-[200px]">
                        {prov.baseUrl}
                      </p>
                    </div>

                    <div className="flex flex-col items-end gap-1">
                      <SketchBadge variant={prov.status === 'healthy' ? 'green' : prov.status === 'degraded' ? 'yellow' : 'red'}>
                        {prov.status}
                      </SketchBadge>
                      {ping && (
                        <span className={`text-xs font-mono ${ping.ok ? 'text-[var(--pen-green)]' : 'text-[var(--marker-red)]'}`}>
                          {ping.ok ? `${ping.pingMs}ms` : 'error'}
                        </span>
                      )}
                    </div>
                  </div>

                  <div className="mt-3 pt-2 border-t border-[var(--ink)]/20 flex items-center justify-between text-xs font-mono">
                    <span>Auth: <strong>{prov.authScheme}</strong></span>
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        handleTestPing(prov.id);
                      }}
                      className="hover:underline text-[var(--pen-blue)] cursor-pointer"
                    >
                      {pingStatus[prov.id] ? (pingStatus[prov.id].ok ? '⚡ OK' : '⚡ Failed') : '⚡ Test Ping'}
                    </button>
                  </div>
                </div>
              );
            })}
          </div>

          {/* Right: Active Provider & Models Editor */}
          {activeProvider && (
            <div className="lg:col-span-2 space-y-6">
              <WobblyCard decoration="tape" className="p-6">
                {/* Provider Info Header */}
                <div className="flex flex-wrap items-start justify-between gap-4 pb-4 border-b-2 border-dashed border-[var(--ink)]/30 mb-4">
                  <div>
                    <h3 className="text-2xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
                      <Globe className="w-6 h-6 text-[var(--pen-blue)]" />
                      {activeProvider.name}
                    </h3>
                    <code className="text-sm font-mono text-[var(--ink)]/80 bg-[var(--erased)] px-2 py-0.5 rounded border border-[var(--ink)]/30 inline-block mt-1">
                      Base URL: {activeProvider.baseUrl}
                    </code>
                  </div>

                  <div className="flex flex-wrap items-center gap-2">
                    <SketchButton
                      variant="secondary"
                      size="sm"
                      disabled={isDiscovering}
                      onClick={handleFetchModelsDiscovery}
                      className="gap-1.5 font-heading"
                    >
                      <RefreshCw className={`w-4 h-4 ${isDiscovering ? 'animate-spin' : ''}`} />
                      {isDiscovering ? 'Querying Upstream...' : 'Fetch Models (Discovery)'}
                    </SketchButton>
                    <SketchButton
                      variant="secondary"
                      size="sm"
                      disabled={lifecycleBusy === 'reconcile'}
                      onClick={handleReconcileProvider}
                      className="gap-1.5 font-heading"
                    >
                      <RefreshCw className={`w-4 h-4 ${lifecycleBusy === 'reconcile' ? 'animate-spin' : ''}`} />
                      Reconcile
                    </SketchButton>
                    <SketchButton
                      variant="secondary"
                      size="sm"
                      disabled={lifecycleBusy === 'pricing'}
                      onClick={handleSyncPricing}
                      className="gap-1.5 font-heading"
                    >
                      Sync Pricing
                    </SketchButton>
                    <SketchButton
                      variant="secondary"
                      size="sm"
                      onClick={() => openEditProvider(activeProvider)}
                      className="gap-1.5 font-heading"
                    >
                      <Sliders className="w-4 h-4" />
                      Edit
                    </SketchButton>
                    <SketchButton
                      variant="primary"
                      size="sm"
                      onClick={() => setShowAddModelModal(true)}
                      className="gap-1 font-heading font-bold"
                    >
                      <Plus className="w-4 h-4" />
                      Add Model
                    </SketchButton>

                    {confirmDeleteProviderId === activeProvider.id ? (
                      <div className="flex items-center gap-1 bg-[var(--tint-red)] px-2.5 py-1 border border-[var(--marker-red)] rounded text-xs font-heading">
                        <span className="text-[var(--danger-text)] font-bold">Delete {activeProvider.name}?</span>
                        <button
                          onClick={() => {
                            onDeleteProvider(activeProvider.id);
                            setConfirmDeleteProviderId(null);
                          }}
                          className="px-2 py-0.5 bg-[var(--marker-red)] text-[var(--surface)] rounded font-bold hover:brightness-90 cursor-pointer"
                        >
                          Confirm
                        </button>
                        <button
                          onClick={() => setConfirmDeleteProviderId(null)}
                          className="px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded hover:bg-[var(--erased)] cursor-pointer"
                        >
                          Cancel
                        </button>
                      </div>
                    ) : (
                      <button
                        onClick={() => setConfirmDeleteProviderId(activeProvider.id)}
                        className="px-2.5 py-1 text-xs font-heading font-bold text-[var(--marker-red)] hover:bg-[var(--tint-red)] border border-[var(--marker-red)]/50 hover:border-[var(--marker-red)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                        title="Delete this upstream provider"
                      >
                        <Trash2 className="w-3.5 h-3.5" />
                        <span>Delete Provider</span>
                      </button>
                    )}
                  </div>
                </div>

                <div className="mb-5 grid grid-cols-1 xl:grid-cols-2 gap-3">
                  <div className="p-3 bg-[var(--paper)] border border-[var(--ink)] rounded text-xs font-mono">
                    <strong className="font-heading text-sm block mb-2">Model lifecycle</strong>
                    <div className="grid grid-cols-2 gap-2">
                      <label>
                        Reconcile interval (sec)
                        <input
                          type="number"
                          min={0}
                          value={lifecycleSettings?.reconciliation_interval_secs ?? 0}
                          onChange={(e) =>
                            setLifecycleSettings((current) =>
                              current
                                ? { ...current, reconciliation_interval_secs: Number(e.target.value) }
                                : current,
                            )
                          }
                          className="mt-1 w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1 rounded"
                        />
                      </label>
                      <label>
                        Pricing sync interval (sec)
                        <input
                          type="number"
                          min={0}
                          value={lifecycleSettings?.pricing_sync_interval_secs ?? 0}
                          onChange={(e) =>
                            setLifecycleSettings((current) =>
                              current
                                ? { ...current, pricing_sync_interval_secs: Number(e.target.value) }
                                : current,
                            )
                          }
                          className="mt-1 w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1 rounded"
                        />
                      </label>
                    </div>
                    <div className="mt-2 flex items-center justify-between gap-2">
                      <span className="text-[var(--ink)]/60">0 disables scheduling; minimum enabled interval is 300s.</span>
                      <button
                        type="button"
                        disabled={!lifecycleSettings || savingLifecycleSettings}
                        onClick={handleSaveLifecycleSettings}
                        className="px-2 py-1 border border-[var(--ink)] rounded font-heading font-bold hover:bg-[var(--erased)] disabled:opacity-50"
                      >
                        {savingLifecycleSettings ? 'Saving…' : 'Save schedule'}
                      </button>
                    </div>
                  </div>
                  <div className="p-3 bg-[var(--paper)] border border-[var(--ink)] rounded text-xs font-mono">
                    <strong className="font-heading text-sm block mb-1">Lifecycle policy</strong>
                    <div>Metadata reconciliation observes drift; it never silently changes configured model fields.</div>
                    <div>Pricing sync preserves operator-owned prices and only adopts known upstream/catalog price fields.</div>
                    <div>Capability probes are explicit, bounded, scoped, and expire after the configured freshness window.</div>
                    {providerLifecycle && (
                      <div className="mt-2 pt-2 border-t border-[var(--ink)]/20 space-y-1">
                        <div>
                          Reconcile · attempt {providerLifecycle.reconciliation.last_attempt ? new Date(providerLifecycle.reconciliation.last_attempt).toLocaleString() : '—'}
                          {' · '}success {providerLifecycle.reconciliation.last_success ? new Date(providerLifecycle.reconciliation.last_success).toLocaleString() : '—'}
                        </div>
                        {providerLifecycle.reconciliation.last_error && (
                          <div className="text-[var(--danger-text)]">
                            Last reconciliation failure: {providerLifecycle.reconciliation.last_error}
                          </div>
                        )}
                        <div>
                          Pricing · attempt {providerLifecycle.pricing_sync.last_attempt ? new Date(providerLifecycle.pricing_sync.last_attempt).toLocaleString() : '—'}
                          {' · '}success {providerLifecycle.pricing_sync.last_success ? new Date(providerLifecycle.pricing_sync.last_success).toLocaleString() : '—'}
                        </div>
                        {providerLifecycle.pricing_sync.last_error && (
                          <div className="text-[var(--danger-text)]">
                            Last pricing failure: {providerLifecycle.pricing_sync.last_error}
                          </div>
                        )}
                      </div>
                    )}
                    {lifecycleNotice && (
                      <div className="mt-2 pt-2 border-t border-[var(--ink)]/20 text-[var(--pen-blue)]">
                        {lifecycleNotice}
                      </div>
                    )}
                  </div>
                </div>

              {/* Model Discovery Results (if any) */}
              {discoveryResults && (
                <div className="p-4 bg-[var(--postit)] border-2 border-[var(--ink)] sketch-shadow-sm mb-6 rounded-lg">
                  <h4 className="font-heading font-bold text-lg text-[var(--ink)] mb-1">
                    🔍 Discovered Upstream Models (Live Probe)
                  </h4>
                  <p className="text-sm font-body text-[var(--ink)]/80 mb-3">
                    The endpoint returned the following model IDs. Select which models to import into Kinetix:
                  </p>
                  {discoveryResults.length > 0 && (
                    <div className="mb-3">
                      <input
                        type="text"
                        value={discoverySearch}
                        onChange={(e) => setDiscoverySearch(e.target.value)}
                        placeholder={`Search ${discoveryResults.length} models… (fuzzy)`}
                        className="w-full md:w-96 px-3 py-1.5 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm rounded outline-none focus:border-[var(--pen-blue)]"
                      />
                    </div>
                  )}
                  <div className="flex flex-wrap gap-2">
                    {discoveryResults.length === 0 && (
                      <span className="text-sm font-mono text-[var(--danger-text)]">
                        No models returned (check the credential or the error above).
                      </span>
                    )}
                    {discoveryResults.length > 0 &&
                      filteredDiscovery.map((m) => (
                      <div
                        key={m.id}
                        className="bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-1.5 text-xs font-mono sketch-shadow-sm flex items-center gap-2 rounded"
                      >
                        <span className="font-bold">{m.id}</span>
                        {m.canonical_model_id ? (
                          <span className="text-[var(--ink)]/50">
                            canonical: {m.canonical_model_id}
                          </span>
                        ) : null}
                        {m.model_type ? (
                          <span className="text-[var(--marker-red)]">
                            type: {m.model_type}
                          </span>
                        ) : null}
                        {m.context_window ? (
                          <span className="text-[var(--ink)]/50">{m.context_window.toLocaleString()} ctx</span>
                        ) : null}
                        {m.transport ? (
                          <span className="text-[var(--pen-blue)]">transport: {m.transport}</span>
                        ) : null}
                        {m.reasoning_capability ? (
                          <span className="text-[var(--pen-blue)]">
                            reasoning:{' '}
                            {m.capability_sources?.reasoning?.includes('plugin_capabilities_json')
                              ? 'plugin-managed · '
                              : ''}
                            {m.provider_variant?.kind === 'reasoning_tier' && m.provider_variant.fixed
                              ? (m.provider_variant.reasoning_level || m.provider_variant.id)
                              : m.reasoning_capability.levels.join('/')}
                          </span>
                        ) : null}
                        {m.capabilities?.vision ? (
                          <span className="text-[var(--pen-blue)]">vision</span>
                        ) : null}
                        {m.capabilities?.tool_calling ? (
                          <span className="text-[var(--pen-blue)]">tools</span>
                        ) : null}
                        {m.already_imported ? (
                          <span className="text-[var(--pen-green)] font-bold">✓ imported</span>
                        ) : m.execution_supported === false ? (
                          <span className="text-[var(--marker-red)] font-bold">
                            not executable
                          </span>
                        ) : (
                          <button
                            onClick={() => handleImportDiscoveredModel(m)}
                            className="bg-[var(--pen-green)] text-[var(--surface)] px-2 py-0.5 rounded hover:bg-[var(--success-text)] cursor-pointer"
                          >
                            + Import
                          </button>
                        )}
                      </div>
                    ))}
                    {discoveryResults.length > 0 && filteredDiscovery.length === 0 && (
                      <span className="text-sm font-mono text-[var(--ink)]/60">
                        No models match “{discoverySearch}”.
                      </span>
                    )}
                  </div>
                </div>
              )}

              {/* Models List for this Provider */}
              <div className="space-y-4">
                <div className="flex items-center justify-between">
                  <h4 className="text-xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
                    <Cpu className="w-5 h-5 text-[var(--marker-red)]" />
                    Configured Models ({providerModels.length})
                  </h4>
                  {providerModels.length > 0 && (
                    <button
                      onClick={() => setShowAddModelModal(true)}
                      className="text-xs font-heading font-bold text-[var(--pen-blue)] hover:underline flex items-center gap-1 cursor-pointer"
                    >
                      <Plus className="w-3.5 h-3.5" />
                      Configure Another Model
                    </button>
                  )}
                </div>

                {providerModels.length === 0 ? (
                  <div className="p-8 text-center bg-[var(--surface)] border-2 border-dashed border-[var(--ink)]/30 rounded-lg">
                    <Cpu className="w-10 h-10 text-[var(--ink)]/40 mx-auto mb-2" />
                    <p className="font-heading font-bold text-lg text-[var(--ink)]">No Models Configured</p>
                    <p className="text-sm font-body text-[var(--ink)]/70 max-w-md mx-auto mt-1 mb-4">
                      Probe upstream models via live discovery or manually register custom upstream model IDs for this provider.
                    </p>
                    <div className="flex items-center justify-center gap-3">
                      <SketchButton
                        variant="secondary"
                        size="sm"
                        onClick={handleFetchModelsDiscovery}
                        disabled={isDiscovering}
                      >
                        Fetch Models (Discovery)
                      </SketchButton>
                      <SketchButton
                        variant="primary"
                        size="sm"
                        onClick={() => setShowAddModelModal(true)}
                      >
                        + Add Custom Model
                      </SketchButton>
                    </div>
                  </div>
                ) : (
                  <div className="space-y-3">
                    {providerModels.map((m) => (
                      <div
                        key={m.id}
                        className="p-4 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm rounded-lg"
                      >
                        <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-3 border-b border-[var(--ink)]/20 pb-2 mb-3">
                          <div>
                            <div className="flex items-center gap-2 flex-wrap">
                              <span className="font-heading font-bold text-lg text-[var(--ink)]">
                                {m.displayName}
                              </span>
                              <span className="text-xs font-mono bg-[var(--erased)] px-1.5 py-0.5 rounded border border-[var(--ink)]/40">
                                id: {m.upstreamModelId}
                              </span>
                            </div>
                            <span className="text-xs font-mono text-[var(--ink)]/70">
                              Context: {m.contextWindow?.toLocaleString() ?? 'unknown'} tokens • Max Output: {m.maxOutputTokens ?? 'unknown'}
                            </span>
                          </div>

                          {/* Capabilities badges & Delete button */}
                          <div className="flex flex-wrap items-center gap-2">
                            <div className="flex flex-wrap items-center gap-1">
                              {m.capabilities.text && <SketchBadge variant="default">Text</SketchBadge>}
                              {m.capabilities.vision && <SketchBadge variant="blue">Vision</SketchBadge>}
                              {m.capabilities.reasoning && <SketchBadge variant="yellow">Reasoning</SketchBadge>}
                              {m.capabilities.toolCalling && <SketchBadge variant="green">Tools</SketchBadge>}
                              {m.capabilities.structuredOutput && <SketchBadge variant="blue">Structured</SketchBadge>}
                            </div>

                            {confirmDeleteModelId === m.id ? (
                              <div className="flex items-center gap-1 bg-[var(--tint-red)] px-2 py-1 border border-[var(--marker-red)] rounded text-xs font-heading">
                                <span className="text-[var(--danger-text)] font-bold">Remove model?</span>
                                <button
                                  onClick={() => {
                                    onDeleteModel(m.id);
                                    setConfirmDeleteModelId(null);
                                  }}
                                  className="px-2 py-0.5 bg-[var(--marker-red)] text-[var(--surface)] rounded font-bold hover:brightness-90 cursor-pointer"
                                >
                                  Delete
                                </button>
                                <button
                                  onClick={() => setConfirmDeleteModelId(null)}
                                  className="px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded hover:bg-[var(--erased)] cursor-pointer"
                                >
                                  Cancel
                                </button>
                              </div>
                            ) : (
                              <div className="flex items-center gap-1">
                                <button
                                  onClick={() => openEditModel(m)}
                                  className="px-2 py-1 text-xs font-heading font-bold text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-[var(--pen-blue)]/40 hover:border-[var(--pen-blue)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                                  title="Edit model configuration"
                                >
                                  <Pencil className="w-3.5 h-3.5" />
                                  <span>Edit</span>
                                </button>
                                <button
                                  onClick={() => setConfirmDeleteModelId(m.id)}
                                  className="px-2 py-1 text-xs font-heading font-bold text-[var(--marker-red)] hover:bg-[var(--tint-red)] border border-[var(--marker-red)]/40 hover:border-[var(--marker-red)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                                  title="Remove model from provider"
                                >
                                  <Trash2 className="w-3.5 h-3.5" />
                                  <span>Remove</span>
                                </button>
                              </div>
                            )}
                          </div>
                        </div>

                        {modelReconciliation(m) && (
                          <div className="mb-3 p-3 bg-[var(--erased-soft)] border border-[var(--ink)]/40 rounded text-xs font-mono">
                            <div className="flex flex-wrap items-center justify-between gap-2">
                              <div className="flex flex-wrap items-center gap-2">
                                <strong className="font-heading text-sm">Reconciliation</strong>
                                <SketchBadge
                                  variant={
                                    modelReconciliation(m)?.status === 'changed' ||
                                    modelReconciliation(m)?.status === 'missing' ||
                                    modelReconciliation(m)?.status === 'deprecated'
                                      ? 'yellow'
                                      : modelReconciliation(m)?.status === 'unchanged' ||
                                          modelReconciliation(m)?.status === 'accepted'
                                        ? 'green'
                                        : 'default'
                                  }
                                >
                                  {modelReconciliation(m)?.status || 'unknown'}
                                </SketchBadge>
                                <span>
                                  {modelReconciliation(m)?.diff?.length || 0} field(s) changed
                                </span>
                                {modelReconciliation(m)?.last_success_at && (
                                  <span className="text-[var(--ink)]/60">
                                    last success {new Date(modelReconciliation(m)!.last_success_at!).toLocaleString()}
                                  </span>
                                )}
                              </div>
                              {!!modelReconciliation(m)?.diff?.length && (
                                <div className="flex flex-wrap gap-1">
                                  <button
                                    type="button"
                                    onClick={() =>
                                      setReconciliationSelections((current) => ({
                                        ...current,
                                        [m.id]: modelReconciliation(m)?.diff?.map((diff) => diff.field || '').filter(Boolean) || [],
                                      }))
                                    }
                                    className="px-2 py-1 border border-[var(--ink)] rounded"
                                  >
                                    Select all
                                  </button>
                                  <button
                                    type="button"
                                    disabled={
                                      lifecycleBusy === `accept:${m.id}`
                                      || !(reconciliationSelections[m.id]?.length)
                                    }
                                    onClick={() =>
                                      handleReconciliationAction(
                                        m.id,
                                        'accept',
                                        reconciliationSelections[m.id] || [],
                                      )
                                    }
                                    className="px-2 py-1 border border-[var(--pen-green)] text-[var(--pen-green)] rounded font-bold disabled:opacity-40"
                                  >
                                    Accept selected
                                  </button>
                                  <button
                                    type="button"
                                    disabled={lifecycleBusy === `ignore:${m.id}`}
                                    onClick={() => handleReconciliationAction(m.id, 'ignore')}
                                    className="px-2 py-1 border border-[var(--ink)] rounded font-bold"
                                  >
                                    Ignore
                                  </button>
                                  <button
                                    type="button"
                                    disabled={
                                      lifecycleBusy === `pin:${m.id}`
                                      || !(reconciliationSelections[m.id]?.length)
                                    }
                                    onClick={() =>
                                      handleReconciliationAction(
                                        m.id,
                                        'pin',
                                        reconciliationSelections[m.id] || [],
                                      )
                                    }
                                    className="px-2 py-1 border border-[var(--pen-blue)] text-[var(--pen-blue)] rounded font-bold disabled:opacity-40"
                                  >
                                    Pin selected
                                  </button>
                                </div>
                              )}
                            </div>
                            {!!modelReconciliation(m)?.diff?.length && (
                              <div className="mt-3 space-y-2">
                                {modelReconciliation(m)?.diff?.map((diff) => {
                                  const selected = reconciliationSelections[m.id]?.includes(diff.field || '') || false;
                                  return (
                                    <label
                                      key={diff.field}
                                      className="block p-2 bg-[var(--surface)] border border-[var(--ink)]/30 rounded cursor-pointer"
                                    >
                                      <div className="flex items-center gap-2 font-bold">
                                        <input
                                          type="checkbox"
                                          checked={selected}
                                          onChange={() =>
                                            setReconciliationSelections((current) => {
                                              const existing = current[m.id] || [];
                                              const field = diff.field || '';
                                              const next = existing.includes(field)
                                                ? existing.filter((candidate) => candidate !== field)
                                                : [...existing, field];
                                              return { ...current, [m.id]: next };
                                            })
                                          }
                                        />
                                        <span>Δ {diff.field}</span>
                                      </div>
                                      <div className="mt-1 grid grid-cols-1 lg:grid-cols-3 gap-2 text-[var(--ink)]/70">
                                        <div>
                                          <strong>Configured</strong>
                                          <pre className="whitespace-pre-wrap break-all">{formatDriftValue(diff.configured)}</pre>
                                        </div>
                                        <div>
                                          <strong>Observed</strong>
                                          <pre className="whitespace-pre-wrap break-all">{formatDriftValue(diff.observed)}</pre>
                                        </div>
                                        <div>
                                          <strong>Source</strong>
                                          <pre className="whitespace-pre-wrap break-all">{formatDriftValue(diff.source)}</pre>
                                        </div>
                                      </div>
                                    </label>
                                  );
                                })}
                              </div>
                            )}
                          </div>
                        )}

                        <div className="mb-3 p-3 bg-[var(--paper)] border border-[var(--ink)]/40 rounded text-xs font-mono">
                          <div className="flex flex-wrap items-center justify-between gap-2">
                            <div>
                              <strong className="font-heading text-sm">Verified capability probes</strong>
                              <span className="ml-2 text-[var(--ink)]/60">
                                transport {probeEvidenceSummary(m, 'transport', selectedProbeAccountId(m.providerId))} ·
                                reasoning efforts {
                                  Object.keys(modelProbeEvidence(m))
                                    .filter((key) => key.startsWith('reasoning_effort_'))
                                    .map((key) =>
                                      `${key.replace('reasoning_effort_', '')}:${probeEvidenceSummary(
                                        m,
                                        key,
                                        selectedProbeAccountId(m.providerId),
                                      )}`,
                                    )
                                    .join(', ') || '—'
                                } ·
                                tools {probeEvidenceSummary(m, 'tool_calling', selectedProbeAccountId(m.providerId))} ·
                                structured {probeEvidenceSummary(m, 'structured_output', selectedProbeAccountId(m.providerId))}
                              </span>
                            </div>
                            <div className="flex flex-wrap items-center gap-2 text-[var(--ink)]/70">
                              <label className="flex items-center gap-1">
                                Probe account
                                <select
                                  value={selectedProbeAccountId(m.providerId)}
                                  onChange={(event) =>
                                    setProbeAccountByProvider((current) => ({
                                      ...current,
                                      [m.providerId]: event.target.value,
                                    }))
                                  }
                                  className="bg-[var(--surface)] border border-[var(--ink)] px-2 py-1 rounded"
                                >
                                  {probeAccountsForProvider(m.providerId).map((account) => (
                                    <option key={account.id} value={account.id}>
                                      {account.label} · {account.status}
                                    </option>
                                  ))}
                                </select>
                              </label>
                              <span>
                                Target: {activeProvider.name} / {selectedProbeAccount(m.providerId)?.label || 'no account'} / {m.upstreamModelId} / {modelProbeTransport(m, activeProvider)}
                              </span>
                            </div>
                            <div className="flex flex-wrap gap-1">
                              {(['transport', 'tool_calling', 'structured_output'] as const).map((capability) => (
                                <button
                                  key={capability}
                                  type="button"
                                  disabled={
                                    lifecycleBusy === `probe:${m.id}:${capability}`
                                    || !selectedProbeAccountId(m.providerId)
                                  }
                                  onClick={() => handleProbeModel(m, capability)}
                                  className="px-2 py-1 border border-[var(--ink)] rounded hover:bg-[var(--erased)]"
                                >
                                  Probe {capability.replace('_', ' ')}
                                  {probeStatus[`${m.id}:${capability}`]
                                    ? ` · ${probeStatus[`${m.id}:${capability}`]}`
                                    : ''}
                                </button>
                              ))}
                              {modelReasoningLevels(m).map((level) => {
                                const key = `${m.id}:reasoning:${level}`;
                                return (
                                  <button
                                    key={key}
                                    type="button"
                                    disabled={
                                      lifecycleBusy === `probe:${key}`
                                      || !selectedProbeAccountId(m.providerId)
                                    }
                                    onClick={() => handleProbeModel(m, 'reasoning', level)}
                                    className="px-2 py-1 border border-[var(--ink)] rounded hover:bg-[var(--erased)]"
                                  >
                                    Probe reasoning {level}
                                    {probeStatus[key] ? ` · ${probeStatus[key]}` : ''}
                                  </button>
                                );
                              })}
                              {modelCanProbeReasoningDisable(m) && (
                                <button
                                  type="button"
                                  disabled={
                                    lifecycleBusy === `probe:${m.id}:reasoning_disable:off`
                                    || !selectedProbeAccountId(m.providerId)
                                  }
                                  onClick={() => handleProbeModel(m, 'reasoning_disable', 'off')}
                                  className="px-2 py-1 border border-[var(--ink)] rounded hover:bg-[var(--erased)]"
                                >
                                  Probe reasoning off
                                  {probeStatus[`${m.id}:reasoning_disable:off`]
                                    ? ` · ${probeStatus[`${m.id}:reasoning_disable:off`]}`
                                    : ''}
                                </button>
                              )}
                            </div>
                          </div>
                        </div>

                        {/* Prices & Parameter policies */}
                        <div className="grid grid-cols-1 md:grid-cols-2 gap-3 text-xs font-mono">
                          <div className="bg-[var(--paper)] p-2 border border-[var(--ink)] rounded">
                            <strong className="font-heading text-sm text-[var(--ink)] block mb-1">
                              💵 Token Pricing · {modelPricingSource(m)}
                            </strong>
                            <div>Input: {m.prices.inputPer1M == null ? 'unknown' : `${m.prices.inputPer1M} / 1M`}</div>
                            <div>Output: {m.prices.outputPer1M == null ? 'unknown' : `${m.prices.outputPer1M} / 1M`}</div>
                            <div>Cache read: {m.prices.cachedPer1M == null ? 'unknown' : `${m.prices.cachedPer1M} / 1M`}</div>
                            <div>Cache write: {m.prices.cacheWritePer1M == null ? 'unknown' : `${m.prices.cacheWritePer1M} / 1M`}</div>
                            {m.capabilities.reasoning && (
                              <div>Thinking: {m.prices.thinkingPer1M == null ? 'output-rate fallback' : `${m.prices.thinkingPer1M} / 1M`}</div>
                            )}
                          </div>

                          <div className="bg-[var(--paper)] p-2 border border-[var(--ink)] rounded">
                            <strong className="font-heading text-sm text-[var(--ink)] block mb-1">
                              ⚙️ Parameter & Thinking Controls
                            </strong>
                            <div>Temperature Policy: <strong>Clamp (0.0 - 2.0)</strong></div>
                            {pluginManagedReasoning(m) ? (
                              <>
                                <div>
                                  Reasoning: <strong className="text-[var(--pen-blue)]">Plugin-managed</strong>
                                  {pluginManagedReasoning(m)?.variant?.fixed
                                    ? ` · ${pluginManagedReasoning(m)?.variant?.reasoning_level || pluginManagedReasoning(m)?.variant?.id || ''}`
                                    : ''}
                                </div>
                                <div>
                                  Supported:{' '}
                                  <strong className="text-[var(--pen-blue)]">
                                    {pluginManagedReasoning(m)?.capability?.levels?.join(' / ') || 'provider-defined'}
                                  </strong>
                                </div>
                              </>
                            ) : (
                              <>
                                <div>
                                  Thinking Levels:{' '}
                                  <strong className="text-[var(--pen-blue)]">
                                    {Object.keys(m.thinkingMap.levels).sort().join(', ') || 'none'}
                                  </strong>
                                </div>
                                <div className="truncate">
                                  Budget Field: <code>{m.thinkingMap.budgetField || '—'}</code>
                                </div>
                              </>
                            )}
                          </div>
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </WobblyCard>
          </div>
        )}
      </div>
    )}

      {/* Add Provider Modal */}
      {showAddProviderModal && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-lg">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setShowAddProviderModal(false)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>

              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
                <Server className="w-6 h-6 text-[var(--pen-blue)]" />
                {editingProviderId ? 'Edit Upstream Provider' : 'Add Upstream Provider (No Presets)'}
              </h3>

              <form onSubmit={handleCreateProvider} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Provider Name
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. Google Gemini, Mistral, Local vLLM"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Endpoint Base URL
                  </label>
                  <input
                    type="url"
                    required
                    placeholder="https://api.openai.com/v1 or custom host"
                    value={baseUrl}
                    onChange={(e) => setBaseUrl(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Wire Format
                    </label>
                    <select
                      value={wireFormat}
                      onChange={(e) => setWireFormat(e.target.value as any)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none font-mono"
                      style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                    >
                      <option value="gemini">Gemini API</option>
                      <option value="openai">OpenAI Compatible</option>
                      <option value="anthropic">Anthropic Messages</option>
                      <option value="plugin">Plugin adapter</option>
                    </select>
                  </div>

                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Auth Scheme
                    </label>
                    <select
                      value={authScheme}
                      onChange={(e) => setAuthScheme(e.target.value as any)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none font-mono"
                      style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
                    >
                      <option value="bearer">Bearer Header (Authorization)</option>
                      <option value="custom_header">Custom Header (e.g. x-api-key)</option>
                      <option value="query_param">Query Param (?key=...)</option>
                    </select>
                  </div>
                </div>

                {authScheme === 'custom_header' && (
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Custom Header Name
                    </label>
                    <input
                      type="text"
                      placeholder="e.g. x-api-key"
                      value={customHeader}
                      onChange={(e) => setCustomHeader(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    />
                  </div>
                )}

                {authScheme === 'query_param' && (
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Custom Query Parameter Name
                    </label>
                    <input
                      type="text"
                      placeholder="e.g. key"
                      value={customParam}
                      onChange={(e) => setCustomParam(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    />
                  </div>
                )}

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Models Path
                    </label>
                    <input
                      type="text"
                      placeholder="/models"
                      value={modelsPath}
                      onChange={(e) => setModelsPath(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    />
                  </div>
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Extra Headers
                    </label>
                    <textarea
                      rows={2}
                      placeholder={'anthropic-version: 2023-06-01'}
                      value={extraHeaders}
                      onChange={(e) => setExtraHeaders(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-sm font-mono sketch-shadow-sm focus:outline-none"
                    />
                  </div>
                </div>

                {/* Credential — needed for authenticated model discovery, and to
                    create the provider's first account (FR-10.11). */}
                <div
                  className="p-3 bg-[var(--postit)]/60 border-2 border-dashed border-[var(--ink)]/40"
                  style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                >
                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                        API Key {editingProviderId ? '(leave blank to keep)' : '(optional)'}
                      </label>
                      <input
                        type="password"
                        autoComplete="off"
                        placeholder={editingProviderId ? '•••••• (unchanged)' : 'sk-... or provider key'}
                        value={apiKey}
                        onChange={(e) => setApiKey(e.target.value)}
                        className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                      />
                    </div>
                    <div>
                      <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                        Account Label
                      </label>
                      <input
                        type="text"
                        placeholder="e.g. Primary key"
                        value={accountLabel}
                        onChange={(e) => setAccountLabel(e.target.value)}
                        className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                      />
                    </div>
                  </div>
                  <p className="text-xs font-body text-[var(--ink)]/70 mt-2">
                    Stored encrypted at rest. Required to fetch an authenticated upstream model list
                    and to create the first account for this provider.
                  </p>
                </div>

                <details className="text-sm font-body">
                  <summary className="cursor-pointer font-heading font-bold text-[var(--pen-blue)]">
                    Advanced (security & timeout)
                  </summary>
                  <div className="grid grid-cols-2 gap-3 mt-3">                    <div>
                      <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                        Timeout (ms)
                      </label>
                      <input
                        type="number"
                        min={1000}
                        value={timeoutMs}
                        onChange={(e) => setTimeoutMs(Number(e.target.value) || 120000)}
                        className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                      />
                    </div>
                    <div>
                      <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                        Capability Mode
                      </label>
                      <select
                        value={capabilityMode}
                        onChange={(e) => setCapabilityMode(e.target.value as any)}
                        className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                      >
                        <option value="permissive">Permissive (never reject on caps)</option>
                        <option value="strict">Strict (reject unmet caps)</option>
                      </select>
                    </div>
                    <div className="col-span-2 p-3 border-2 border-dashed border-[var(--ink)]/30 bg-[var(--surface)]/70">
                      <div className="font-heading font-bold text-sm mb-2">Plugin bindings (optional)</div>
                      <p className="text-xs font-body text-[var(--ink)]/65 mb-3">
                        Bind this provider to capabilities from an enabled plugin. Use the explicit{' '}
                        <code>plugin:&lt;id&gt;/&lt;capability&gt;</code> reference shown on the Plugins page.
                      </p>
                      <div className="space-y-2">
                        <label className="block">
                          <span className="block text-xs font-heading font-bold mb-1">Wire adapter</span>
                          <input
                            type="text"
                            placeholder="plugin:dev.example.foo/foo-wire"
                            value={wirePlugin}
                            onChange={(e) => setWirePlugin(e.target.value)}
                            className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-sm font-mono sketch-shadow-sm focus:outline-none"
                          />
                        </label>
                        <label className="block">
                          <span className="block text-xs font-heading font-bold mb-1">Credential strategy</span>
                          <input
                            type="text"
                            placeholder="plugin:dev.example.foo/foo-oauth"
                            value={credentialPlugin}
                            onChange={(e) => setCredentialPlugin(e.target.value)}
                            className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-sm font-mono sketch-shadow-sm focus:outline-none"
                          />
                        </label>
                        <label className="block">
                          <span className="block text-xs font-heading font-bold mb-1">Model source</span>
                          <input
                            type="text"
                            placeholder="plugin:dev.example.foo/foo-models"
                            value={modelSourcePlugin}
                            onChange={(e) => setModelSourcePlugin(e.target.value)}
                            className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-sm font-mono sketch-shadow-sm focus:outline-none"
                          />
                        </label>
                      </div>
                    </div>
                    <div className="col-span-2">
                      <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                        Credential Host Binding (comma-separated, optional)
                      </label>
                      <input
                        type="text"
                        placeholder="e.g. api.example.com, uploads.example.com"
                        value={credentialHosts}
                        onChange={(e) => setCredentialHosts(e.target.value)}
                        className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                      />
                    </div>
                    <label className="flex items-center gap-2 font-body text-sm">
                      <input
                        type="checkbox"
                        checked={followRedirects}
                        onChange={(e) => setFollowRedirects(e.target.checked)}
                      />
                      Follow redirects (default off)
                    </label>
                    <label className="flex items-center gap-2 font-body text-sm">
                      <input
                        type="checkbox"
                        checked={allowInsecureTls}
                        onChange={(e) => setAllowInsecureTls(e.target.checked)}
                      />
                      Allow plain-HTTP (dev only)
                    </label>
                  </div>
                </details>

                {validation && (
                  <div
                    className="p-3 text-sm font-mono"
                    style={{
                      borderRadius: DESIGN_TOKENS.radii.wobbly,
                      background: validation.valid ? 'var(--tint-green)' : 'var(--tint-red)',
                      border: `2px solid ${validation.valid ? 'var(--pen-green)' : 'var(--marker-red)'}`,
                    }}
                  >
                    <div className="font-bold mb-1">
                      {validation.valid ? 'Validate: passed' : 'Validate: problems found'}
                    </div>
                    {validation.problems.map((p, i) => (
                      <div key={i} style={{ color: 'var(--danger-text)' }}>
                        • {p}
                      </div>
                    ))}
                    {validation.warnings.map((w, i) => (
                      <div key={i} style={{ color: 'var(--marker-orange)' }}>
                        ⚠ {w}
                      </div>
                    ))}
                  </div>
                )}

                <div className="pt-2 flex justify-end gap-3">
                  <SketchButton
                    type="button"
                    variant="ghost"
                    onClick={() => setShowAddProviderModal(false)}
                  >
                    Cancel
                  </SketchButton>
                  <SketchButton
                    type="button"
                    variant="secondary"
                    onClick={handleValidateProvider}
                    disabled={validating || !name.trim() || !baseUrl.trim()}
                  >
                    {validating ? 'Validating…' : 'Validate (Dry Run)'}
                  </SketchButton>
                  <SketchButton type="submit" variant="danger" className="font-bold" disabled={isSaving}>
                    {isSaving ? 'Saving…' : editingProviderId ? 'Save Changes' : 'Save Provider'}
                  </SketchButton>
                </div>
              </form>
            </WobblyCard>
          </div>
        </div>
      )}

      {/* Add Model Modal */}
      {showAddModelModal && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-lg max-h-[90vh] overflow-y-auto">
            <WobblyCard decoration="tack" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setShowAddModelModal(false)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>

              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-1 flex items-center gap-2">
                <Cpu className="w-6 h-6 text-[var(--marker-red)]" />
                {editingModelId ? `Edit Model for ${activeProvider.name}` : `Configure Model for ${activeProvider.name}`}
              </h3>
              <p className="text-sm font-body text-[var(--ink)]/80 mb-4">
                Define the model identifier, token capabilities, and per-million token pricing.
              </p>

              <form onSubmit={handleCreateCustomModel} className="space-y-4">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Upstream Model ID (Wire Name)
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. gemini-2.5-flash, claude-3-7-sonnet, gpt-4o"
                    value={modelUpstreamId}
                    onChange={(e) => setModelUpstreamId(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Execution Transport Override
                  </label>
                  <input
                    type="text"
                    placeholder="leave blank for discovery/provider default"
                    value={modelTransportOverride}
                    onChange={(e) => setModelTransportOverride(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                  <p className="text-xs font-body text-[var(--ink)]/60 mt-1">
                    Use openai, openai-responses, anthropic, gemini, or a plugin:&lt;id&gt;/&lt;adapter&gt; reference. Blank keeps discovery/provider defaults.
                  </p>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Display Name
                  </label>
                  <input
                    type="text"
                    placeholder="e.g. Gemini 2.5 Flash (Production)"
                    value={modelDisplayName}
                    onChange={(e) => setModelDisplayName(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Context Window
                    </label>
                    <input
                      type="number"
                      min={0}
                      step={1000}
                      placeholder={`${DEFAULT_CONTEXT_WINDOW} (default)`}
                      value={modelContextWindow}
                      onChange={(e) => setModelContextWindow(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    />
                  </div>

                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Max Output
                    </label>
                    <input
                      type="number"
                      min={0}
                      placeholder={`${DEFAULT_MAX_OUTPUT} (default)`}
                      value={modelMaxOutput}
                      onChange={(e) => setModelMaxOutput(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    />
                  </div>
                </div>

                <p className="text-xs font-body text-[var(--ink)]/60">
                  New manual models use {DEFAULT_CONTEXT_WINDOW.toLocaleString()} context ·{' '}
                  {DEFAULT_MAX_OUTPUT.toLocaleString()} max output when blank. Imported models keep blank values unknown.
                </p>

                {/* Token Pricing */}
                <div className="grid grid-cols-2 gap-3 bg-[var(--erased-soft)] p-3 border border-[var(--ink)] rounded">
                  {([
                    ['Input Price ($ / 1M)', modelInputPrice, setModelInputPrice],
                    ['Output Price ($ / 1M)', modelOutputPrice, setModelOutputPrice],
                    ['Cache Read ($ / 1M)', modelCachedPrice, setModelCachedPrice],
                    ['Cache Write ($ / 1M)', modelCacheWritePrice, setModelCacheWritePrice],
                    ['Thinking ($ / 1M)', modelThinkingPrice, setModelThinkingPrice],
                  ] as const).map(([label, value, setter]) => (
                    <div key={String(label)}>
                      <label className="block text-xs font-heading font-bold text-[var(--ink)] mb-1">
                        {String(label)}
                      </label>
                      <input
                        type="number"
                        step="any"
                        min={0}
                        value={value ?? ''}
                        placeholder="unknown"
                        onChange={(e) =>
                          (setter as React.Dispatch<React.SetStateAction<number | null>>)(
                            e.target.value === '' ? null : Number(e.target.value),
                          )
                        }
                        className="w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1 text-sm font-mono focus:outline-none rounded"
                      />
                    </div>
                  ))}
                </div>

                {/* Capabilities */}
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-2">
                    Model Capabilities
                  </label>
                  <div className="grid grid-cols-2 gap-2 text-sm font-body">
                    <label className="flex items-center gap-2 cursor-pointer">
                      <input
                        type="checkbox"
                        checked={capText ?? false}
                        onChange={(e) => setCapText(e.target.checked)}
                        className="w-4 h-4 accent-[var(--marker-red)]"
                      />
                      <span>Text Generation</span>
                    </label>
                    <label className="flex items-center gap-2 cursor-pointer">
                      <input
                        type="checkbox"
                        checked={capVision ?? false}
                        onChange={(e) => setCapVision(e.target.checked)}
                        className="w-4 h-4 accent-[var(--marker-red)]"
                      />
                      <span>Vision / Multimodal</span>
                    </label>
                    <label className="flex items-center gap-2 cursor-pointer">
                      <input
                        type="checkbox"
                        checked={capReasoning ?? false}
                        onChange={(e) => setCapReasoning(e.target.checked)}
                        className="w-4 h-4 accent-[var(--marker-red)]"
                      />
                      <span>Reasoning / Thinking</span>
                    </label>
                    <label className="flex items-center gap-2 cursor-pointer">
                      <input
                        type="checkbox"
                        checked={capTools ?? false}
                        onChange={(e) => setCapTools(e.target.checked)}
                        className="w-4 h-4 accent-[var(--marker-red)]"
                      />
                      <span>Tool Calling</span>
                    </label>
                    <label className="flex items-center gap-2 cursor-pointer">
                      <input
                        type="checkbox"
                        checked={capStructuredOutput ?? false}
                        onChange={(e) => setCapStructuredOutput(e.target.checked)}
                        className="w-4 h-4 accent-[var(--marker-red)]"
                      />
                      <span>Structured Output / JSON</span>
                    </label>
                  </div>
                </div>

                {capReasoning && (
                  <details className="bg-[var(--erased-soft)] p-3 border border-[var(--ink)] rounded">
                    <summary className="cursor-pointer text-sm font-heading font-bold text-[var(--ink)]">
                      Advanced / Override reasoning mapping
                    </summary>
                    <div className="space-y-3 mt-3">
                    <div>
                      <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                        Canonical Thinking Map
                      </label>
                      <p className="text-xs font-body text-[var(--ink)]/70">
                        Configure canonical levels as JSON objects, or scalar values sent under the field for the selected reasoning mode.
                      </p>
                    </div>
                    <div>
                      <label className="block text-xs font-heading font-bold text-[var(--ink)] mb-1">
                        Thinking Mode
                      </label>
                      <select
                        value={modelThinkingMode}
                        onChange={(e) => setModelThinkingMode(e.target.value as '' | 'manual_budget' | 'level' | 'adaptive')}
                        className="w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1.5 text-sm font-mono focus:outline-none rounded"
                      >
                        <option value="">Legacy / inferred</option>
                        <option value="manual_budget">Manual budget</option>
                        <option value="level">Level / effort</option>
                        <option value="adaptive">Adaptive thinking</option>
                      </select>
                    </div>
                    <div className="grid grid-cols-1 gap-2">
                      {thinkingLevelInputs.map(([level, value, setter]) => (
                        <div key={level}>
                          <label className="block text-xs font-heading font-bold text-[var(--ink)] mb-1 capitalize">
                            {level}
                          </label>
                          <input
                            type="text"
                            value={value}
                            onChange={(e) => setter(e.target.value)}
                            placeholder={`{"reasoning_effort":"${level}"} or numeric budget`}
                            className="w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1.5 text-sm font-mono focus:outline-none rounded"
                          />
                        </div>
                      ))}
                    </div>
                    {modelThinkingMode === 'level' || modelThinkingMode === 'adaptive' ? (
                      <div>
                        <label className="block text-xs font-heading font-bold text-[var(--ink)] mb-1">
                          Level Field
                        </label>
                        <input
                          type="text"
                          value={modelThinkingLevelField}
                          onChange={(e) => setModelThinkingLevelField(e.target.value)}
                          placeholder="e.g. output_config.effort"
                          className="w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1.5 text-sm font-mono focus:outline-none rounded"
                        />
                        <p className="text-xs font-body text-[var(--ink)]/60 mt-1">
                          Scalar adaptive levels are written here; Anthropic adaptive thinking never emits budget_tokens.
                        </p>
                      </div>
                    ) : (
                      <div>
                        <label className="block text-xs font-heading font-bold text-[var(--ink)] mb-1">
                          Budget Field (optional)
                        </label>
                        <input
                          type="text"
                          value={modelThinkingBudgetField}
                          onChange={(e) => setModelThinkingBudgetField(e.target.value)}
                          placeholder="e.g. thinking.budget_tokens"
                          className="w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1.5 text-sm font-mono focus:outline-none rounded"
                        />
                        <p className="text-xs font-body text-[var(--ink)]/60 mt-1">
                          Used when a legacy/manual level mapping is a scalar. Object mappings can use dotted field paths directly.
                        </p>
                      </div>
                    )}
                    </div>
                  </details>
                )}

                <div className="pt-2 flex justify-end gap-3">
                  <SketchButton
                    type="button"
                    variant="ghost"
                    onClick={() => setShowAddModelModal(false)}
                  >
                    Cancel
                  </SketchButton>
                  <SketchButton
                    type="button"
                    variant="secondary"
                    onClick={handleValidateModel}
                    disabled={validatingModel || !modelUpstreamId.trim()}
                  >
                    {validatingModel ? 'Validating…' : 'Validate (Dry Run)'}
                  </SketchButton>
                  <SketchButton type="submit" variant="primary" className="font-bold">
                    {editingModelId ? 'Save Changes' : 'Save Model Configuration'}
                  </SketchButton>
                </div>
                {modelValidation && (
                  <div
                    className="mt-3 p-3 text-sm font-mono"
                    style={{
                      borderRadius: DESIGN_TOKENS.radii.wobbly,
                      background: modelValidation.valid ? 'var(--tint-green)' : 'var(--tint-red)',
                      border: `2px solid ${modelValidation.valid ? 'var(--pen-green)' : 'var(--marker-red)'}`,
                    }}
                  >
                    <div className="font-bold mb-1">
                      {modelValidation.valid ? 'Validate: passed' : 'Validate: problems found'}
                    </div>
                    {modelValidation.problems.map((p, i) => (
                      <div key={i} style={{ color: 'var(--danger-text)' }}>• {p}</div>
                    ))}
                    {modelValidation.warnings.map((w, i) => (
                      <div key={i} style={{ color: 'var(--marker-orange)' }}>⚠ {w}</div>
                    ))}
                  </div>
                )}
              </form>
            </WobblyCard>
          </div>
        </div>
      )}
    </div>
  );
};
