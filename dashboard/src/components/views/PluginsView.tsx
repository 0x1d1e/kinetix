import React, { useCallback, useEffect, useMemo, useState } from 'react';
import {
  AlertTriangle,
  Box,
  CheckCircle2,
  KeyRound,
  LogIn,
  Network,
  PackagePlus,
  Power,
  PowerOff,
  RefreshCw,
  Search,
  ShieldCheck,
  Trash2,
  Upload,
} from 'lucide-react';
import {
  Kinetix,
  PluginCatalogEntry,
  PluginCatalogPreview,
  PluginDetail,
  PluginInstallResult,
  PluginPermissionResponse,
  PluginRollbackPreview,
  PluginSettingState,
  PluginSummary,
} from '../../lib/resources';
import { Provider } from '../../types';
import { SketchBadge, SketchButton, WobblyCard } from '../HandDrawnElements';
import { ConnectionParameterFields } from '../ConnectionParameterFields';
import { useAuthEnrollment } from '../CredentialAuthFlow';

function fileAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error('Failed to read plugin package.'));
    reader.onload = () => {
      const value = String(reader.result ?? '');
      const comma = value.indexOf(',');
      if (comma < 0) {
        reject(new Error('Failed to encode plugin package.'));
        return;
      }
      resolve(value.slice(comma + 1));
    };
    reader.readAsDataURL(file);
  });
}

function requestedGrantPairs(plugin: PluginSummary): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  if (plugin.permissions.network_hosts.length > 0) {
    out.push(['network_hosts', JSON.stringify(plugin.permissions.network_hosts)]);
  }
  if (plugin.permissions.credential_scopes.length > 0) {
    out.push(['credential_scopes', JSON.stringify(plugin.permissions.credential_scopes)]);
  }
  if (plugin.permissions.credential_read) {
    out.push(['credential_read', 'true']);
  }
  return out;
}

function isFullyApproved(plugin: PluginSummary, permissions: PluginPermissionResponse | null): boolean {
  if (!permissions) return requestedGrantPairs(plugin).length === 0;
  const approved = new Set(
    permissions.approved.map((grant) => `${grant.permission}\0${grant.value_json}`),
  );
  return requestedGrantPairs(plugin).every(
    ([permission, value]) => approved.has(`${permission}\0${value}`),
  );
}

function prettyCapability(capability: string): string {
  return capability
    .replaceAll('_', ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

export const PluginsView: React.FC = () => {
  const [plugins, setPlugins] = useState<PluginSummary[]>([]);
  const [catalog, setCatalog] = useState<PluginCatalogEntry[]>([]);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<PluginDetail | null>(null);
  const [permissions, setPermissions] = useState<PluginPermissionResponse | null>(null);
  const [settings, setSettings] = useState<PluginSettingState[]>([]);
  const [settingDrafts, setSettingDrafts] = useState<Record<string, string | boolean>>({});
  const [rollbackPreview, setRollbackPreview] = useState<PluginRollbackPreview | null>(null);
  const [catalogPreview, setCatalogPreview] = useState<PluginCatalogPreview | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [connectionDrafts, setConnectionDrafts] = useState<Record<string, Record<string, string>>>({});
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [showInstall, setShowInstall] = useState(false);
  const [installTab, setInstallTab] = useState<'upload' | 'url'>('upload');
  const [installUrl, setInstallUrl] = useState('');
  const [packageFile, setPackageFile] = useState<File | null>(null);
  const [sha256, setSha256] = useState('');
  const [trustedKeys, setTrustedKeys] = useState('');
  const [allowUntrusted, setAllowUntrusted] = useState(false);
  const [catalogQuery, setCatalogQuery] = useState('');
  const [catalogCapabilityFilter, setCatalogCapabilityFilter] = useState<string>('all');
  const [refreshingCatalog, setRefreshingCatalog] = useState(false);

  const loadDetail = useCallback(async (id: string) => {
    const [plugin, grants, settingState] = await Promise.all([
      Kinetix.plugin(id),
      Kinetix.pluginPermissions(id),
      Kinetix.pluginSettings(id),
    ]);
    setDetail(plugin);
    setPermissions(grants);
    setSettings(settingState.settings);

    const drafts: Record<string, string | boolean> = {};
    for (const setting of settingState.settings) {
      if (setting.kind === 'boolean') {
        drafts[setting.key] = setting.value === true;
      } else if (setting.kind === 'secret') {
        drafts[setting.key] = '';
      } else {
        drafts[setting.key] = typeof setting.value === 'string' ? setting.value : '';
      }
    }
    setSettingDrafts(drafts);
  }, []);

  const refresh = useCallback(async (preferredId?: string | null) => {
    setLoading(true);
    try {
      const [rows, providerRows, catalogResponse] = await Promise.all([
        Kinetix.plugins(),
        Kinetix.providers(),
        Kinetix.pluginCatalog(),
      ]);
      setPlugins(rows);
      setProviders(providerRows);
      setCatalog(catalogResponse.plugins);
      const target = preferredId ?? selectedId;
      if (target && rows.some((plugin) => plugin.id === target)) {
        setSelectedId(target);
        await loadDetail(target);
      } else if (rows.length > 0) {
        setSelectedId(rows[0].id);
        await loadDetail(rows[0].id);
      } else {
        setSelectedId(null);
        setDetail(null);
        setPermissions(null);
        setSettings([]);
        setSettingDrafts({});
      }
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [loadDetail, selectedId]);

  const handleRefreshCatalog = async () => {
    setRefreshingCatalog(true);
    setError(null);
    try {
      await Kinetix.refreshPluginCatalog();
      const res = await Kinetix.pluginCatalog();
      setCatalog(res.plugins);
      setNotice('Marketplace catalog refreshed successfully.');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setRefreshingCatalog(false);
    }
  };

  const filteredCatalog = useMemo(() => {
    return catalog.filter((entry) => {
      if (catalogCapabilityFilter !== 'all') {
        if (!entry.capabilities.includes(catalogCapabilityFilter)) {
          return false;
        }
      }
      if (catalogQuery.trim()) {
        const q = catalogQuery.trim().toLowerCase();
        const matches =
          entry.id.toLowerCase().includes(q) ||
          entry.name.toLowerCase().includes(q) ||
          entry.description.toLowerCase().includes(q) ||
          entry.publisher.toLowerCase().includes(q);
        if (!matches) return false;
      }
      return true;
    });
  }, [catalog, catalogCapabilityFilter, catalogQuery]);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const authResult = params.get('plugin_auth');
    if (authResult) {
      const messages: Record<string, string> = {
        success: 'Account connected successfully through the plugin authorization flow.',
        cancelled: 'Account authorization was cancelled.',
        error: 'Account authorization failed during the provider exchange.',
        reauthorization_required:
          'The provider rejected the newly authorized credential. Reauthorize the account and try again.',
        binding_changed:
          'Account authorization was refused because the provider plugin binding changed during login.',
      };
      const message = messages[authResult] ?? 'Account authorization returned an unknown result.';
      if (authResult === 'success') {
        setNotice('Account connected successfully. Discover and configure models in Providers & Models.');
      } else {
        setError(message);
      }
      params.delete('plugin_auth');
      params.delete('plugin_auth_provider');
      const query = params.toString();
      window.history.replaceState(
        null,
        '',
        `${window.location.pathname}${query ? `?${query}` : ''}${window.location.hash}`,
      );
    }

    void refresh(null);
    // Initial load only; later refreshes are explicit so selecting an item does
    // not re-run this effect through the selectedId dependency.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const selectPlugin = async (id: string) => {
    setSelectedId(id);
    setRollbackPreview(null);
    setBusy('detail');
    try {
      await loadDetail(id);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const authEnrollment = useAuthEnrollment({
    onSuccess: async () => {
      setNotice('Account connected successfully. Discover and configure models in Providers & Models.');
      await refresh(selectedId);
    },
    onError: (message) => {
      setError(message);
      setBusy(null);
    },
  });

  const mutate = async (label: string, fn: () => Promise<unknown>, message: string) => {
    setBusy(label);
    setError(null);
    setNotice(null);
    try {
      await fn();
      setNotice(message);
      await refresh(selectedId);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const removePlugin = async (id: string) => {
    setBusy('remove');
    setError(null);
    setNotice(null);
    try {
      await Kinetix.removePlugin(id);
      setSelectedId(null);
      setDetail(null);
      setPermissions(null);
      setSettings([]);
      setSettingDrafts({});
      setNotice('Plugin removed.');
      await refresh(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const saveSettings = async () => {
    if (!selectedId) return;
    setBusy('settings');
    setError(null);
    setNotice(null);

    const values: Record<string, unknown> = {};
    for (const setting of settings) {
      const draft = settingDrafts[setting.key];
      if (setting.kind === 'secret') {
        if (typeof draft === 'string' && draft.length > 0) {
          values[setting.key] = draft;
        }
        continue;
      }
      values[setting.key] = draft ?? (setting.kind === 'boolean' ? false : '');
    }

    try {
      const response = await Kinetix.updatePluginSettings(selectedId, values);
      setSettings(response.settings);
      setSettingDrafts((current) => {
        const next = { ...current };
        for (const setting of response.settings) {
          if (setting.kind === 'secret') {
            next[setting.key] = '';
          }
        }
        return next;
      });
      setNotice('Plugin settings saved.');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const clearSetting = async (key: string) => {
    if (!selectedId) return;
    setBusy(`setting:${key}`);
    setError(null);
    setNotice(null);
    try {
      const response = await Kinetix.updatePluginSettings(selectedId, { [key]: null });
      setSettings(response.settings);
      setSettingDrafts((current) => ({ ...current, [key]: '' }));
      setNotice(`Cleared plugin setting “${key}”.`);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const reviewRollback = async (id: string, sha256: string) => {
    setBusy(`preview:${sha256}`);
    setError(null);
    try {
      const preview = await Kinetix.previewPluginRollback(id, sha256);
      setRollbackPreview(preview);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const confirmRollback = async () => {
    if (!selectedId || !rollbackPreview) return;
    const target = rollbackPreview;
    setBusy(`rollback:${target.package_sha256}`);
    setError(null);
    setNotice(null);
    try {
      await Kinetix.rollbackPlugin(selectedId, target.package_sha256);
      setRollbackPreview(null);
      setNotice(
        `Rolled back to v${target.target_version}. Review permissions before enabling.`,
      );
      await refresh(selectedId);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const reviewCatalogInstall = async (entry: PluginCatalogEntry) => {
    setBusy(`catalog-preview:${entry.id}`);
    setError(null);
    setNotice(null);
    try {
      const preview = await Kinetix.previewCatalogPlugin(entry.id);
      setCatalogPreview(preview);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const confirmCatalogInstall = async (approveAndEnable = false) => {
    if (!catalogPreview) return;
    const preview = catalogPreview;
    setBusy(`catalog:${preview.id}`);
    setError(null);
    setNotice(null);
    try {
      const outcome = await Kinetix.installCatalogPlugin(preview.id);
      if (approveAndEnable) {
        await Kinetix.approvePluginPermissions(outcome.id);
        await Kinetix.enablePlugin(outcome.id);
        setNotice(`Installed, approved, and enabled ${outcome.id} v${outcome.version}.`);
      } else {
        setNotice(
          `Installed ${outcome.id} v${outcome.version} from the trusted catalog. Review permissions before enabling it.`,
        );
      }
      setCatalogPreview(null);
      await refresh(outcome.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const install = async (e: React.FormEvent) => {
    e.preventDefault();
    if (installTab === 'upload' && !packageFile) {
      setError('Choose a .kxp package first.');
      return;
    }
    if (installTab === 'url' && !installUrl.trim()) {
      setError('Enter a valid .kxp download URL.');
      return;
    }
    setBusy('install');
    setError(null);
    setNotice(null);
    try {
      const keys = trustedKeys
        .split(/[\n,]/)
        .map((value) => value.trim())
        .filter(Boolean);

      let outcome: PluginInstallResult;
      if (installTab === 'upload' && packageFile) {
        const package_base64 = await fileAsBase64(packageFile);
        outcome = await Kinetix.installPlugin({
          package_base64,
          sha256: sha256.trim() || undefined,
          trusted_keys: keys,
          allow_untrusted_signature: allowUntrusted,
        });
      } else {
        outcome = await Kinetix.installPlugin({
          url: installUrl.trim(),
          sha256: sha256.trim() || undefined,
          trusted_keys: keys,
          allow_untrusted_signature: allowUntrusted,
        });
      }

      setNotice(
        `Installed ${outcome.id} v${outcome.version}. Review permissions before enabling it.`,
      );
      setShowInstall(false);
      setPackageFile(null);
      setInstallUrl('');
      setSha256('');
      setTrustedKeys('');
      setAllowUntrusted(false);
      await refresh(outcome.id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const setupAndConnect = async (
    pluginId: string,
    integrationId: string,
    _flowName: string,
  ) => {
    setBusy(`setup:${integrationId}`);
    setError(null);
    setNotice(null);
    try {
      const provider = await Kinetix.setupPluginIntegrationProvider(pluginId, integrationId, connectionDrafts[`${pluginId}/${integrationId}`] || {});
      setConnectionDrafts((drafts) => ({ ...drafts, [`${pluginId}/${integrationId}`]: {} }));
      await authEnrollment.begin(
        provider.id,
        () => Kinetix.startProviderCredentialEnrollment(provider.id),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const setupProvider = async (pluginId: string, integrationId: string) => {
    setBusy(`setup:${integrationId}`);
    setError(null);
    setNotice(null);
    try {
      const provider = await Kinetix.setupPluginIntegrationProvider(pluginId, integrationId, connectionDrafts[`${pluginId}/${integrationId}`] || {});
      setConnectionDrafts((drafts) => ({ ...drafts, [`${pluginId}/${integrationId}`]: {} }));
      setNotice(provider.created ? 'Provider created successfully.' : 'Existing Provider selected. Connection values unchanged.');
      await refresh(selectedId);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const connectAccount = async (
    _pluginId: string,
    _flowName: string,
    providerId: string,
  ) => {
    setBusy(`auth:${providerId}`);
    setError(null);
    setNotice(null);
    try {
      await authEnrollment.begin(
        providerId,
        () => Kinetix.startProviderCredentialEnrollment(providerId),
      );
    } finally {
      setBusy(null);
    }
  };


  const selected = useMemo(
    () => plugins.find((plugin) => plugin.id === selectedId) ?? detail,
    [plugins, selectedId, detail],
  );
  const fullyApproved = selected ? isFullyApproved(selected, permissions) : false;
  const requested = selected ? requestedGrantPairs(selected) : [];

  return (
    <div className="space-y-6">
      <div className="flex flex-col lg:flex-row lg:items-start gap-4">
        <div className="flex-1">
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2 flex-wrap">
            <Box className="w-7 h-7 text-[var(--pen-blue)]" />
            <span>Plugins &amp; Integrations</span>
            <SketchBadge variant="blue" rotation="1deg">WASM</SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80 max-w-3xl">
            Install sandboxed <span className="font-mono">.kxp</span> packages, review their requested
            authority, validate them, and control their runtime lifecycle from the dashboard.
          </p>
        </div>
        <div className="flex gap-2 flex-wrap">
          <SketchButton
            variant="secondary"
            onClick={() => void refresh(selectedId)}
            disabled={loading || busy !== null}
            className="gap-2"
          >
            <RefreshCw className={`w-4 h-4 ${loading ? 'animate-spin' : ''}`} />
            Refresh
          </SketchButton>
          <SketchButton
            variant="primary"
            onClick={() => setShowInstall((value) => !value)}
            className="gap-2"
          >
            <PackagePlus className="w-4 h-4" />
            Install .kxp
          </SketchButton>
        </div>
      </div>

      {authEnrollment.modal}

      {error && (
        <div className="p-3 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] text-sm font-mono text-[var(--danger-text)] flex gap-2 items-start">
          <AlertTriangle className="w-4 h-4 mt-0.5 shrink-0" />
          <span>{error}</span>
        </div>
      )}
      {notice && (
        <div className="p-3 bg-[var(--tint-green)] border-2 border-[var(--pen-green)] text-sm font-mono text-[var(--success-text)] flex gap-2 items-start">
          <CheckCircle2 className="w-4 h-4 mt-0.5 shrink-0" />
          <span>{notice}</span>
        </div>
      )}


      {showInstall && (
        <WobblyCard decoration="tape" className="p-5">
          <form onSubmit={install} className="space-y-4">
            <div>
              <h3 className="text-xl font-heading font-bold flex items-center gap-2">
                <Upload className="w-5 h-5 text-[var(--pen-blue)]" />
                Install Kinetix Extension Package
              </h3>
              <p className="text-sm font-body text-[var(--ink)]/75">
                Install a custom <span className="font-mono">.kxp</span> package from a local file or remote URL (GitHub release, CDN, or raw git host).
              </p>
            </div>

            <div className="flex gap-2 border-b border-[var(--ink)]/20 pb-2">
              <button
                type="button"
                onClick={() => setInstallTab('upload')}
                className={`text-sm font-heading font-bold px-3 py-1.5 rounded transition-all cursor-pointer ${
                  installTab === 'upload'
                    ? 'bg-[var(--ink)] text-[var(--paper)]'
                    : 'bg-[var(--surface)] text-[var(--ink)]/70 hover:text-[var(--ink)]'
                }`}
              >
                Upload File (.kxp)
              </button>
              <button
                type="button"
                onClick={() => setInstallTab('url')}
                className={`text-sm font-heading font-bold px-3 py-1.5 rounded transition-all cursor-pointer ${
                  installTab === 'url'
                    ? 'bg-[var(--ink)] text-[var(--paper)]'
                    : 'bg-[var(--surface)] text-[var(--ink)]/70 hover:text-[var(--ink)]'
                }`}
              >
                Install from URL
              </button>
            </div>

            <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
              {installTab === 'upload' ? (
                <label className="block">
                  <span className="block text-sm font-heading font-bold mb-1">Package file</span>
                  <input
                    type="file"
                    accept=".kxp,application/octet-stream"
                    onChange={(e) => setPackageFile(e.target.files?.[0] ?? null)}
                    className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm"
                  />
                </label>
              ) : (
                <label className="block">
                  <span className="block text-sm font-heading font-bold mb-1">Package URL (.kxp)</span>
                  <input
                    type="url"
                    value={installUrl}
                    onChange={(e) => setInstallUrl(e.target.value)}
                    placeholder="https://github.com/user/repo/releases/download/v1.0/plugin.kxp"
                    className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm"
                  />
                </label>
              )}
              <label className="block">
                <span className="block text-sm font-heading font-bold mb-1">Expected SHA-256 (optional)</span>
                <input
                  value={sha256}
                  onChange={(e) => setSha256(e.target.value)}
                  placeholder="64 hex characters"
                  className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm"
                />
              </label>
            </div>

            <label className="block">
              <span className="block text-sm font-heading font-bold mb-1">
                Trusted Ed25519 public keys (optional, one per line)
              </span>
              <textarea
                value={trustedKeys}
                onChange={(e) => setTrustedKeys(e.target.value)}
                rows={3}
                placeholder="Base64 or hex Ed25519 public keys to trust for this installation"
                className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-xs"
              />
            </label>

            <label className="flex items-start gap-2 text-sm font-body cursor-pointer">
              <input
                type="checkbox"
                checked={allowUntrusted}
                onChange={(e) => setAllowUntrusted(e.target.checked)}
                className="mt-1 cursor-pointer"
              />
              <span>
                Allow unsigned or untrusted signature (check this if the custom package is self-built or signed with a custom/unregistered key).
              </span>
            </label>

            <div className="flex gap-2">
              <SketchButton
                type="submit"
                variant="primary"
                disabled={busy === 'install' || (installTab === 'upload' ? !packageFile : !installUrl.trim())}
                className="gap-2"
              >
                <Upload className="w-4 h-4" />
                {busy === 'install' ? 'Installing…' : 'Install package'}
              </SketchButton>
              <SketchButton type="button" variant="secondary" onClick={() => setShowInstall(false)}>
                Cancel
              </SketchButton>
            </div>
          </form>
        </WobblyCard>
      )}

      {catalog.length > 0 && (
        <WobblyCard variant="muted" className="p-5">
          <div className="flex items-start justify-between gap-3 flex-wrap">
            <div>
              <h3 className="text-xl font-heading font-bold">Discover Marketplace</h3>
              <p className="text-sm font-body text-[var(--ink)]/70">
                Official plugins verified and distributed through the Kinetix ecosystem.
                Installing or updating validates cryptographic signatures and prompts for operator permission approval.
              </p>
            </div>
            <div className="flex items-center gap-2">
              <SketchButton
                variant="secondary"
                className="gap-2 text-xs py-1.5 px-3"
                disabled={refreshingCatalog || busy !== null}
                onClick={() => void handleRefreshCatalog()}
              >
                <RefreshCw className={`w-3.5 h-3.5 ${refreshingCatalog ? 'animate-spin' : ''}`} />
                {refreshingCatalog ? 'Refreshing…' : 'Refresh Marketplace'}
              </SketchButton>
              <SketchBadge variant="blue">Official catalog</SketchBadge>
            </div>
          </div>

          <div className="mt-4 flex flex-col sm:flex-row gap-3 items-center justify-between">
            <div className="relative w-full sm:max-w-md">
              <Search className="w-4 h-4 absolute left-3 top-1/2 -translate-y-1/2 text-[var(--ink)]/40 pointer-events-none" />
              <input
                type="text"
                value={catalogQuery}
                onChange={(e) => setCatalogQuery(e.target.value)}
                placeholder="Search marketplace plugins by name, ID, or description…"
                className="w-full pl-9 pr-3 py-1.5 text-sm rounded border border-[var(--ink)]/30 bg-[var(--surface)] focus:outline-none focus:border-[var(--ink)] font-body"
              />
            </div>
            <div className="flex flex-wrap gap-1.5 self-start sm:self-auto">
              {[
                { id: 'all', label: 'All' },
                { id: 'credential_strategy', label: 'OAuth / Credential' },
                { id: 'provider_adapter', label: 'Provider Adapter' },
                { id: 'model_source', label: 'Model Discovery' },
                { id: 'auth_flow', label: 'Auth Flow' },
              ].map((chip) => (
                <button
                  key={chip.id}
                  type="button"
                  onClick={() => setCatalogCapabilityFilter(chip.id)}
                  className={`text-xs px-2.5 py-1 rounded-full font-medium transition-colors border ${
                    catalogCapabilityFilter === chip.id
                      ? 'bg-[var(--ink)] text-[var(--paper)] border-[var(--ink)]'
                      : 'bg-[var(--surface)] text-[var(--ink)]/70 border-[var(--ink)]/20 hover:border-[var(--ink)]/40'
                  }`}
                >
                  {chip.label}
                </button>
              ))}
            </div>
          </div>

          {filteredCatalog.length === 0 ? (
            <div className="mt-6 text-center py-8 border border-dashed border-[var(--ink)]/20 rounded">
              <p className="text-sm font-body text-[var(--ink)]/60">
                No marketplace plugins match the current filters.
              </p>
            </div>
          ) : (
            <div className="mt-4 grid grid-cols-1 lg:grid-cols-2 gap-3">
              {filteredCatalog.map((entry) => {
                const installed = plugins.find((plugin) => plugin.id === entry.id);
                const hasUpdate = Boolean(installed && entry.update_available);

                return (
                  <div
                    key={entry.id}
                    className={`p-4 border-2 ${
                      hasUpdate ? 'border-amber-400 bg-amber-50/20' : 'border-[var(--ink)]/25 bg-[var(--surface)]'
                    }`}
                    style={{ borderRadius: '12px 9px 14px 10px / 9px 14px 9px 12px' }}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div>
                        <div className="font-heading font-bold flex items-center gap-2">
                          <span>{entry.name}</span>
                          {hasUpdate && (
                            <span className="text-[10px] bg-amber-200 text-amber-900 font-bold px-1.5 py-0.5 rounded uppercase tracking-wider">
                              Update Available
                            </span>
                          )}
                        </div>
                        <div className="text-xs font-mono text-[var(--ink)]/55">{entry.publisher}</div>
                      </div>
                      <SketchBadge variant={installed ? (hasUpdate ? 'yellow' : 'green') : 'default'}>
                        {installed ? `Installed v${installed.version}` : `v${entry.latest_version}`}
                      </SketchBadge>
                    </div>

                    <p className="mt-2 text-sm font-body text-[var(--ink)]/75">{entry.description}</p>
                    <div className="mt-3 flex flex-wrap gap-1">
                      {entry.capabilities.map((capability) => (
                        <code key={capability} className="text-xs bg-[var(--erased)] px-2 py-1">
                          {capability}
                        </code>
                      ))}
                    </div>
                    {entry.note && (
                      <p className="mt-3 text-xs font-body text-[var(--ink)]/60">{entry.note}</p>
                    )}
                    <div className="mt-3 flex items-center justify-between gap-3 flex-wrap">
                      <div className="text-xs font-mono text-[var(--ink)]/55">
                        Artifact: {entry.artifact_name}
                      </div>
                      {installed && !hasUpdate ? (
                        <SketchBadge variant="green">Current (v{installed.version})</SketchBadge>
                      ) : entry.install_ready ? (
                        <SketchButton
                          variant="primary"
                          className="gap-2"
                          disabled={busy !== null}
                          onClick={() => void reviewCatalogInstall(entry)}
                        >
                          <ShieldCheck className="w-4 h-4" />
                          {busy === `catalog-preview:${entry.id}`
                            ? 'Verifying…'
                            : hasUpdate
                              ? `Review update to v${entry.latest_version}`
                              : 'Review install'}
                        </SketchButton>
                      ) : (
                        <SketchBadge variant="yellow">
                          {entry.trust_status === 'unavailable'
                            ? 'Trust unavailable'
                            : 'Discovery only'}
                        </SketchBadge>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </WobblyCard>
      )}

      {catalogPreview && (
        <WobblyCard decoration="tape" className="p-5">
          <div className="flex flex-col md:flex-row md:items-start gap-4">
            <div className="flex-1">
              <h3 className="text-xl font-heading font-bold">
                {catalogPreview.current_version
                  ? `Review update: v${catalogPreview.current_version} → v${catalogPreview.target_version}`
                  : `Review install: v${catalogPreview.target_version}`}
              </h3>
              <p className="mt-1 text-sm font-body text-[var(--ink)]/70">
                Kinetix downloaded the exact catalog artifact and verified its HTTPS distribution
                constraints, SHA-256, manifest identity/version, and trusted Ed25519 signature.
                Confirmation repeats those checks before installation.
              </p>
            </div>
            <SketchBadge variant="green">Signature verified</SketchBadge>
          </div>

          <div className="mt-4 grid grid-cols-1 lg:grid-cols-3 gap-4">
            <div>
              <h4 className="font-heading font-bold text-sm mb-2">Network hosts</h4>
              {catalogPreview.permission_diff.network_hosts.added.length === 0 &&
              catalogPreview.permission_diff.network_hosts.removed.length === 0 ? (
                <p className="text-xs font-mono text-[var(--ink)]/55">No change</p>
              ) : (
                <div className="space-y-1 text-xs font-mono">
                  {catalogPreview.permission_diff.network_hosts.added.map((value) => (
                    <div key={`catalog-host-add-${value}`}>+ {value}</div>
                  ))}
                  {catalogPreview.permission_diff.network_hosts.removed.map((value) => (
                    <div key={`catalog-host-remove-${value}`}>− {value}</div>
                  ))}
                </div>
              )}
            </div>

            <div>
              <h4 className="font-heading font-bold text-sm mb-2">Credential scopes</h4>
              {catalogPreview.permission_diff.credential_scopes.added.length === 0 &&
              catalogPreview.permission_diff.credential_scopes.removed.length === 0 ? (
                <p className="text-xs font-mono text-[var(--ink)]/55">No change</p>
              ) : (
                <div className="space-y-1 text-xs font-mono">
                  {catalogPreview.permission_diff.credential_scopes.added.map((value) => (
                    <div key={`catalog-scope-add-${value}`}>+ {value}</div>
                  ))}
                  {catalogPreview.permission_diff.credential_scopes.removed.map((value) => (
                    <div key={`catalog-scope-remove-${value}`}>− {value}</div>
                  ))}
                </div>
              )}
            </div>

            <div>
              <h4 className="font-heading font-bold text-sm mb-2">Credential plaintext</h4>
              <p className="text-xs font-mono">
                {catalogPreview.permission_diff.credential_read.changed
                  ? `${catalogPreview.permission_diff.credential_read.from ? 'enabled' : 'disabled'} → ${catalogPreview.permission_diff.credential_read.to ? 'enabled' : 'disabled'}`
                  : catalogPreview.permission_diff.credential_read.to
                    ? 'enabled (unchanged)'
                    : 'disabled (unchanged)'}
              </p>
            </div>
          </div>

          <div className="mt-4 p-3 bg-[var(--erased)]/60 border border-dashed border-[var(--ink)]/25">
            <div className="text-xs font-mono break-all">SHA-256: {catalogPreview.sha256}</div>
            <div className="mt-1 text-xs font-mono text-[var(--ink)]/65">
              Requested capabilities: {catalogPreview.provides.map((item) => prettyCapability(item.capability)).join(', ') || 'none'}
            </div>
          </div>

          <div className="mt-4 flex gap-2 flex-wrap items-center">
            <SketchButton
              variant="primary"
              disabled={busy !== null}
              onClick={() => void confirmCatalogInstall(true)}
            >
              <CheckCircle2 className="w-4 h-4" />
              {busy === `catalog:${catalogPreview.id}`
                ? 'Installing…'
                : catalogPreview.current_version
                  ? `Update & Enable (v${catalogPreview.target_version})`
                  : `Install & Enable (v${catalogPreview.target_version})`}
            </SketchButton>
            <SketchButton
              variant="secondary"
              disabled={busy !== null}
              onClick={() => void confirmCatalogInstall(false)}
            >
              <PackagePlus className="w-4 h-4" />
              Install Disabled (Review Later)
            </SketchButton>
            <SketchButton
              variant="secondary"
              disabled={busy !== null}
              onClick={() => setCatalogPreview(null)}
            >
              Cancel
            </SketchButton>
          </div>
        </WobblyCard>
      )}

      <div className="grid grid-cols-1 xl:grid-cols-[minmax(280px,0.8fr)_minmax(0,2fr)] gap-6">
        <div className="space-y-3">
          {loading && plugins.length === 0 && (
            <WobblyCard variant="muted" className="p-5 text-sm font-mono">
              Loading plugins…
            </WobblyCard>
          )}
          {!loading && plugins.length === 0 && (
            <WobblyCard variant="muted" className="p-5">
              <h3 className="font-heading font-bold text-lg mb-1">No plugins installed</h3>
              <p className="text-sm font-body text-[var(--ink)]/75">
                Upload a <span className="font-mono">.kxp</span> package to start extending Kinetix.
              </p>
            </WobblyCard>
          )}
          {plugins.map((plugin) => (
            <button
              key={plugin.id}
              type="button"
              onClick={() => void selectPlugin(plugin.id)}
              className={`w-full text-left p-4 border-2 cursor-pointer transition-all ${selectedId === plugin.id
                ? 'bg-[var(--surface)] border-[var(--ink)] sketch-shadow-sm -translate-y-0.5'
                : 'bg-transparent border-[var(--ink)]/25 hover:border-[var(--ink)]/60 hover:bg-[var(--erased)]/40'
              }`}
              style={{ borderRadius: '14px 10px 16px 10px / 10px 16px 10px 14px' }}
            >
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <div className="font-heading font-bold truncate">{plugin.name || plugin.id}</div>
                  <div className="font-mono text-[0.72rem] text-[var(--ink)]/55 truncate">{plugin.id}</div>
                </div>
                <SketchBadge variant={plugin.status === 'enabled' ? 'green' : 'yellow'} rotation="1deg">
                  {plugin.status}
                </SketchBadge>
              </div>
              <div className="mt-2 text-xs font-mono text-[var(--ink)]/70">
                v{plugin.version} · API {plugin.plugin_api_major}
              </div>
            </button>
          ))}
        </div>

        <div>
          {selected ? (
            <div className="space-y-5">
              <WobblyCard decoration="tack" className="p-5">
                <div className="flex flex-col md:flex-row md:items-start gap-4">
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2 flex-wrap">
                      <h3 className="text-2xl font-heading font-bold">{selected.name || selected.id}</h3>
                      <SketchBadge variant={selected.status === 'enabled' ? 'green' : 'yellow'}>
                        {selected.status}
                      </SketchBadge>
                      <SketchBadge variant={selected.signature === 'verified' ? 'green' : 'yellow'}>
                        {selected.signature}
                      </SketchBadge>
                    </div>
                    <div className="font-mono text-xs text-[var(--ink)]/60 mt-1 break-all">{selected.id}</div>
                    <div className="font-mono text-xs text-[var(--ink)]/60 mt-1 break-all">
                      SHA-256 {selected.sha256}
                    </div>
                  </div>
                  <div className="flex gap-2 flex-wrap">
                    <SketchButton
                      variant="secondary"
                      disabled={busy !== null}
                      onClick={() =>
                        void mutate(
                          'validate',
                          () => Kinetix.validatePlugin(selected.id),
                          'Plugin component validated successfully.',
                        )
                      }
                    >
                      Validate
                    </SketchButton>
                    {selected.status === 'enabled' ? (
                      <SketchButton
                        variant="secondary"
                        disabled={busy !== null}
                        onClick={() =>
                          void mutate(
                            'disable',
                            () => Kinetix.disablePlugin(selected.id),
                            'Plugin disabled.',
                          )
                        }
                        className="gap-2"
                      >
                        <PowerOff className="w-4 h-4" /> Disable
                      </SketchButton>
                    ) : (
                      <SketchButton
                        variant="primary"
                        disabled={busy !== null || !fullyApproved}
                        onClick={() =>
                          void mutate(
                            'enable',
                            () => Kinetix.enablePlugin(selected.id),
                            'Plugin enabled.',
                          )
                        }
                        className="gap-2"
                      >
                        <Power className="w-4 h-4" /> Enable
                      </SketchButton>
                    )}
                  </div>
                </div>
              </WobblyCard>

              {selected.integrations.length > 0 && (
                <WobblyCard decoration="tape" className="p-5">
                  <h4 className="text-lg font-heading font-bold mb-3">Integrations</h4>
                  <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                    {selected.integrations.map((integration) => {
                      const parameterized = Object.keys(integration.provider?.parameters || {}).length > 0;
                      const integrationProviders = providers.filter((provider) =>
                        provider.sourcePluginId === selected.id && provider.sourceIntegrationId === integration.id,
                      );
                      const values = connectionDrafts[`${selected.id}/${integration.id}`] || {};
                      return (
                      <div
                        key={integration.id}
                        className="p-4 border-2 border-[var(--ink)]/30 bg-[var(--surface)]"
                        style={{ borderRadius: '12px 9px 14px 10px / 9px 14px 9px 12px' }}
                      >
                        <div className="flex items-start justify-between gap-2">
                          <div>
                            <div className="font-heading font-bold">{integration.name}</div>
                            <code className="text-[0.7rem] text-[var(--ink)]/55">{integration.id}</code>
                          </div>
                          <SketchBadge variant="blue">Integration</SketchBadge>
                        </div>
                        {integration.description && (
                          <p className="mt-2 text-sm font-body text-[var(--ink)]/75">
                            {integration.description}
                          </p>
                        )}
                        <div className="mt-3 flex flex-wrap gap-1">
                          {integration.provider_adapter && (
                            <code className="text-xs bg-[var(--erased)] px-2 py-1">
                              adapter:{integration.provider_adapter}
                            </code>
                          )}
                          {integration.credential_strategy && (
                            <code className="text-xs bg-[var(--erased)] px-2 py-1">
                              credential:{integration.credential_strategy}
                            </code>
                          )}
                          {integration.auth_flow && (
                            <code className="text-xs bg-[var(--erased)] px-2 py-1">
                              login:{integration.auth_flow}
                            </code>
                          )}
                          {integration.model_source && (
                            <code className="text-xs bg-[var(--erased)] px-2 py-1">
                              models:{integration.model_source}
                            </code>
                          )}
                        </div>

                        {parameterized && integration.provider?.parameters && (
                          <div className="mt-3">
                            <ConnectionParameterFields
                              declarations={integration.provider.parameters}
                              values={values}
                              onChange={(values) => setConnectionDrafts((drafts) => ({ ...drafts, [`${selected.id}/${integration.id}`]: values }))}
                              disabled={busy !== null || selected.status !== 'enabled'}
                            />
                            {integrationProviders.length > 0 && (
                              <p className="mt-2 text-xs font-body text-[var(--ink)]/60">
                                Different values create another Provider. Existing Providers are not changed.
                              </p>
                            )}
                          </div>
                        )}
                        {integrationProviders.length > 0 && (
                          <div className="mt-3 flex items-center gap-1.5 text-xs font-heading font-bold text-emerald-700">
                            <CheckCircle2 className="w-4 h-4 text-emerald-600 shrink-0" />
                            <span>
                              Active in Upstream Providers ({integrationProviders.map((provider) => provider.name).join(', ')})
                            </span>
                          </div>
                        )}

                        {(parameterized || integrationProviders.length === 0) &&
                          integration.provider &&
                          selected.ui.actions.filter((action) => action.integration === integration.id).length === 0 && (
                            <div className="mt-4 space-y-2">
                              <div className="text-xs font-mono text-[var(--ink)]/55 break-all">
                                {integration.provider.base_url}
                              </div>
                              <SketchButton
                                variant="primary"
                                className="gap-2"
                                disabled={busy !== null || selected.status !== 'enabled'}
                                onClick={() => void setupProvider(selected.id, integration.id)}
                              >
                                <PackagePlus className="w-4 h-4" />
                                {busy === `setup:${integration.id}` ? 'Setting up…' : integrationProviders.length > 0 ? 'Add another Provider' : 'Set up Provider'}
                              </SketchButton>
                              <p className="text-xs font-body text-[var(--ink)]/60">
                                Create the upstream provider from the plugin&apos;s validated defaults.
                              </p>
                            </div>
                          )}

                        {selected.ui.actions
                          .filter((action) => action.integration === integration.id)
                          .map((action) => {
                            if (
                              action.kind !== 'auth' ||
                              !integration.auth_flow ||
                              !integration.credential_strategy
                            ) {
                              return null;
                            }

                            const compatibleProviders = providers.filter(
                              (provider) =>
                                provider.credentialPlugin ===
                                `plugin:${selected.id}/${integration.credential_strategy}`,
                            );

                            return (
                              <div key={action.id} className="mt-4 space-y-2">
                                {action.description && (
                                  <p className="text-xs font-body text-[var(--ink)]/65">
                                    {action.description}
                                  </p>
                                )}
                                {compatibleProviders.map((provider) => (
                                  <SketchButton
                                    key={provider.id}
                                    variant="primary"
                                    className="gap-2"
                                    disabled={busy !== null || selected.status !== 'enabled'}
                                    onClick={() =>
                                      void connectAccount(
                                        selected.id,
                                        integration.auth_flow!,
                                        provider.id,
                                      )
                                    }
                                  >
                                    <LogIn className="w-4 h-4" />
                                    {action.label} · {provider.name}
                                  </SketchButton>
                                ))}
                                {(compatibleProviders.length === 0 || parameterized) &&
                                  (integration.provider ? (
                                    <div className="space-y-2">
                                      <div className="text-xs font-mono text-[var(--ink)]/55 break-all">
                                        {integration.provider.base_url}
                                      </div>
                                      <SketchButton
                                        variant="primary"
                                        className="gap-2"
                                        disabled={busy !== null || selected.status !== 'enabled'}
                                        onClick={() =>
                                          void setupAndConnect(
                                            selected.id,
                                            integration.id,
                                            integration.auth_flow!,
                                          )
                                        }
                                      >
                                        <LogIn className="w-4 h-4" />
                                        {busy === `setup:${integration.id}`
                                          ? 'Setting up…'
                                          : integrationProviders.length > 0
                                            ? `Add another Provider & ${action.label.toLowerCase()}`
                                            : `Set up & ${action.label.toLowerCase()}`}
                                      </SketchButton>
                                      <p className="text-xs font-body text-[var(--ink)]/60">
                                        Kinetix will create the provider from the plugin&apos;s
                                        validated defaults, bind only this integration&apos;s
                                        capabilities, then start the authorization flow.
                                      </p>
                                    </div>
                                  ) : (
                                    <p className="text-xs font-body text-[var(--ink)]/60">
                                      Bind a provider&apos;s credential plugin to{' '}
                                      <code>
                                        plugin:{selected.id}/{integration.credential_strategy}
                                      </code>{' '}
                                      before using “{action.label}”.
                                    </p>
                                  ))}
                              </div>
                            );
                          })}
                      </div>
                      );
                    })}
                  </div>
                </WobblyCard>
              )}

              {settings.length > 0 && (
                <WobblyCard className="p-5">
                  <div className="flex flex-col md:flex-row md:items-start gap-4">
                    <div className="flex-1">
                      <h4 className="text-lg font-heading font-bold">Settings</h4>
                      <p className="text-sm font-body text-[var(--ink)]/70">
                        These values are stored encrypted by Kinetix. Secret values are write-only in the dashboard.
                      </p>
                    </div>
                    <SketchButton
                      variant="primary"
                      disabled={busy !== null}
                      onClick={() => void saveSettings()}
                    >
                      {busy === 'settings' ? 'Saving…' : 'Save settings'}
                    </SketchButton>
                  </div>

                  <div className="mt-4 grid grid-cols-1 lg:grid-cols-2 gap-4">
                    {settings.map((setting) => (
                      <div key={setting.key} className="space-y-1">
                        <div className="flex items-center justify-between gap-2">
                          <label className="text-sm font-heading font-bold" htmlFor={`plugin-setting-${setting.key}`}>
                            {setting.label}
                            {setting.required && <span className="text-[var(--marker-red)]"> *</span>}
                          </label>
                          {setting.configured && !setting.required && (
                            <button
                              type="button"
                              className="text-xs font-mono underline text-[var(--ink)]/60 hover:text-[var(--ink)]"
                              disabled={busy !== null}
                              onClick={() => void clearSetting(setting.key)}
                            >
                              Clear
                            </button>
                          )}
                        </div>

                        {setting.kind === 'boolean' ? (
                          <label className="flex items-center gap-2 min-h-10">
                            <input
                              id={`plugin-setting-${setting.key}`}
                              type="checkbox"
                              checked={settingDrafts[setting.key] === true}
                              onChange={(e) =>
                                setSettingDrafts((current) => ({
                                  ...current,
                                  [setting.key]: e.target.checked,
                                }))
                              }
                            />
                            <span className="text-sm font-body">
                              {settingDrafts[setting.key] === true ? 'Enabled' : 'Disabled'}
                            </span>
                          </label>
                        ) : setting.kind === 'select' ? (
                          <select
                            id={`plugin-setting-${setting.key}`}
                            value={String(settingDrafts[setting.key] ?? '')}
                            onChange={(e) =>
                              setSettingDrafts((current) => ({
                                ...current,
                                [setting.key]: e.target.value,
                              }))
                            }
                            className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm"
                          >
                            {!setting.required && <option value="">—</option>}
                            {setting.options.map((option) => (
                              <option key={option} value={option}>{option}</option>
                            ))}
                          </select>
                        ) : (
                          <input
                            id={`plugin-setting-${setting.key}`}
                            type={setting.kind === 'secret' ? 'password' : 'text'}
                            value={String(settingDrafts[setting.key] ?? '')}
                            placeholder={
                              setting.kind === 'secret' && setting.configured
                                ? 'Configured — enter a new value to replace'
                                : undefined
                            }
                            onChange={(e) =>
                              setSettingDrafts((current) => ({
                                ...current,
                                [setting.key]: e.target.value,
                              }))
                            }
                            className="w-full px-3 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm"
                          />
                        )}

                        {setting.description && (
                          <p className="text-xs font-body text-[var(--ink)]/60">{setting.description}</p>
                        )}
                        {setting.kind === 'secret' && setting.configured && (
                          <div className="text-xs font-mono text-[var(--success-text)]">Configured</div>
                        )}
                      </div>
                    ))}
                  </div>
                </WobblyCard>
              )}

              <div className="grid grid-cols-1 lg:grid-cols-2 gap-5">
                <WobblyCard className="p-5">
                  <h4 className="text-lg font-heading font-bold flex items-center gap-2 mb-3">
                    <Box className="w-5 h-5 text-[var(--pen-blue)]" />
                    Capabilities
                  </h4>
                  <div className="space-y-2">
                    {selected.provides.length === 0 && (
                      <div className="text-sm text-[var(--ink)]/60">No capabilities reported.</div>
                    )}
                    {selected.provides.map((provided) => (
                      <div key={`${provided.capability}:${provided.name}`} className="flex items-center justify-between gap-3 border-b border-dashed border-[var(--ink)]/20 pb-2">
                        <span className="font-body text-sm">{prettyCapability(provided.capability)}</span>
                        <code className="text-xs break-all">{provided.name}</code>
                      </div>
                    ))}
                  </div>
                </WobblyCard>

                <WobblyCard variant="muted" className="p-5">
                  <h4 className="text-lg font-heading font-bold mb-3">Runtime limits</h4>
                  <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm">
                    <dt className="text-[var(--ink)]/65">Memory</dt><dd className="font-mono">{selected.limits.memory}</dd>
                    <dt className="text-[var(--ink)]/65">Wall time</dt><dd className="font-mono">{selected.limits.wall_time_ms} ms</dd>
                    <dt className="text-[var(--ink)]/65">HTTP requests</dt><dd className="font-mono">{selected.limits.max_outbound_requests}</dd>
                    <dt className="text-[var(--ink)]/65">HTTP body</dt><dd className="font-mono">{selected.limits.max_http_body}</dd>
                    <dt className="text-[var(--ink)]/65">Storage</dt><dd className="font-mono">{selected.limits.storage}</dd>
                    <dt className="text-[var(--ink)]/65">Routing facts</dt><dd className="font-mono">{selected.routing_facts_mode}</dd>
                    {selected.routing_facts_mode === 'cached' && (
                      <>
                        <dt className="text-[var(--ink)]/65">Fact refresh</dt>
                        <dd className="font-mono">{selected.routing_facts_refresh_ms} ms</dd>
                      </>
                    )}
                  </dl>
                </WobblyCard>
              </div>

              {detail?.packages && detail.packages.length > 0 && (
                <WobblyCard variant="muted" className="p-5">
                  <h4 className="text-lg font-heading font-bold mb-3">Retained packages</h4>
                  <div className="space-y-2">
                    {detail.packages.map((pkg) => {
                      const current = pkg.package_sha256 === selected.sha256;
                      return (
                        <div
                          key={pkg.package_sha256}
                          className="grid grid-cols-[auto_1fr_auto] gap-x-3 gap-y-1 border-b border-dashed border-[var(--ink)]/20 pb-2"
                        >
                          <SketchBadge variant={current ? 'green' : 'default'}>
                            v{pkg.version}
                          </SketchBadge>
                          <code className="text-xs break-all self-center">{pkg.package_sha256}</code>
                          {current ? (
                            <SketchBadge variant="green">Active</SketchBadge>
                          ) : (
                            <SketchButton
                              variant="secondary"
                              disabled={busy !== null}
                              onClick={() => void reviewRollback(selected.id, pkg.package_sha256)}
                            >
                              {busy === `preview:${pkg.package_sha256}` ? 'Reviewing…' : 'Review rollback'}
                            </SketchButton>
                          )}
                          <span className="text-xs text-[var(--ink)]/55">Stored package</span>
                          <code className="text-xs text-[var(--ink)]/55 break-all">{pkg.package_path}</code>
                          <span className="text-xs font-mono text-[var(--ink)]/55">{pkg.source}</span>
                        </div>
                      );
                    })}
                  </div>
                </WobblyCard>
              )}

              {rollbackPreview && (
                <WobblyCard decoration="tape" className="p-5">
                  <div className="flex flex-col md:flex-row md:items-start gap-4">
                    <div className="flex-1">
                      <h4 className="text-lg font-heading font-bold">
                        Review rollback: v{rollbackPreview.current_version} → v{rollbackPreview.target_version}
                      </h4>
                      <p className="text-sm font-body text-[var(--ink)]/70 mt-1">
                        The retained package has been re-hashed and its manifest revalidated for this preview.
                        Rollback will still recompile it, disable the plugin, and clear every approved permission.
                      </p>
                    </div>
                    <SketchBadge variant={rollbackPreview.signature === 'verified' ? 'green' : 'yellow'}>
                      {rollbackPreview.signature}
                    </SketchBadge>
                  </div>

                  <div className="mt-4 grid grid-cols-1 lg:grid-cols-2 gap-4">
                    <div>
                      <h5 className="font-heading font-bold text-sm mb-2">Network hosts</h5>
                      {rollbackPreview.permission_diff.network_hosts.added.length === 0 &&
                      rollbackPreview.permission_diff.network_hosts.removed.length === 0 ? (
                        <p className="text-xs font-mono text-[var(--ink)]/55">No change</p>
                      ) : (
                        <div className="space-y-1 text-xs font-mono">
                          {rollbackPreview.permission_diff.network_hosts.added.map((value) => (
                            <div key={`host-add-${value}`}>+ {value}</div>
                          ))}
                          {rollbackPreview.permission_diff.network_hosts.removed.map((value) => (
                            <div key={`host-remove-${value}`}>− {value}</div>
                          ))}
                        </div>
                      )}
                    </div>

                    <div>
                      <h5 className="font-heading font-bold text-sm mb-2">Credential scopes</h5>
                      {rollbackPreview.permission_diff.credential_scopes.added.length === 0 &&
                      rollbackPreview.permission_diff.credential_scopes.removed.length === 0 ? (
                        <p className="text-xs font-mono text-[var(--ink)]/55">No change</p>
                      ) : (
                        <div className="space-y-1 text-xs font-mono">
                          {rollbackPreview.permission_diff.credential_scopes.added.map((value) => (
                            <div key={`scope-add-${value}`}>+ {value}</div>
                          ))}
                          {rollbackPreview.permission_diff.credential_scopes.removed.map((value) => (
                            <div key={`scope-remove-${value}`}>− {value}</div>
                          ))}
                        </div>
                      )}
                    </div>
                  </div>

                  <div className="mt-4 p-3 bg-[var(--erased)]/60 border border-dashed border-[var(--ink)]/25 text-sm">
                    Plaintext credential access:{' '}
                    <strong>
                      {rollbackPreview.permission_diff.credential_read.changed
                        ? `${rollbackPreview.permission_diff.credential_read.from ? 'enabled' : 'disabled'} → ${rollbackPreview.permission_diff.credential_read.to ? 'enabled' : 'disabled'}`
                        : rollbackPreview.permission_diff.credential_read.to
                          ? 'enabled (unchanged)'
                          : 'disabled (unchanged)'}
                    </strong>
                  </div>

                  <div className="mt-4 flex gap-2 flex-wrap">
                    <SketchButton
                      variant="primary"
                      disabled={busy !== null}
                      onClick={() => void confirmRollback()}
                    >
                      {busy === `rollback:${rollbackPreview.package_sha256}`
                        ? 'Restoring…'
                        : `Confirm rollback to v${rollbackPreview.target_version}`}
                    </SketchButton>
                    <SketchButton
                      variant="secondary"
                      disabled={busy !== null}
                      onClick={() => setRollbackPreview(null)}
                    >
                      Cancel
                    </SketchButton>
                  </div>
                </WobblyCard>
              )}

              <WobblyCard decoration="tape" className="p-5">
                <div className="flex flex-col md:flex-row md:items-start gap-4">
                  <div className="flex-1">
                    <h4 className="text-lg font-heading font-bold flex items-center gap-2">
                      <ShieldCheck className="w-5 h-5 text-[var(--pen-blue)]" />
                      Permission review
                    </h4>
                    <p className="text-sm font-body text-[var(--ink)]/75">
                      Approval is tied to the current manifest. A plugin should not be enabled until the requested
                      authority has been reviewed.
                    </p>
                  </div>
                  <SketchBadge variant={fullyApproved ? 'green' : 'yellow'}>
                    {fullyApproved ? 'Approved' : 'Approval required'}
                  </SketchBadge>
                </div>

                <div className="mt-4 space-y-3">
                  {requested.length === 0 && (
                    <div className="text-sm font-body text-[var(--ink)]/70">
                      This plugin requests no privileged host capabilities.
                    </div>
                  )}

                  {selected.permissions.network_hosts.length > 0 && (
                    <div className="p-3 border-2 border-[var(--ink)]/30 bg-[var(--surface)]">
                      <div className="flex items-center gap-2 font-heading font-bold text-sm">
                        <Network className="w-4 h-4" /> Network hosts
                      </div>
                      <div className="mt-1 flex flex-wrap gap-1">
                        {selected.permissions.network_hosts.map((host) => (
                          <code key={host} className="text-xs bg-[var(--erased)] px-2 py-1">{host}</code>
                        ))}
                      </div>
                    </div>
                  )}

                  {selected.permissions.credential_scopes.length > 0 && (
                    <div className="p-3 border-2 border-[var(--ink)]/30 bg-[var(--surface)]">
                      <div className="flex items-center gap-2 font-heading font-bold text-sm">
                        <KeyRound className="w-4 h-4" /> Credential scopes
                      </div>
                      <div className="mt-1 flex flex-wrap gap-1">
                        {selected.permissions.credential_scopes.map((scope) => (
                          <code key={scope} className="text-xs bg-[var(--erased)] px-2 py-1">{scope}</code>
                        ))}
                      </div>
                    </div>
                  )}

                  {selected.permissions.credential_read && (
                    <div className="p-3 border-2 border-[var(--marker-red)] bg-[var(--tint-red)]">
                      <div className="flex items-center gap-2 font-heading font-bold text-sm text-[var(--danger-text)]">
                        <AlertTriangle className="w-4 h-4" />
                        High risk: plaintext credential read
                      </div>
                      <p className="text-sm mt-1 text-[var(--danger-text)]">
                        The plugin can receive plaintext credentials inside its WASM memory for approved scopes.
                      </p>
                    </div>
                  )}
                </div>

                <div className="mt-4 flex gap-2 flex-wrap">
                  {!fullyApproved && (
                    <SketchButton
                      variant="primary"
                      disabled={busy !== null}
                      onClick={() =>
                        void mutate(
                          'approve',
                          () => Kinetix.approvePluginPermissions(selected.id),
                          'Current manifest permissions approved.',
                        )
                      }
                      className="gap-2"
                    >
                      <ShieldCheck className="w-4 h-4" /> Approve current permissions
                    </SketchButton>
                  )}
                  {permissions?.approved.map((grant) => (
                    <SketchButton
                      key={grant.permission}
                      variant="secondary"
                      disabled={busy !== null}
                      onClick={() =>
                        void mutate(
                          `revoke:${grant.permission}`,
                          () => Kinetix.revokePluginPermission(selected.id, grant.permission),
                          `Revoked ${grant.permission}; plugin is disabled until permissions are approved again.`,
                        )
                      }
                    >
                      Revoke {grant.permission}
                    </SketchButton>
                  ))}
                </div>
              </WobblyCard>

              <WobblyCard variant="muted" className="p-5">
                <div className="flex flex-col md:flex-row md:items-center gap-4">
                  <div className="flex-1">
                    <h4 className="font-heading font-bold">Remove plugin</h4>
                    <p className="text-sm font-body text-[var(--ink)]/70">
                      Removes the installed plugin and its host-managed plugin state.
                    </p>
                  </div>
                  <SketchButton
                    variant="danger"
                    disabled={busy !== null}
                    onClick={() => {
                      if (!window.confirm(`Remove plugin ${selected.id}? This also removes its plugin state.`)) {
                        return;
                      }
                      void removePlugin(selected.id);
                    }}
                    className="gap-2"
                  >
                    <Trash2 className="w-4 h-4" /> Remove
                  </SketchButton>
                </div>
              </WobblyCard>
            </div>
          ) : (
            <WobblyCard variant="muted" className="p-5 text-sm font-body text-[var(--ink)]/70">
              Select an installed plugin to inspect its capabilities and permissions.
            </WobblyCard>
          )}
        </div>
      </div>
    </div>
  );
};
