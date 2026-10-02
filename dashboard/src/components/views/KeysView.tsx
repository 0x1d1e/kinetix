import React, { useState } from 'react';
import { Key, Plus, Copy, Check, ShieldAlert, Sparkles, Trash2, Power, Search, X } from 'lucide-react';
import { VirtualKey } from '../../types';
import { WobblyCard, SketchButton, SketchBadge } from '../HandDrawnElements';
import { formatCurrency, formatTokens } from '../../lib/designSystem';
import { useConfirm } from '../../lib/useConfirm';
import ClientProfileGenerator from './ClientProfileGenerator';
import { FirstRunGuide } from '../FirstRunGuide';

interface KeysViewProps {
  keys: VirtualKey[];
  onAddKey: (newKey: VirtualKey) => Promise<{ key: VirtualKey; fullKey: string } | null>;
  onUpdateKeyStatus: (id: string, status: 'active' | 'disabled' | 'revoked') => void;
  onUpdateKeyIps: (id: string, ips: string[]) => void;
  onDeleteKey?: (id: string) => void;
  showSetupGuide?: boolean;
  providerCount?: number;
  modelCount?: number;
  onOpenProviders?: () => void;
}

/** Inline per-key IP allowlist editor (FR-3.4). Empty means "no restriction". */
const IpAllowlistEditor: React.FC<{
  keyId: string;
  current: string[];
  onSave: (id: string, ips: string[]) => void;
}> = ({ keyId, current, onSave }) => {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(current.join(', '));
  return (
    <div className="col-span-2 pt-1 border-t border-[var(--ink)]/15 flex flex-wrap items-center gap-2">
      <span>
        IP Allowlist:{' '}
        <strong className="text-[var(--pen-blue)] font-mono">
          {current.length > 0 ? current.join(', ') : 'any'}
        </strong>
      </span>
      {editing ? (
        <span className="flex items-center gap-1">
          <input
            value={text}
            onChange={(e) => setText(e.target.value)}
            placeholder="203.0.113.0/24, 198.51.100.7"
            className="px-2 py-0.5 text-xs font-mono border border-[var(--ink)] rounded bg-[var(--surface)] w-64"
          />
          <button
            onClick={() => {
              const ips = text
                .split(',')
                .map((s) => s.trim())
                .filter(Boolean);
              onSave(keyId, ips);
              setEditing(false);
            }}
            className="px-2 py-0.5 text-xs font-heading font-bold border border-[var(--ink)] bg-[var(--tint-green)] rounded cursor-pointer"
          >
            Save
          </button>
          <button
            onClick={() => {
              setText(current.join(', '));
              setEditing(false);
            }}
            className="px-2 py-0.5 text-xs font-heading border border-[var(--ink)]/40 bg-[var(--surface)] rounded cursor-pointer"
          >
            Cancel
          </button>
        </span>
      ) : (
        <button
          onClick={() => setEditing(true)}
          className="px-2 py-0.5 text-xs font-heading font-bold border border-[var(--ink)]/40 bg-[var(--surface)] hover:bg-[var(--postit)] rounded cursor-pointer"
          title="Edit the per-key IP allowlist (empty = any)"
        >
          Edit
        </button>
      )}
    </div>
  );
};

export const KeysView: React.FC<KeysViewProps> = ({
  keys,
  onAddKey,
  onUpdateKeyStatus,
  onUpdateKeyIps,
  onDeleteKey,
  showSetupGuide = false,
  providerCount = 0,
  modelCount = 0,
  onOpenProviders = () => {},
}) => {
  const { confirm, confirmNode } = useConfirm();
  const [showCreateModal, setShowCreateModal] = useState(false);
  const [copiedKeyId, setCopiedKeyId] = useState<string | null>(null);
  const [newlyCreatedKey, setNewlyCreatedKey] = useState<{ id: string; name: string; key: string } | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState('');

  // Form state
  const [name, setName] = useState('');
  const [owner, setOwner] = useState('');
  const [tag, setTag] = useState('');
  const [allowedModels, setAllowedModels] = useState('*');
  const [rpmLimit, setRpmLimit] = useState(60);
  const [tpmLimit, setTpmLimit] = useState(100000);
  const [maxConcurrentRequests, setMaxConcurrentRequests] = useState('');
  const [dailyBudget, setDailyBudget] = useState(15.0);
  const [monthlyBudget, setMonthlyBudget] = useState(60.0);

  const handleCopy = (keyText: string, id: string) => {
    navigator.clipboard.writeText(keyText);
    setCopiedKeyId(id);
    setTimeout(() => setCopiedKeyId(null), 2000);
  };

  const handleCreateSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setIsSubmitting(true);
    setError(null);

    const newKeyObj: VirtualKey = {
      id: '',
      key: '',
      name: name.trim() || 'Untitled Key',
      owner: owner.trim() || 'Team Member',
      tag: tag.trim() || 'general',
      allowedModels: allowedModels
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean),
      allowedProviders: [],
      rpmLimit: Number(rpmLimit) || 0,
      tpmLimit: Number(tpmLimit) || 0,
      maxConcurrentRequests: maxConcurrentRequests === '' ? null : Number(maxConcurrentRequests),
      dailyBudget: Number(dailyBudget) || 0,
      monthlyBudget: Number(monthlyBudget) || 0,
      currentDailySpend: 0,
      currentMonthlySpend: 0,
      createdAt: new Date().toISOString(),
      expiresAt: null,
      status: 'active',
      totalRequests: 0,
      totalTokens: 0,
    };

    const result = await onAddKey(newKeyObj);
    setIsSubmitting(false);
    if (!result) {
      setError('Failed to create key. See the banner for details.');
      return;
    }
    // The server returns the full key exactly once (FR-3.1).
    setNewlyCreatedKey({ id: result.key.id, name: result.key.name, key: result.fullKey });
    setShowCreateModal(false);
    setName('');
    setOwner('');
    setTag('');
    setMaxConcurrentRequests('');
  };

  const normalizedSearch = searchQuery.trim().toLowerCase();
  const filteredKeys = keys.filter((k) => {
    if (!normalizedSearch) return true;
    return [
      k.id,
      k.name,
      k.owner,
      k.tag,
      k.key,
      k.status,
      ...k.allowedModels,
    ].some((value) => String(value ?? '').toLowerCase().includes(normalizedSearch));
  });

  return (
    <div className="space-y-6">
      {confirmNode}
      {showSetupGuide && (
        <FirstRunGuide
          providerCount={providerCount}
          modelCount={modelCount}
          keyCount={keys.length}
          onOpenProviders={onOpenProviders}
          onCreateKey={() => setShowCreateModal(true)}
        />
      )}
      {/* Top Banner & Action */}
      <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Virtual Keys & Client Access</span>
            <SketchBadge variant="yellow" rotation="-1deg">
              {keys.filter((k) => k.status === 'active').length} Active
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Issue scoped credentials for developers and coding tools like Pi. Real upstream keys never leave Kinetix.
          </p>
        </div>

        <div className="flex flex-col sm:flex-row gap-2 w-full md:w-auto">
          <div className="relative min-w-0 sm:w-72">
            <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-[var(--ink)]/50" />
            <input
              type="search"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search virtual keys…"
              className="w-full pl-9 pr-9 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
            />
            {searchQuery && (
              <button type="button" onClick={() => setSearchQuery('')} className="absolute right-2 top-1/2 -translate-y-1/2 p-1 text-[var(--ink)]/60 hover:text-[var(--marker-red)] cursor-pointer" title="Clear search">
                <X className="w-4 h-4" />
              </button>
            )}
          </div>
          <SketchButton
            variant="primary"
            size="md"
            onClick={() => setShowCreateModal(true)}
            className="gap-2 font-heading font-bold whitespace-nowrap"
          >
            <Plus className="w-5 h-5" />
            Issue New Virtual Key
          </SketchButton>
        </div>
      </div>

      {/* Newly Created Key Alert (Shown once!) */}
      {newlyCreatedKey && (
        <WobblyCard decoration="tape" variant="postit" className="p-4 bg-[var(--postit)]">
          <div className="flex items-start justify-between gap-4">
            <div>
              <div className="flex items-center gap-2 mb-1">
                <Sparkles className="w-5 h-5 text-[var(--marker-orange)]" />
                <h3 className="font-heading font-bold text-xl text-[var(--ink)]">
                  New Virtual Key Generated for {newlyCreatedKey.name}!
                </h3>
              </div>
              <p className="text-sm font-body text-[var(--ink)]/80 mb-2">
                Copy this key now! In accordance with security requirements, this raw key is never shown again in full.
              </p>
              <div className="flex items-center gap-2 bg-[var(--surface)] px-3 py-2 border-2 border-[var(--ink)] font-mono text-sm sketch-shadow-sm select-all">
                <code className="text-[var(--ink)] font-bold">{newlyCreatedKey.key}</code>
                <button
                  onClick={() => handleCopy(newlyCreatedKey.key, 'newly-created')}
                  className="ml-auto p-1.5 hover:bg-[var(--erased)] border border-[var(--ink)] rounded cursor-pointer"
                >
                  {copiedKeyId === 'newly-created' ? (
                    <Check className="w-4 h-4 text-[var(--pen-green)]" />
                  ) : (
                    <Copy className="w-4 h-4 text-[var(--ink)]" />
                  )}
                </button>
              </div>
            </div>
            <button
              onClick={() => setNewlyCreatedKey(null)}
              className="text-[var(--ink)] font-bold hover:text-[var(--marker-red)] cursor-pointer"
            >
              ✕
            </button>
          </div>
        </WobblyCard>
      )}

      {/* Keys List Grid */}
      <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
        {keys.length > 0 && filteredKeys.length === 0 && (
          <WobblyCard decoration="tack" className="md:col-span-2 p-8 text-center bg-[var(--surface)]">
            <Search className="w-10 h-10 text-[var(--ink)]/35 mx-auto mb-2" />
            <p className="font-heading font-bold text-lg">No keys match “{searchQuery}”.</p>
            <button onClick={() => setSearchQuery('')} className="mt-2 text-sm font-heading font-bold text-[var(--pen-blue)] hover:underline cursor-pointer">
              Clear search
            </button>
          </WobblyCard>
        )}
        {filteredKeys.map((k, idx) => {
          const rotation = idx % 2 === 0 ? '-0.5deg' : '0.5deg';
          const dailyPct =
            k.dailyBudget > 0
              ? Math.min(100, Math.round((k.currentDailySpend / k.dailyBudget) * 100))
              : 0;
          const monthlyPct =
            k.monthlyBudget > 0
              ? Math.min(100, Math.round((k.currentMonthlySpend / k.monthlyBudget) * 100))
              : 0;

          return (
            <WobblyCard
              key={k.id}
              decoration={idx % 3 === 0 ? 'tape' : idx % 3 === 1 ? 'tack' : 'none'}
              rotation={rotation}
              className="flex flex-col justify-between"
            >
              <div>
                {/* Header of Key Card */}
                <div className="flex items-start justify-between gap-2 mb-2">
                  <div>
                    <h3 className="text-xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
                      <Key className="w-5 h-5 text-[var(--pen-blue)]" />
                      {k.name}
                    </h3>
                    <p className="text-sm font-body text-[var(--ink)]/70">
                      Owner: <strong>{k.owner}</strong> • Tag:{' '}
                      <span className="font-mono text-xs bg-[var(--erased)] px-1.5 py-0.5 rounded border border-[var(--ink)]/40">
                        {k.tag}
                      </span>
                    </p>
                  </div>

                  {k.status === 'active' ? (
                    <SketchBadge variant="green" rotation="1deg">
                      Active
                    </SketchBadge>
                  ) : k.status === 'disabled' ? (
                    <SketchBadge variant="yellow" rotation="-1deg">
                      Disabled
                    </SketchBadge>
                  ) : (
                    <SketchBadge variant="red" rotation="1deg">
                      Revoked
                    </SketchBadge>
                  )}
                </div>

                {/* Key masked string (raw keys are shown once at creation) */}
                <div className="flex items-center gap-2 bg-[var(--paper)] p-2 border-2 border-dashed border-[var(--ink)] mb-4 text-xs font-mono">
                  <span className="truncate flex-1">
                    {k.key}
                  </span>
                  <span className="text-[10px] text-[var(--ink)]/50 shrink-0">shown once</span>
                </div>

                {/* Limits & Budgets */}
                <div className="space-y-3 text-sm font-body mb-4">
                  <div>
                    <div className="flex justify-between text-xs font-heading font-bold mb-1">
                      <span>Daily Spend: {formatCurrency(k.currentDailySpend)}</span>
                      <span>Limit: {formatCurrency(k.dailyBudget)} ({dailyPct}%)</span>
                    </div>
                    <div className="w-full h-3 bg-[var(--erased)] border-2 border-[var(--ink)] rounded-full overflow-hidden">
                      <div
                        className={`h-full border-r-2 border-[var(--ink)] ${
                          dailyPct > 80 ? 'bg-[var(--marker-red)]' : 'bg-[var(--pen-blue)]'
                        }`}
                        style={{ width: `${dailyPct}%` }}
                      />
                    </div>
                  </div>

                  <div>
                    <div className="flex justify-between text-xs font-heading font-bold mb-1">
                      <span>Monthly Spend: {formatCurrency(k.currentMonthlySpend)}</span>
                      <span>Cap: {formatCurrency(k.monthlyBudget)} ({monthlyPct}%)</span>
                    </div>
                    <div className="w-full h-3 bg-[var(--erased)] border-2 border-[var(--ink)] rounded-full overflow-hidden">
                      <div
                        className={`h-full border-r-2 border-[var(--ink)] ${
                          monthlyPct > 80 ? 'bg-[var(--marker-red)]' : 'bg-[var(--pen-green)]'
                        }`}
                        style={{ width: `${monthlyPct}%` }}
                      />
                    </div>
                  </div>

                  <div className="grid grid-cols-2 gap-2 text-xs font-mono bg-[var(--surface)] p-2.5 border border-[var(--ink)] rounded">
                    <div>
                      Rate Limit: <strong>{k.rpmLimit} RPM</strong>
                    </div>
                    <div>
                      Tokens: <strong>{formatTokens(k.tpmLimit)} TPM</strong>
                    </div>
                    <div className="col-span-2">
                      Concurrency: <strong>{k.maxConcurrentRequests ?? 'unlimited'}</strong>
                    </div>
                    <div className="col-span-2 pt-1 border-t border-[var(--ink)]/15">
                      Allowed Models:{' '}
                      <strong className="text-[var(--pen-blue)]">{k.allowedModels.join(', ')}</strong>
                    </div>
                    <IpAllowlistEditor
                      keyId={k.id}
                      current={k.allowedIps ?? []}
                      onSave={onUpdateKeyIps}
                    />
                  </div>
                </div>
              </div>

              {/* Actions Footer */}
              <div className="flex flex-wrap items-center justify-between gap-2 border-t-2 border-dashed border-[var(--ink)]/30 pt-3 mt-2">
                <span className="text-xs font-mono text-[var(--ink)]/60">
                  Total: {k.totalRequests.toLocaleString()} reqs • {formatTokens(k.totalTokens)} tok
                </span>

                <div className="flex items-center gap-2 shrink-0">
                  {k.status === 'active' ? (
                    <button
                      onClick={() => onUpdateKeyStatus(k.id, 'disabled')}
                      className="px-2 py-1 text-xs font-heading font-bold border border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--postit)] rounded flex items-center gap-1 cursor-pointer whitespace-nowrap"
                      title="Temporarily disable key"
                    >
                      <Power className="w-3.5 h-3.5 text-[var(--marker-orange)]" />
                      Disable
                    </button>
                  ) : (
                    <button
                      onClick={() => onUpdateKeyStatus(k.id, 'active')}
                      className="px-2 py-1 text-xs font-heading font-bold border border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--tint-green)] rounded flex items-center gap-1 cursor-pointer whitespace-nowrap"
                      title="Activate key"
                    >
                      <Power className="w-3.5 h-3.5 text-[var(--pen-green)]" />
                      Activate
                    </button>
                  )}

                  <button
                    onClick={async () => {
                      const ok = await confirm({
                        title: `Revoke and delete "${k.name}"?`,
                        message:
                          'The key is permanently removed from the database and its usage logs are deleted. Any client using it will immediately receive 401.',
                        confirmLabel: 'Revoke & Delete',
                        danger: true,
                      });
                      if (!ok) return;
                      if (onDeleteKey) onDeleteKey(k.id);
                      else onUpdateKeyStatus(k.id, 'revoked');
                    }}
                    className="px-2 py-1 text-xs font-heading font-bold border border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--marker-red)] hover:text-[var(--surface)] rounded flex items-center gap-1 cursor-pointer whitespace-nowrap"
                    title="Permanently revoke and delete key"
                  >
                    <Trash2 className="w-3.5 h-3.5" />
                    Revoke
                  </button>
                </div>
              </div>
            </WobblyCard>
          );
        })}
      </div>

      <ClientProfileGenerator keys={keys} newlyCreatedKey={newlyCreatedKey} />

      {/* Create Modal */}
      {showCreateModal && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-lg">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setShowCreateModal(false)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>

              <h3 className="text-2xl font-heading font-bold text-[var(--ink)] mb-4 flex items-center gap-2">
                <Key className="w-6 h-6 text-[var(--pen-blue)]" />
                Issue New Virtual Key
              </h3>

              <form onSubmit={handleCreateSubmit} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Key Description / Name
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. Alice (Pi Coding Agent)"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
                  />
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Owner Name / Email
                    </label>
                    <input
                      type="text"
                      required
                      placeholder="e.g. Alice Vance"
                      value={owner}
                      onChange={(e) => setOwner(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                      style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
                    />
                  </div>

                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Tag (Attribution)
                    </label>
                    <input
                      type="text"
                      placeholder="e.g. dev-pi, ci, agent"
                      value={tag}
                      onChange={(e) => setTag(e.target.value)}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                      style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
                    />
                  </div>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Allowed Models / Routes
                  </label>
                  <input
                    type="text"
                    placeholder="* or coder, free, gemini-*"
                    value={allowedModels}
                    onChange={(e) => setAllowedModels(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: '255px 25px 225px 25px / 25px 225px 25px 255px' }}
                  />
                  <p className="text-xs text-[var(--ink)]/60 mt-1">Use * to allow all configured models and routes.</p>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Daily Budget (USD)
                    </label>
                    <input
                      type="number"
                      step="1"
                      min="1"
                      value={dailyBudget}
                      onChange={(e) => setDailyBudget(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                      style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
                    />
                  </div>

                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      Monthly Budget (USD)
                    </label>
                    <input
                      type="number"
                      step="5"
                      min="5"
                      value={monthlyBudget}
                      onChange={(e) => setMonthlyBudget(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                      style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
                    />
                  </div>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      RPM Limit (Req/min)
                    </label>
                    <input
                      type="number"
                      value={rpmLimit}
                      onChange={(e) => setRpmLimit(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                      style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
                    />
                  </div>

                  <div>
                    <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                      TPM Limit (Tok/min)
                    </label>
                    <input
                      type="number"
                      value={tpmLimit}
                      onChange={(e) => setTpmLimit(Number(e.target.value))}
                      className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                      style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
                    />
                  </div>
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Max Concurrent Requests
                  </label>
                  <input
                    type="number"
                    min="1"
                    step="1"
                    value={maxConcurrentRequests}
                    onChange={(e) => setMaxConcurrentRequests(e.target.value)}
                    placeholder="Unlimited"
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
                  />
                  <p className="text-xs text-[var(--ink)]/60 mt-1">Blank leaves this key without a concurrency cap.</p>
                </div>

                <div className="pt-2 flex justify-end gap-3">
                  {error && <span className="text-xs text-[var(--danger-text)] font-mono self-center">{error}</span>}
                  <SketchButton
                    type="button"
                    variant="ghost"
                    onClick={() => setShowCreateModal(false)}
                  >
                    Cancel
                  </SketchButton>
                  <SketchButton type="submit" variant="danger" className="font-bold" disabled={isSubmitting}>
                    {isSubmitting ? 'Generating…' : 'Generate Virtual Key'}
                  </SketchButton>
                </div>
              </form>
            </WobblyCard>
          </div>
        </div>
      )}
    </div>
  );
};
