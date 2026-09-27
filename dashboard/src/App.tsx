/**
 * @license
 * SPDX-License-Identifier: Apache-2.0
 *
 * Kinetix admin dashboard. All data is loaded from the Kinetix admin API
 * (/admin/api/*); there is no mock data. The app is served from the same origin
 * as the API, so the admin session cookie is sent automatically.
 */

import React, { useCallback, useEffect, useState } from 'react';
import { Sidebar, TopBar, NavTab, TAB_ROUTES } from './components/Navbar';
import { LiveTesterModal } from './components/LiveTesterModal';
import { LoginScreen } from './components/LoginScreen';
import { KeysView } from './components/views/KeysView';
import { RoutesView } from './components/views/RoutesView';
import { ProvidersView } from './components/views/ProvidersView';
import { AccountsView } from './components/views/AccountsView';
import { UsageView } from './components/views/UsageView';
import { RequestsView } from './components/views/RequestsView';
import { HealthView } from './components/views/HealthView';
import { LiveRequest } from './types';
import { AliasesView } from './components/views/AliasesView';
import { AuditView } from './components/views/AuditView';
import { SquiggleDivider, SketchButton, SketchBadge } from './components/HandDrawnElements';
import { EMPTY_METRICS } from './lib/mappers';
import { Kinetix, ExportFile, UsageDay } from './lib/resources';
import { SettingsView } from './components/views/SettingsView';
import { PluginsView } from './components/views/PluginsView';
import { ApiError } from './lib/api';
import { useTheme } from './lib/theme';
import {
  VirtualKey,
  Route,
  Provider,
  Account,
  ModelConfig,
  ModelAlias,
  AuditLog,
  RequestLog,
  ProxyMetrics,
} from './types';
import { Play, AlertTriangle } from 'lucide-react';

type AuthState = 'checking' | 'signed-out' | 'signed-in';

function getTabFromPath(path: string): NavTab {
  const normalized = path.replace(/\/$/, '');
  const entries = Object.entries(TAB_ROUTES) as [NavTab, string][];
  for (const [tab, route] of entries) {
    if (normalized === route || normalized === route.replace('/admin', '')) {
      return tab;
    }
  }
  return 'keys';
}

export default function App() {
  const [auth, setAuth] = useState<AuthState>('checking');
  const [currentUser, setCurrentUser] = useState('admin');
  const [activeTab, setActiveTab] = useState<NavTab>(() =>
    typeof window !== 'undefined' ? getTabFromPath(window.location.pathname) : 'keys',
  );
  const [isTesterOpen, setIsTesterOpen] = useState(false);
  const [navOpen, setNavOpen] = useState(false);
  const { mode: themeMode, setTheme } = useTheme();

  // Reactive data, all sourced from the admin API.
  const [keys, setKeys] = useState<VirtualKey[]>([]);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [models, setModels] = useState<ModelConfig[]>([]);
  const [routes, setRoutes] = useState<Route[]>([]);
  const [aliases, setAliases] = useState<ModelAlias[]>([]);
  const [requests, setRequests] = useState<RequestLog[]>([]);
  const [liveRequests, setLiveRequests] = useState<LiveRequest[]>([]);
  const [exportFiles, setExportFiles] = useState<ExportFile[]>([]);
  const [exportDir, setExportDir] = useState<string>('');
  const [exportRetentionDays, setExportRetentionDays] = useState<number>(30);
  const [exportDays, setExportDays] = useState<UsageDay[]>([]);
  const [auditLogs, setAuditLogs] = useState<AuditLog[]>([]);
  const [metrics, setMetrics] = useState<ProxyMetrics>(EMPTY_METRICS);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(false);

  // ---- session bootstrap --------------------------------------------------
  useEffect(() => {
    Kinetix.me()
      .then((r) => {
        setCurrentUser(r.user || 'admin');
        setAuth('signed-in');
      })
      .catch(() => setAuth('signed-out'));
  }, []);

  // ---- data loading -------------------------------------------------------
  const refresh = useCallback(async () => {
    setIsRefreshing(true);
    try {
      const [k, p, a, m, c, al, req, aud, met, exp] = await Promise.all([
        Kinetix.keys(),
        Kinetix.providers(),
        Kinetix.accounts(),
        Kinetix.models(),
        Kinetix.routes(),
        Kinetix.aliases(),
        Kinetix.requests(),
        Kinetix.audit(),
        Kinetix.overview(),
        Kinetix.exports(),
      ]);
      setKeys(k);
      setProviders(p);
      setAccounts(a);
      setModels(m);
      setRoutes(c);
      setAliases(al);
      setRequests(req);
      setAuditLogs(aud);
      setMetrics(met);
      setExportFiles(exp.files);
      setExportDir(exp.dir);
      setExportRetentionDays(exp.retention_days);
      setExportDays(exp.days);
      setLoadError(null);
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        setAuth('signed-out');
      } else {
        setLoadError(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setIsRefreshing(false);
    }
  }, []);

  useEffect(() => {
    if (auth === 'signed-in') {
      refresh();
    }
  }, [auth, refresh]);

  // Poll overview metrics so the header stays live.
  useEffect(() => {
    if (auth !== 'signed-in') return;
    const id = setInterval(() => {
      Kinetix.overview().then(setMetrics).catch(() => {});
    }, 15000);
    return () => clearInterval(id);
  }, [auth]);

  // Poll the live in-flight view while the Requests tab is open (FR-8.3).
  useEffect(() => {
    if (auth !== 'signed-in' || activeTab !== 'requests') return;
    let cancelled = false;
    const poll = () => {
      Kinetix.liveRequests()
        .then((r) => {
          if (!cancelled) setLiveRequests(r);
        })
        .catch(() => {});
    };
    poll();
    const id = setInterval(poll, 2000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [auth, activeTab]);

  // ---- routing ------------------------------------------------------------
  useEffect(() => {
    const handlePopState = () => setActiveTab(getTabFromPath(window.location.pathname));
    window.addEventListener('popstate', handlePopState);
    return () => window.removeEventListener('popstate', handlePopState);
  }, []);

  const handleSelectTab = (tab: NavTab) => {
    setActiveTab(tab);
    const targetPath = TAB_ROUTES[tab];
    if (window.location.pathname !== targetPath) {
      window.history.pushState({ tab }, '', targetPath);
    }
  };

  // ---- auth ---------------------------------------------------------------
  const handleLoginSuccess = (username: string) => {
    setCurrentUser(username);
    setAuth('signed-in');
  };

  const handleLogout = async () => {
    try {
      await Kinetix.logout();
    } catch {
      // ignore
    }
    setAuth('signed-out');
  };

  // ---- mutations (all server-backed, then refresh) ------------------------
  const withRefresh = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await refresh();
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e));
    }
  };

  // Like withRefresh, but rethrows so a caller (e.g. a modal that stays open on
  // failure) can keep the form on screen instead of silently swallowing the error.
  const withRefreshOrThrow = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await refresh();
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e));
      throw e;
    }
  };

  const handleAddKey = async (
    newKey: VirtualKey,
  ): Promise<{ key: VirtualKey; fullKey: string } | null> => {
    try {
      const created = await Kinetix.createKey({
        name: newKey.name,
        owner: newKey.owner,
        tag: newKey.tag,
        allowed_models: newKey.allowedModels,
        rpm_limit: newKey.rpmLimit || null,
        tpm_limit: newKey.tpmLimit || null,
        daily_budget: newKey.dailyBudget || null,
        monthly_budget: newKey.monthlyBudget || null,
      });
      await refresh();
      return created;
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e));
      return null;
    }
  };

  const handleUpdateKeyStatus = (id: string, status: 'active' | 'disabled' | 'revoked') =>
    withRefresh(() => Kinetix.updateKey(id, { status }));

  const handleUpdateKeyIps = (id: string, ips: string[]) =>
    withRefresh(() => Kinetix.updateKey(id, { allowed_ips: ips }));

  const handleDeleteKey = (id: string) => withRefresh(() => Kinetix.deleteKey(id));

  const handleAddRoute = (newRoute: Route) =>
    withRefresh(() =>
      Kinetix.createRoute({
        name: newRoute.name,
        description: newRoute.description,
        strategy: newRoute.selectionStrategy,
        fallback_triggers: newRoute.fallbackTriggers,
        portability_policy: newRoute.portabilityPolicy,
        cache_affinity: newRoute.cacheAffinity,
        sticky_routing: newRoute.stickyRouting,
        targets: newRoute.targets.map((t) => ({
          account_id: t.accountId || null,
          model_id: t.modelId,
          priority: t.priority,
          weight: t.weight ?? 1,
        })),
      }),
    );

  const handleUpdateRoute = (updated: Route) =>
    withRefresh(() =>
      Kinetix.updateRoute(updated.id, {
        name: updated.name,
        description: updated.description,
        strategy: updated.selectionStrategy,
        fallback_triggers: updated.fallbackTriggers,
        portability_policy: updated.portabilityPolicy,
        cache_affinity: updated.cacheAffinity,
        sticky_routing: updated.stickyRouting,
        targets: updated.targets.map((t) => ({
          account_id: t.accountId || null,
          model_id: t.modelId,
          priority: t.priority,
          weight: t.weight ?? 1,
        })),
      }),
    );

  const handleDeleteRoute = (routeId: string) => withRefresh(() => Kinetix.deleteRoute(routeId));

  const handleAddProvider = (prov: Provider) =>
    withRefreshOrThrow(() =>
      Kinetix.createProvider({
        name: prov.name,
        base_url: prov.baseUrl,
        wire_format: prov.wireFormat,
        auth_scheme: prov.authScheme,
        custom_header_name: prov.customHeaderName || null,
        custom_param_name: prov.customParamName || null,
        extra_headers: prov.extraHeaders || {},
        models_path: prov.modelsPath || null,
        timeout_ms: prov.timeoutMs,
        capability_mode: prov.capabilityMode,
        credential_hosts: prov.credentialHosts || '',
        follow_redirects: !!prov.followRedirects,
        allow_insecure_tls: !!prov.allowInsecureTls,
        wire_plugin: prov.wirePlugin || '',
        credential_plugin: prov.credentialPlugin || '',
        model_source_plugin: prov.modelSourcePlugin || '',
        api_key: prov.apiKey || null,
        account_label: prov.accountLabel || null,
      }),
    );

  const handleUpdateProvider = (providerId: string, prov: Provider) =>
    withRefreshOrThrow(() =>
      Kinetix.updateProvider(providerId, {
        name: prov.name,
        base_url: prov.baseUrl,
        wire_format: prov.wireFormat,
        auth_scheme: prov.authScheme,
        custom_header_name: prov.customHeaderName || null,
        custom_param_name: prov.customParamName || null,
        extra_headers: prov.extraHeaders || {},
        models_path: prov.modelsPath || null,
        timeout_ms: prov.timeoutMs,
        capability_mode: prov.capabilityMode,
        credential_hosts: prov.credentialHosts || '',
        follow_redirects: !!prov.followRedirects,
        allow_insecure_tls: !!prov.allowInsecureTls,
        wire_plugin: prov.wirePlugin || '',
        credential_plugin: prov.credentialPlugin || '',
        model_source_plugin: prov.modelSourcePlugin || '',
        api_key: prov.apiKey || null,
        account_label: prov.accountLabel || null,
      }),
    );

  const handleDeleteProvider = (providerId: string) =>
    withRefresh(() => Kinetix.deleteProvider(providerId));

  const modelCapabilitiesPayload = (model: ModelConfig): Record<string, boolean> => {
    const out: Record<string, boolean> = {};
    const fields: [string, boolean | undefined][] = [
      ['text', model.capabilities.text],
      ['vision', model.capabilities.vision],
      ['reasoning', model.capabilities.reasoning],
      ['tool_calling', model.capabilities.toolCalling],
      ['audio', model.capabilities.audio],
      ['structured_output', model.capabilities.structuredOutput],
    ];
    for (const [key, value] of fields) {
      if (typeof value === 'boolean') out[key] = value;
    }
    return out;
  };

  const handleAddModel = (model: ModelConfig) =>
    withRefresh(() =>
      Kinetix.createModel(model.providerId, {
        upstream_id: model.upstreamModelId,
        display_name: model.displayName,
        transport_override: model.transportOverride ?? null,
        enabled: model.enabled,
        context_window: model.contextWindow,
        max_output_tokens: model.maxOutputTokens,
        capabilities: modelCapabilitiesPayload(model),
        prices: {
          input_per_1m: model.prices.inputPer1M,
          output_per_1m: model.prices.outputPer1M,
          cached_per_1m: model.prices.cachedPer1M,
          cache_write_per_1m: model.prices.cacheWritePer1M,
          thinking_per_1m: model.prices.thinkingPer1M,
        },
        parameters: model.parameters,
        thinking_map: {
          levels: model.thinkingMap.levels,
          mode: model.thinkingMap.mode || null,
          budget_field: model.thinkingMap.budgetField || null,
          level_field: model.thinkingMap.levelField || null,
        },
        discovery: model.discovery || {},
      }),
    );

  const handleDeleteModel = (modelId: string) => withRefresh(() => Kinetix.deleteModel(modelId));

  const handleUpdateModel = (model: ModelConfig) =>
    withRefresh(() =>
      Kinetix.updateModel(model.id, {
        upstream_id: model.upstreamModelId,
        display_name: model.displayName,
        transport_override: model.transportOverride ?? null,
        enabled: model.enabled,
        context_window: model.contextWindow,
        max_output_tokens: model.maxOutputTokens,
        capabilities: modelCapabilitiesPayload(model),
        prices: {
          input_per_1m: model.prices.inputPer1M,
          output_per_1m: model.prices.outputPer1M,
          cached_per_1m: model.prices.cachedPer1M,
          cache_write_per_1m: model.prices.cacheWritePer1M,
          thinking_per_1m: model.prices.thinkingPer1M,
        },
        parameters: model.parameters,
        thinking_map: {
          levels: model.thinkingMap.levels,
          mode: model.thinkingMap.mode || null,
          budget_field: model.thinkingMap.budgetField || null,
          level_field: model.thinkingMap.levelField || null,
        },
      }),
    );

  const handleAddAccount = (acc: Account & { apiKey?: string }) =>
    withRefresh(() =>
      Kinetix.createAccount({
        provider_id: acc.providerId,
        label: acc.label,
        api_key: acc.apiKey,
        priority: acc.priority,
        weight: acc.weight,
        soft_quota_usd: acc.softQuotaSpendLimit ?? null,
        quota_type: acc.quotaType,
      }),
    );

  const handleUpdateAccount = (acc: Account) =>
    withRefresh(() =>
      Kinetix.updateAccount(acc.id, {
        provider_id: acc.providerId,
        label: acc.label,
        priority: acc.priority,
        weight: acc.weight,
        soft_quota_usd: acc.softQuotaSpendLimit ?? null,
        quota_type: acc.quotaType,
        status: acc.status,
      }),
    );

  const handleDeleteAccount = (accountId: string) =>
    withRefresh(() => Kinetix.deleteAccount(accountId));

  const handleResetAccount = (accountId: string) =>
    withRefresh(() => Kinetix.resetAccount(accountId));

  const handleAddAlias = (alias: ModelAlias) =>
    withRefresh(() =>
      Kinetix.createAlias({
        alias: alias.aliasName,
        target_type: alias.targetType,
        target_id: alias.targetId,
        description: alias.description,
      }),
    );

  const handleDeleteAlias = (id: string) => withRefresh(() => Kinetix.deleteAlias(id));

  // ---- render -------------------------------------------------------------
  if (auth === 'checking') {
    return (
      <div className="min-h-screen bg-[var(--paper)] flex items-center justify-center font-heading text-[var(--ink)]">
        Checking gateway session…
      </div>
    );
  }

  if (auth === 'signed-out') {
    return <LoginScreen onLoginSuccess={handleLoginSuccess} />;
  }

  return (
    <div className="min-h-screen bg-[var(--paper)] text-[var(--ink)] flex selection:bg-[var(--postit)] selection:text-[var(--ink)]">
      <Sidebar
        activeTab={activeTab}
        onSelectTab={handleSelectTab}
        currentUser={currentUser}
        onLogout={handleLogout}
        mobileOpen={navOpen}
        onCloseMobile={() => setNavOpen(false)}
      />

      <div className="flex-1 min-w-0 flex flex-col">
        <TopBar
          activeTab={activeTab}
          metrics={metrics}
          onOpenTester={() => setIsTesterOpen(true)}
          onOpenNav={() => setNavOpen(true)}
          onRefresh={refresh}
          isRefreshing={isRefreshing}
          themeMode={themeMode}
          onThemeChange={setTheme}
        />

        <main className="flex-1 w-full p-4 md:p-8">
        {loadError && (
          <div className="mb-4 p-3 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] rounded-lg text-sm font-mono text-[var(--danger-text)] flex items-center gap-2">
            <AlertTriangle className="w-4 h-4" />
            <span className="flex-1">{loadError}</span>
            <button onClick={() => setLoadError(null)} className="font-bold cursor-pointer">
              ✕
            </button>
          </div>
        )}

        {activeTab === 'keys' && (
          <KeysView
            keys={keys}
            onAddKey={handleAddKey}
            onUpdateKeyStatus={handleUpdateKeyStatus}
            onUpdateKeyIps={handleUpdateKeyIps}
            onDeleteKey={handleDeleteKey}
          />
        )}

        {activeTab === 'routes' && (
          <RoutesView
            routes={routes}
            accounts={accounts}
            models={models}
            allowedProviders={keys[0]?.allowedProviders ?? []}
            onAddRoute={handleAddRoute}
            onUpdateRoute={handleUpdateRoute}
            onDeleteRoute={handleDeleteRoute}
          />
        )}

        {activeTab === 'providers' && (
          <ProvidersView
            providers={providers}
            models={models}
            onAddProvider={handleAddProvider}
            onUpdateProvider={handleUpdateProvider}
            onAddModel={handleAddModel}
            onUpdateModel={handleUpdateModel}
            onDeleteModel={handleDeleteModel}
            onDeleteProvider={handleDeleteProvider}
            onRefresh={refresh}
          />
        )}

        {activeTab === 'accounts' && (
          <AccountsView
            accounts={accounts}
            providers={providers}
            onAddAccount={handleAddAccount}
            onUpdateAccount={handleUpdateAccount}
            onDeleteAccount={handleDeleteAccount}
            onResetAccount={handleResetAccount}
            onRefresh={refresh}
          />
        )}

        {activeTab === 'usage' && (
          <UsageView
            keys={keys}
            models={models}
            requests={requests}
            exportFiles={exportFiles}
            exportDir={exportDir}
            exportRetentionDays={exportRetentionDays}
            exportDays={exportDays}
            onExportDay={(day) => withRefresh(() => Kinetix.exportDay(day))}
            onDeleteExport={(name) => withRefresh(() => Kinetix.deleteExport(name))}
            onRefreshExports={refresh}
          />
        )}

        {activeTab === 'requests' && (
          <RequestsView requests={requests} liveRequests={liveRequests} />
        )}

        {activeTab === 'health' && <HealthView />}

        {activeTab === 'aliases' && (
          <AliasesView
            aliases={aliases}
            routes={routes}
            models={models}
            onAddAlias={handleAddAlias}
            onDeleteAlias={handleDeleteAlias}
          />
        )}

        {activeTab === 'audit' && <AuditView logs={auditLogs} />}

        {activeTab === 'plugins' && <PluginsView />}

        {activeTab === 'settings' && <SettingsView onLogout={handleLogout} />}
        </main>

        <div className="w-full px-4 md:px-8">
          <SquiggleDivider />
        </div>

        <footer className="w-full py-6 px-4 text-center font-body text-sm text-[var(--ink)]/70">
          <p className="flex items-center justify-center gap-2 flex-wrap">
            <strong className="font-heading text-base text-[var(--ink)]">Kinetix</strong>
            <span>•</span>
            <span>Zero-downtime LLM Multi-Protocol Proxy</span>
            <span>•</span>
            <span className="underline decoration-wavy decoration-[var(--marker-red)]">Hand-Drawn Design System</span>
          </p>
          <p className="text-xs text-[var(--ink)]/50 font-mono mt-1">
            OpenAI &amp; Anthropic streaming in • Gemini, OpenAI, &amp; Anthropic upstream out • SQLite WAL at rest
          </p>
        </footer>
      </div>

      <div className="fixed bottom-6 right-6 z-40">
        <SketchButton
          variant="danger"
          size="lg"
          onClick={() => setIsTesterOpen(true)}
          className="gap-2 font-heading font-bold shadow-lg shadow-black/10"
        >
          <Play className="w-5 h-5 fill-[var(--surface)]" />
          Test Proxy Live
        </SketchButton>
      </div>

      <LiveTesterModal
        isOpen={isTesterOpen}
        onClose={() => {
          setIsTesterOpen(false);
          refresh();
        }}
        keys={keys}
        routes={routes}
        models={models}
      />
    </div>
  );
}
