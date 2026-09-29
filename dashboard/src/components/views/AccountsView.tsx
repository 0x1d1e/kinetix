import React, { useState } from 'react';
import { Users, Plus, ShieldCheck, Clock, AlertTriangle, RefreshCw, KeyRound, Sparkles, Trash2, Search, X, Sliders } from 'lucide-react';
import { Account, Provider } from '../../types';
import { WobblyCard, SketchButton, SketchBadge } from '../HandDrawnElements';
import { formatCurrency, formatTokens, DESIGN_TOKENS } from '../../lib/designSystem';
import { Kinetix, TestResult } from '../../lib/resources';
import { useAuthEnrollment } from '../CredentialAuthFlow';

interface AccountsViewProps {
  accounts: Account[];
  providers: Provider[];
  onAddAccount: (acc: Account & { apiKey?: string }) => void;
  onUpdateAccount: (acc: Account) => void;
  onDeleteAccount: (accountId: string) => void;
  onResetAccount: (accountId: string) => void;
  onRefresh: () => void | Promise<void>;
}

export const AccountsView: React.FC<AccountsViewProps> = ({
  accounts,
  providers,
  onAddAccount,
  onUpdateAccount,
  onDeleteAccount,
  onResetAccount,
  onRefresh,
}) => {
  const [showAddModal, setShowAddModal] = useState(false);
  const [searchQuery, setSearchQuery] = useState('');
  const [confirmDeleteAccountId, setConfirmDeleteAccountId] = useState<string | null>(null);
  const [providerFilter, setProviderFilter] = useState('all');
  const [editingAccount, setEditingAccount] = useState<Account | null>(null);
  const [editLabel, setEditLabel] = useState('');
  const [editPriority, setEditPriority] = useState(1);
  const [editWeight, setEditWeight] = useState(1);
  const [editQuota, setEditQuota] = useState('');
  const [editQuotaType, setEditQuotaType] = useState<Account['quotaType']>('none');
  const [label, setLabel] = useState('');
  const [providerId, setProviderId] = useState(providers[0]?.id || '');
  const [apiKey, setApiKey] = useState('');
  const [softQuota, setSoftQuota] = useState(100);
  const [accountValidation, setAccountValidation] = useState<{ valid: boolean; problems: string[] } | null>(null);
  const [validatingAccount, setValidatingAccount] = useState(false);
  const [testingAccountId, setTestingAccountId] = useState<string | null>(null);
  const [testResults, setTestResults] = useState<Record<string, TestResult>>({});
  const [enrollmentError, setEnrollmentError] = useState<string | null>(null);
  const [enrollmentNotice, setEnrollmentNotice] = useState<string | null>(null);

  const authEnrollment = useAuthEnrollment({
    onSuccess: async () => {
      setEnrollmentError(null);
      setEnrollmentNotice('Account connected successfully.');
      await onRefresh();
    },
    onError: (message) => {
      setEnrollmentNotice(null);
      setEnrollmentError(message);
    },
  });

  const handleValidateAccount = async () => {
    const prov = providers.find((p) => p.id === providerId) || providers[0];
    if (!label.trim() || !apiKey.trim() || !prov) return;
    setValidatingAccount(true);
    try {
      const r = await Kinetix.validateAccount({
        provider_id: prov.id,
        label: label.trim(),
        api_key: apiKey.trim(),
        quota_type: 'monthly',
      });
      setAccountValidation({ valid: r.valid, problems: r.problems || [] });
    } catch (e) {
      setAccountValidation({ valid: false, problems: [(e as Error).message] });
    } finally {
      setValidatingAccount(false);
    }
  };

  const handleCreateSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!label.trim() || !apiKey.trim()) return;

    const prov = providers.find((p) => p.id === providerId) || providers[0];
    const masked = apiKey.slice(0, 6) + '...' + apiKey.slice(-4);

    const newAcc: Account & { apiKey?: string } = {
      id: '',
      providerId: prov.id,
      providerName: prov.name,
      label: label.trim(),
      keyMasked: masked,
      status: 'healthy',
      quotaType: 'monthly',
      softQuotaSpendLimit: softQuota,
      currentSpend: 0,
      requestsCount: 0,
      tokensCount: 0,
      priority: accounts.filter((a) => a.providerId === prov.id).length + 1,
      weight: 1,
      apiKey: apiKey.trim(),
    };

    onAddAccount(newAcc);
    setShowAddModal(false);
    setLabel('');
    setApiKey('');
  };

  const probeAccount = async (acc: Account, resetOnSuccess = false) => {
    setTestingAccountId(acc.id);
    try {
      const result = await Kinetix.testAccount(acc.id);
      setTestResults((current) => ({ ...current, [acc.id]: result }));
      if (resetOnSuccess && result.ok) {
        onResetAccount(acc.id);
      }
    } catch (e) {
      setTestResults((current) => ({
        ...current,
        [acc.id]: {
          ok: false,
          status: 0,
          error: e instanceof Error ? e.message : String(e),
        },
      }));
    } finally {
      setTestingAccountId(null);
    }
  };

  const normalizedSearch = searchQuery.trim().toLowerCase();
  const filteredAccounts = accounts.filter((acc) => {
    if (!normalizedSearch) return true;
    return [acc.id, acc.label, acc.providerName, acc.keyMasked, acc.status]
      .some((value) => String(value ?? '').toLowerCase().includes(normalizedSearch));
  });

  const visibleProviders = providers.filter((provider) =>
    providerFilter === 'all' || provider.id === providerFilter,
  );
  const orphanAccounts = filteredAccounts.filter(
    (account) => !providers.some((provider) => provider.id === account.providerId),
  );

  const openAddForProvider = (id: string) => {
    setProviderId(id);
    setShowAddModal(true);
  };

  const openConfigure = (acc: Account) => {
    setEditingAccount(acc);
    setEditLabel(acc.label);
    setEditPriority(acc.priority);
    setEditWeight(acc.weight || 1);
    setEditQuota(acc.softQuotaSpendLimit == null ? '' : String(acc.softQuotaSpendLimit));
    setEditQuotaType(acc.quotaType);
  };

  const handleConfigureSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!editingAccount || !editLabel.trim() || editPriority < 1 || editWeight < 1) return;
    const quota = editQuota.trim() === '' ? undefined : Number(editQuota);
    if (quota !== undefined && (!Number.isFinite(quota) || quota < 0)) return;
    onUpdateAccount({
      ...editingAccount,
      label: editLabel.trim(),
      priority: Math.trunc(editPriority),
      weight: Math.trunc(editWeight),
      softQuotaSpendLimit: quota,
      quotaType: editQuotaType,
    });
    setEditingAccount(null);
  };

  return (
    <div className="space-y-6">
      {/* Top Banner */}
      <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Key Pool & Accounts Health</span>
            <SketchBadge variant="yellow" rotation="-1deg">
              FR-4 & FR-12
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Accounts represent provider credentials when a provider needs them. Manual keys, sign-in flows, and credential-free providers use separate enrollment paths.
          </p>
        </div>

        <div className="flex flex-col sm:flex-row gap-2 w-full md:w-auto">
          <div className="relative min-w-0 sm:w-72">
            <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-[var(--ink)]/50" />
            <input
              type="search"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search accounts…"
              className="w-full pl-9 pr-9 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
              style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
            />
            {searchQuery && (
              <button
                type="button"
                onClick={() => setSearchQuery('')}
                className="absolute right-2 top-1/2 -translate-y-1/2 p-1 text-[var(--ink)]/60 hover:text-[var(--marker-red)] cursor-pointer"
                title="Clear search"
              >
                <X className="w-4 h-4" />
              </button>
            )}
          </div>
          {providers.some((provider) => provider.credentialMode === 'manual') && (
            <SketchButton
              variant="primary"
              size="md"
              onClick={() => {
                const provider = providers.find((item) => item.credentialMode === 'manual');
                if (provider) openAddForProvider(provider.id);
              }}
              className="gap-2 font-heading font-bold whitespace-nowrap"
            >
              <Plus className="w-5 h-5" />
              Add API Key
            </SketchButton>
          )}
        </div>
      </div>

      {authEnrollment.modal}
      {enrollmentError && (
        <div className="p-3 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] text-sm font-mono text-[var(--danger-text)]">
          {enrollmentError}
        </div>
      )}
      {enrollmentNotice && (
        <div className="p-3 bg-[var(--tint-green)] border-2 border-[var(--pen-green)] text-sm font-mono text-[var(--success-text)]">
          {enrollmentNotice}
        </div>
      )}

      {providers.length > 1 && (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-mono text-[var(--ink)]/60">Pool:</span>
          <button
            onClick={() => setProviderFilter('all')}
            className={`px-3 py-1.5 text-xs font-heading font-bold border border-[var(--ink)] rounded cursor-pointer ${
              providerFilter === 'all' ? 'bg-[var(--postit)] sketch-shadow-sm' : 'bg-[var(--surface)]'
            }`}
          >
            All Providers
          </button>
          {providers.map((provider) => (
            <button
              key={provider.id}
              onClick={() => setProviderFilter(provider.id)}
              className={`px-3 py-1.5 text-xs font-heading font-bold border border-[var(--ink)] rounded cursor-pointer ${
                providerFilter === provider.id ? 'bg-[var(--postit)] sketch-shadow-sm' : 'bg-[var(--surface)]'
              }`}
            >
              {provider.name}
            </button>
          ))}
        </div>
      )}

      {/* Provider account pools */}
      {normalizedSearch && filteredAccounts.length === 0 && accounts.length > 0 ? (
        <WobblyCard decoration="tack" className="p-8 text-center bg-[var(--surface)]">
          <Search className="w-10 h-10 text-[var(--ink)]/35 mx-auto mb-2" />
          <p className="font-heading font-bold text-lg">No accounts match “{searchQuery}”.</p>
          <button
            onClick={() => setSearchQuery('')}
            className="mt-2 text-sm font-heading font-bold text-[var(--pen-blue)] hover:underline cursor-pointer"
          >
            Clear search
          </button>
        </WobblyCard>
      ) : (
        <div className="space-y-8">
          {visibleProviders.map((provider) => {
            const poolAccounts = filteredAccounts.filter((acc) => acc.providerId === provider.id);
            const healthy = poolAccounts.filter((acc) => acc.status === 'healthy').length;
            const cooldown = poolAccounts.filter((acc) => acc.status === 'cooldown').length;
            const exhausted = poolAccounts.filter((acc) => acc.status === 'exhausted').length;
            const disabled = poolAccounts.filter((acc) => acc.status === 'disabled').length;
            const degraded = poolAccounts.filter((acc) => acc.status === 'degraded').length;
            return (
              <section key={provider.id} className="space-y-4">
                <div className="flex flex-wrap items-center justify-between gap-3 border-b-2 border-dashed border-[var(--ink)]/25 pb-2">
                  <div>
                    <h3 className="text-xl font-heading font-bold text-[var(--ink)]">{provider.name} Pool</h3>
                    <p className="text-xs font-mono text-[var(--ink)]/65">
                      {poolAccounts.length} credential(s) · {healthy} healthy · {cooldown} cooldown · {exhausted} exhausted · {degraded} degraded · {disabled} disabled
                    </p>
                  </div>
                  {provider.credentialMode === 'manual' ? (
                    <SketchButton variant="secondary" size="sm" onClick={() => openAddForProvider(provider.id)} className="gap-1">
                      <Plus className="w-4 h-4" />
                      {provider.credentialEnrollment.actionLabel || 'Add API Key'}
                    </SketchButton>
                  ) : provider.credentialMode === 'auth_flow' ? (
                    <SketchButton
                      variant="secondary"
                      size="sm"
                      disabled={!provider.credentialEnrollment.available || authEnrollment.busy}
                      onClick={() => {
                        setEnrollmentError(null);
                        setEnrollmentNotice(null);
                        void authEnrollment.begin(
                          provider.id,
                          () => Kinetix.startProviderCredentialEnrollment(provider.id),
                        );
                      }}
                      className="gap-1"
                    >
                      <KeyRound className="w-4 h-4" />
                      {provider.credentialEnrollment.actionLabel || 'Connect account'}
                    </SketchButton>
                  ) : (
                    <SketchBadge variant="green">No credential required</SketchBadge>
                  )}
                </div>

                {poolAccounts.length === 0 ? (
                  <div className="p-5 text-sm font-body text-[var(--ink)]/65 bg-[var(--surface)] border-2 border-dashed border-[var(--ink)]/25 rounded">
                    {provider.credentialMode === 'none'
                      ? 'No credential required.'
                      : provider.credentialMode === 'auth_flow'
                        ? provider.credentialEnrollment.available
                          ? 'Connect an account to use this provider.'
                          : 'Authentication plugin is unavailable. Manual API-key entry is disabled.'
                        : 'No credentials in this provider pool.'}
                  </div>
                ) : (
                  <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                  {poolAccounts.map((acc, idx) => {
                    const isCooldown = acc.status === 'cooldown';
                    const isExhausted = acc.status === 'exhausted';
                    const isDisabled = acc.status === 'disabled';
                    const isDegraded = acc.status === 'degraded';
                    const rotation = idx % 2 === 0 ? '-0.5deg' : '0.5deg';

                    return (
              <WobblyCard
                key={acc.id}
                decoration={isCooldown ? 'tack' : idx % 2 === 0 ? 'tape' : 'none'}
                rotation={rotation}
                className={`p-5 flex flex-col justify-between ${
                  isCooldown || isDisabled
                    ? 'bg-[var(--tint-red)]'
                    : isExhausted || isDegraded
                      ? 'bg-[var(--tint-yellow)]'
                      : 'bg-[var(--surface)]'
                }`}
              >
                <div>
                  <div className="flex items-start justify-between gap-2 mb-2">
                    <div>
                      <span className="text-xs font-mono bg-[var(--erased)] px-2 py-0.5 rounded border border-[var(--ink)]/30 inline-block mb-1">
                        {acc.providerName}
                      </span>
                      <h3 className="text-xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
                        <KeyRound className="w-5 h-5 text-[var(--pen-blue)]" />
                        {acc.label}
                      </h3>
                    </div>

                    {acc.status === 'healthy' ? (
                      <SketchBadge variant="green" rotation="1deg">
                        Healthy
                      </SketchBadge>
                    ) : acc.status === 'cooldown' ? (
                      <SketchBadge variant="red" rotation="-1deg">
                        In Cooldown (429)
                      </SketchBadge>
                    ) : acc.status === 'exhausted' ? (
                      <SketchBadge variant="yellow" rotation="1deg">
                        Quota Exhausted
                      </SketchBadge>
                    ) : acc.status === 'disabled' ? (
                      <SketchBadge variant="red" rotation="1deg">
                        Disabled
                      </SketchBadge>
                    ) : (
                      <SketchBadge variant="yellow" rotation="1deg">
                        Degraded
                      </SketchBadge>
                    )}
                  </div>

                  {acc.status !== 'healthy' && (
                    <div className="mb-4 rounded border border-[var(--ink)]/25 bg-[var(--surface)]/75 px-3 py-2 text-xs font-mono text-[var(--ink)]/75">
                      <div>Reason: <strong>{acc.statusReason || 'unknown'}</strong></div>
                      {acc.retryAt && <div>Retry at: <time dateTime={acc.retryAt}>{new Date(acc.retryAt).toLocaleString()}</time></div>}
                      {acc.statusChangedAt && <div>Changed: <time dateTime={acc.statusChangedAt}>{new Date(acc.statusChangedAt).toLocaleString()}</time></div>}
                    </div>
                  )}

                  {/* Key masked preview */}
                  <div className="flex items-center justify-between bg-[var(--paper)] p-2 border-2 border-dashed border-[var(--ink)] text-xs font-mono mb-4">
                    <span>Masked Secret: <strong>{acc.keyMasked}</strong></span>
                    <span className="text-[var(--pen-green)] font-bold">🔒 Encrypted at rest</span>
                  </div>

                  {/* Cooldown / Quota warning notice */}
                  {isCooldown && (
                    <div className="p-3 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] sketch-shadow-sm mb-4 rounded text-xs font-mono text-[var(--danger-text)]">
                      <div className="flex items-center gap-1.5 font-bold mb-1">
                        <AlertTriangle className="w-4 h-4" />
                        <span>{acc.lastError || 'Rate Limit (HTTP 429)'}</span>
                      </div>
                      <button
                        onClick={() => void probeAccount(acc, true)}
                        disabled={testingAccountId === acc.id}
                        className="mt-2 px-2 py-1 bg-[var(--surface)] border border-[var(--ink)] text-[var(--ink)] hover:bg-[var(--erased)] rounded flex items-center gap-1 cursor-pointer font-bold disabled:opacity-50"
                      >
                        <RefreshCw className={`w-3 h-3 ${testingAccountId === acc.id ? 'animate-spin' : ''}`} />
                        {testingAccountId === acc.id ? 'Testing…' : 'Test & Clear Cooldown'}
                      </button>
                    </div>
                  )}

                  {testResults[acc.id] && (
                    <div
                      className={`mb-4 p-3 border-2 rounded text-xs font-mono ${
                        testResults[acc.id].ok
                          ? 'bg-[var(--tint-green)] border-[var(--pen-green)]'
                          : 'bg-[var(--tint-red)] border-[var(--marker-red)]'
                      }`}
                    >
                      <div className="font-bold">
                        {testResults[acc.id].ok ? 'Proxy test passed' : 'Proxy test failed'}
                        {testResults[acc.id].status ? ` · HTTP ${testResults[acc.id].status}` : ''}
                        {testResults[acc.id].latency_ms != null ? ` · ${testResults[acc.id].latency_ms}ms` : ''}
                      </div>
                      {testResults[acc.id].error && <div className="mt-1 break-words">{testResults[acc.id].error}</div>}
                      {testResults[acc.id].response_preview && (
                        <div className="mt-1 text-[var(--ink)]/70 break-words">{testResults[acc.id].response_preview}</div>
                      )}
                    </div>
                  )}

                  {/* Soft Quotas & Statistics */}
                  <div className="grid grid-cols-2 gap-2 text-xs font-mono bg-[var(--surface)] p-3 border border-[var(--ink)] rounded mb-4">
                    <div>
                      Requests Served: <strong>{acc.requestsCount.toLocaleString()}</strong>
                    </div>
                    <div>
                      Tokens Processed: <strong>{formatTokens(acc.tokensCount)}</strong>
                    </div>
                    <div>
                      Spend Accrued: <strong>{formatCurrency(acc.currentSpend)}</strong>
                    </div>
                    <div>
                      Soft Quota Cap:{' '}
                      <strong>
                        {acc.softQuotaSpendLimit ? formatCurrency(acc.softQuotaSpendLimit) : 'Unlimited'}
                      </strong>
                    </div>
                    {acc.quotaResetTime && (
                      <div className="col-span-2 text-[var(--pen-blue)] pt-1 border-t border-[var(--ink)]/20">
                        Quota Reset: <strong>{acc.quotaResetTime}</strong>
                      </div>
                    )}
                  </div>
                </div>

                {/* Footer */}
                <div className="flex flex-wrap items-center justify-between gap-2 border-t border-[var(--ink)]/20 pt-3 text-xs font-mono">
                  <div className="text-[var(--ink)]/70">
                    <span>Provider Priority: <strong>Tier #{acc.priority}</strong></span>
                  </div>

                  <div className="flex items-center gap-2 ml-auto">
                    <button
                      onClick={() => openConfigure(acc)}
                      className="px-2 py-1 text-xs font-heading font-bold text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-[var(--pen-blue)]/40 hover:border-[var(--pen-blue)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                      title="Configure this account"
                    >
                      <Sliders className="w-3.5 h-3.5" />
                      <span>Configure</span>
                    </button>
                    <button
                      onClick={() => void probeAccount(acc)}
                      disabled={testingAccountId === acc.id}
                      className="px-2 py-1 text-xs font-heading font-bold text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-[var(--pen-blue)]/40 hover:border-[var(--pen-blue)] rounded flex items-center gap-1 cursor-pointer transition-colors disabled:opacity-50"
                      title="Send a minimal real proxy request through this account"
                    >
                      <RefreshCw className={`w-3.5 h-3.5 ${testingAccountId === acc.id ? 'animate-spin' : ''}`} />
                      <span>{testingAccountId === acc.id ? 'Testing…' : 'Test Proxy'}</span>
                    </button>

                  {confirmDeleteAccountId === acc.id ? (
                    <div className="flex items-center gap-1 bg-[var(--tint-red)] px-2 py-1 border border-[var(--marker-red)] rounded text-xs font-heading">
                      <span className="text-[var(--danger-text)] font-bold">Remove pool key?</span>
                      <button
                        onClick={() => {
                          onDeleteAccount(acc.id);
                          setConfirmDeleteAccountId(null);
                        }}
                        className="px-2 py-0.5 bg-[var(--marker-red)] text-[var(--surface)] rounded font-bold hover:brightness-90 cursor-pointer"
                      >
                        Confirm
                      </button>
                      <button
                        onClick={() => setConfirmDeleteAccountId(null)}
                        className="px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded hover:bg-[var(--erased)] cursor-pointer"
                      >
                        Cancel
                      </button>
                    </div>
                  ) : (
                    <button
                      onClick={() => setConfirmDeleteAccountId(acc.id)}
                      className="px-2 py-1 text-xs font-heading font-bold text-[var(--marker-red)] hover:bg-[var(--tint-red)] border border-[var(--marker-red)]/40 hover:border-[var(--marker-red)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                      title="Remove this credential account from pool"
                    >
                      <Trash2 className="w-3.5 h-3.5" />
                      <span>Remove Pool Key</span>
                    </button>
                  )}
                  </div>
                </div>
              </WobblyCard>
                    );
                  })}
                  </div>
                )}
              </section>
            );
          })}

          {providerFilter === 'all' && orphanAccounts.length > 0 && (
            <section className="space-y-3">
              <h3 className="text-xl font-heading font-bold text-[var(--ink)]">Unknown Provider Pool</h3>
              <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                {orphanAccounts.map((acc) => (
                  <WobblyCard key={acc.id} className="p-5 bg-[var(--surface)]">
                    <h4 className="font-heading font-bold">{acc.label}</h4>
                    <p className="text-xs font-mono">{acc.providerId} · Tier #{acc.priority} · {acc.status}</p>
                  </WobblyCard>
                ))}
              </div>
            </section>
          )}
        </div>
      )}

      {editingAccount && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-lg">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setEditingAccount(null)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>
              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
                <Sliders className="w-6 h-6 text-[var(--pen-blue)]" />
                Configure Account
              </h3>
              <form onSubmit={handleConfigureSubmit} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Account Label
                  </label>
                  <input
                    autoFocus
                    type="text"
                    required
                    value={editLabel}
                    onChange={(e) => setEditLabel(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                  {!editLabel.trim() && (
                    <p className="text-xs text-[var(--danger-text)] mt-1">Label cannot be blank.</p>
                  )}
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">Priority Tier</label>
                    <input
                      type="number"
                      min={1}
                      step={1}
                      value={editPriority}
                      onChange={(e) => setEditPriority(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2"
                    />
                  </div>
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">Weight</label>
                    <input
                      type="number"
                      min={1}
                      step={1}
                      value={editWeight}
                      onChange={(e) => setEditWeight(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2"
                    />
                  </div>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">Soft Quota (USD)</label>
                    <input
                      type="number"
                      min={0}
                      step="0.01"
                      placeholder="Unlimited"
                      value={editQuota}
                      onChange={(e) => setEditQuota(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2"
                    />
                  </div>
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">Quota Reset</label>
                    <select
                      value={editQuotaType}
                      onChange={(e) => setEditQuotaType(e.target.value as Account['quotaType'])}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2"
                    >
                      <option value="none">None</option>
                      <option value="daily">Daily</option>
                      <option value="monthly">Monthly</option>
                      <option value="rolling">Rolling</option>
                    </select>
                  </div>
                </div>

                <div className="flex justify-end gap-3 pt-2">
                  <SketchButton type="button" variant="ghost" onClick={() => setEditingAccount(null)}>
                    Cancel
                  </SketchButton>
                  <SketchButton
                    type="submit"
                    variant="primary"
                    disabled={!editLabel.trim() || editPriority < 1 || editWeight < 1 || (editQuota.trim() !== '' && Number(editQuota) < 0)}
                  >
                    Save Changes
                  </SketchButton>
                </div>
              </form>
            </WobblyCard>
          </div>
        </div>
      )}

      {/* Add Account Modal */}
      {showAddModal && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-lg">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setShowAddModal(false)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>

              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
                <KeyRound className="w-6 h-6 text-[var(--pen-blue)]" />
                Add Upstream Provider Credential
              </h3>

              <form onSubmit={handleCreateSubmit} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Provider
                  </label>
                  <select
                    value={providerId}
                    onChange={(e) => setProviderId(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-body sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  >
                    {providers.filter((p) => p.credentialMode === 'manual').map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name} ({p.wireFormat})
                      </option>
                    ))}
                  </select>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Account Label / Identification
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. Gemini Team Pay-as-you-go #2"
                    value={label}
                    onChange={(e) => setLabel(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Raw API Key (Encrypted immediately on storage)
                  </label>
                  <input
                    type="password"
                    required
                    placeholder="AIzaSy... or sk-..."
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                  <p className="text-xs text-[var(--ink)]/60 mt-1">
                    Upstream keys never leave the server or appear in client responses.
                  </p>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Soft Quota Spend Limit (USD/month)
                  </label>
                  <input
                    type="number"
                    value={softQuota}
                    onChange={(e) => setSoftQuota(Number(e.target.value))}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyBtn }}
                  />
                </div>

                <div className="pt-2 flex justify-end gap-3">
                  <SketchButton
                    type="button"
                    variant="ghost"
                    onClick={() => setShowAddModal(false)}
                  >
                    Cancel
                  </SketchButton>
                  <SketchButton
                    type="button"
                    variant="secondary"
                    onClick={handleValidateAccount}
                    disabled={validatingAccount || !label.trim() || !apiKey.trim()}
                  >
                    {validatingAccount ? 'Validating…' : 'Validate (Dry Run)'}
                  </SketchButton>
                  <SketchButton type="submit" variant="danger" className="font-bold">
                    Save Key to Pool
                  </SketchButton>
                </div>
                {accountValidation && (
                  <div
                    className="mt-3 p-3 text-sm font-mono"
                    style={{
                      borderRadius: DESIGN_TOKENS.radii.wobbly,
                      background: accountValidation.valid ? 'var(--tint-green)' : 'var(--tint-red)',
                      border: `2px solid ${accountValidation.valid ? 'var(--pen-green)' : 'var(--marker-red)'}`,
                    }}
                  >
                    <div className="font-bold mb-1">
                      {accountValidation.valid ? 'Validate: passed' : 'Validate: problems found'}
                    </div>
                    {accountValidation.problems.map((p, i) => (
                      <div key={i} style={{ color: 'var(--danger-text)' }}>• {p}</div>
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
