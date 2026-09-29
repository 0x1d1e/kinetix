import React, { useState } from 'react';
import { Shuffle, Plus, ArrowDown, ArrowUp, Shield, Check, Layers, ArrowRight, Trash2, AlertTriangle, PlayCircle, Search, X } from 'lucide-react';
import { Route, Account, ModelConfig } from '../../types';
import { WobblyCard, SketchButton, SketchBadge } from '../HandDrawnElements';
import { DESIGN_TOKENS } from '../../lib/designSystem';
import { Kinetix } from '../../lib/resources';

const ROUTE_STRATEGIES: ReadonlyArray<readonly [Route['selectionStrategy'], string]> = [
  ['priority', 'Priority (Ordered fallback on failure)'],
  ['round-robin', 'Round-robin load balancing'],
  ['weighted', 'Weighted distribution'],
  ['least-used', 'Least-used (prefer idle accounts)'],
  ['adaptive', 'Adaptive (capacity + TTFT EWMA)'],
];


interface RoutesViewProps {
  routes: Route[];
  accounts: Account[];
  models: ModelConfig[];
  /** Providers the selected virtual key is restricted to (FR-12.19); empty =
   *  no restriction. Used by the Dry Run to reflect access restrictions. */
  allowedProviders?: string[];
  onAddRoute: (newRoute: Route) => void;
  onUpdateRoute: (updated: Route) => void;
  onDeleteRoute: (routeId: string) => void;
}

export const RoutesView: React.FC<RoutesViewProps> = ({
  routes,
  accounts,
  models,
  allowedProviders,
  onAddRoute,
  onUpdateRoute,
  onDeleteRoute,
}) => {
  const [showCreateModal, setShowCreateModal] = useState(false);
  const [selectedRouteId, setSelectedRouteId] = useState<string>(routes[0]?.id || '');
  const [confirmDeleteRouteId, setConfirmDeleteRouteId] = useState<string | null>(null);
  const [confirmRemoveTargetId, setConfirmRemoveTargetId] = useState<string | null>(null);
  const [routeSearch, setRouteSearch] = useState('');

  // New route form
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [strategy, setStrategy] = useState<Route['selectionStrategy']>('priority');
  const [maxConcurrentRequests, setMaxConcurrentRequests] = useState('');
  const [on429, setOn429] = useState(true);
  const [onQuota, setOnQuota] = useState(true);
  const [on5xx, setOn5xx] = useState(true);
  const [sticky, setSticky] = useState(true);
  const [dryRunResult, setDryRunResult] = useState<any | null>(null);
  const [dryRunning, setDryRunning] = useState(false);

  // Add-target form state (per selected route)
  const [newTargetModelId, setNewTargetModelId] = useState('');
  const [newTargetAccountId, setNewTargetAccountId] = useState('');

  const normalizedRouteSearch = routeSearch.trim().toLowerCase();
  const filteredRoutes = routes.filter((route) => {
    if (!normalizedRouteSearch) return true;
    const targetTerms = route.targets.flatMap((target) => {
      const model = models.find((m) => m.id === target.modelId);
      return [
        target.modelId,
        target.modelDisplayName,
        target.providerName,
        model?.upstreamModelId,
        model?.displayName,
      ];
    });
    return [
      route.id,
      route.name,
      route.description,
      route.selectionStrategy,
      ...targetTerms,
    ].some((value) => String(value ?? '').toLowerCase().includes(normalizedRouteSearch));
  });
  const activeRoute =
    filteredRoutes.find((route) => route.id === selectedRouteId) ||
    filteredRoutes[0];

  /** Renumber targets by their (already ordered) position. */
  const renumber = (targets: Route['targets']): Route['targets'] =>
    targets.map((t, i) => ({ ...t, priority: i + 1 }));

  /** Move a target up/down in the priority order and persist. */
  const moveTarget = (route: Route, index: number, dir: -1 | 1) => {
    const next = index + dir;
    if (next < 0 || next >= route.targets.length) return;
    const targets = [...route.targets];
    [targets[index], targets[next]] = [targets[next], targets[index]];
    const reordered = renumber(targets);
    onUpdateRoute({ ...route, targets: reordered, totalHops: Math.max(0, reordered.length - 1) });
  };

  /** Add a chosen model (and optional account) as a new fallback tier. */
  const handleAddTarget = (route: Route) => {
    const model = models.find((m) => m.id === newTargetModelId);
    if (!model) return;
    const account = accounts.find((a) => a.id === newTargetAccountId);
    const target = {
      id: `tgt-${Date.now()}`,
      accountId: account?.id ?? '',
      accountLabel: account?.label ?? '(auto)',
      providerName: model.providerName,
      modelId: model.id,
      modelDisplayName: model.displayName,
      priority: route.targets.length + 1,
      weight: 1,
    };
    const targets = renumber([...route.targets, target]);
    onUpdateRoute({ ...route, targets, totalHops: Math.max(0, targets.length - 1) });
    setNewTargetModelId('');
    setNewTargetAccountId('');
  };

  const handleDryRun = async () => {
    if (!activeRoute) return;
    setDryRunning(true);
    try {
      const r = await Kinetix.dryRunRoute(activeRoute.name, {
        frontend: 'openai',
        has_tools: false,
        has_images: false,
        has_reasoning: false,
        input_tokens: 1000,
        allowed_providers: allowedProviders ?? [],
      });
      setDryRunResult(r);
    } catch (e) {
      setDryRunResult({ error: (e as Error).message });
    } finally {
      setDryRunning(false);
    }
  };

  const handleCreateSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;

    // Routes start empty: the user adds their own fallback targets.
    const newRoute: Route = {
      id: `route-${Date.now()}`,
      name: name.trim().toLowerCase().replace(/\s+/g, '-'),
      description: description.trim() || 'Custom fallback route',
      selectionStrategy: strategy,
      fallbackTriggers: {
        on429,
        onQuota,
        on5xx,
        onTimeout: true,
      },
      targets: [],
      portabilityPolicy: 'strip_with_warning',
      cacheAffinity: true,
      stickyRouting: sticky,
      maxConcurrentRequests: maxConcurrentRequests === '' ? null : Number(maxConcurrentRequests),
      totalHops: 0,
      status: 'active',
    };

    onAddRoute(newRoute);
    setSelectedRouteId(newRoute.id);
    setShowCreateModal(false);
    setName('');
    setDescription('');
    setMaxConcurrentRequests('');
  };

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Routes & Automatic Fallback</span>
            <SketchBadge variant="yellow" rotation="1deg">
              FR-12 Architecture
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Group accounts across providers under one model name. If an account is rate-limited or exhausted, Kinetix falls back automatically before the first byte!
          </p>
        </div>

        <SketchButton
          variant="primary"
          size="md"
          onClick={() => setShowCreateModal(true)}
          className="gap-2 font-heading font-bold"
        >
          <Plus className="w-5 h-5" />
          Create New Route
        </SketchButton>
      </div>

      {/* Main Grid: Routes selector on left, Deep Inspector on right */}
      {routes.length === 0 ? (
        <WobblyCard decoration="tack" className="p-10 text-center bg-[var(--surface)]">
          <Shuffle className="w-12 h-12 text-[var(--pen-blue)] mx-auto mb-3 opacity-60" />
          <h3 className="text-2xl font-heading font-bold text-[var(--ink)]">No Routes Configured</h3>
          <p className="text-base font-body text-[var(--ink)]/80 max-w-lg mx-auto mt-2 mb-6">
            Routes allow you to group multiple upstream provider accounts and models under one seamless model alias. If one account exhausts its quota or hits rate limits, Kinetix instantly retries on the next healthy tier.
          </p>
          <SketchButton
            variant="primary"
            size="md"
            onClick={() => setShowCreateModal(true)}
            className="gap-2 font-heading font-bold"
          >
            <Plus className="w-5 h-5" />
            Create First Fallback Route
          </SketchButton>
        </WobblyCard>
      ) : (
        <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
          {/* Left Column: List of Routes */}
          <div className="space-y-4">
            <h3 className="text-xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
              <Shuffle className="w-5 h-5 text-[var(--pen-blue)]" />
              Configured Routes ({filteredRoutes.length}/{routes.length})
            </h3>

            <div className="relative">
              <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-[var(--ink)]/50" />
              <input
                type="search"
                value={routeSearch}
                onChange={(e) => setRouteSearch(e.target.value)}
                placeholder="Search routes…"
                className="w-full pl-9 pr-9 py-2 bg-[var(--surface)] border-2 border-[var(--ink)] font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
                style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
              />
              {routeSearch && (
                <button type="button" onClick={() => setRouteSearch('')} className="absolute right-2 top-1/2 -translate-y-1/2 p-1 text-[var(--ink)]/60 hover:text-[var(--marker-red)] cursor-pointer" title="Clear search">
                  <X className="w-4 h-4" />
                </button>
              )}
            </div>

            {filteredRoutes.length === 0 && (
              <div className="p-5 text-center bg-[var(--surface)] border-2 border-dashed border-[var(--ink)]/30 rounded">
                <p className="text-sm font-mono text-[var(--ink)]/70">No routes match “{routeSearch}”.</p>
                <button onClick={() => setRouteSearch('')} className="mt-2 text-xs font-heading font-bold text-[var(--pen-blue)] hover:underline cursor-pointer">
                  Clear search
                </button>
              </div>
            )}

            {filteredRoutes.map((route, idx) => {
              const isSelected = route.id === activeRoute?.id;
              const tilt = idx % 2 === 0 ? '-rotate-0.5' : 'rotate-0.5';

              return (
                <div
                  key={route.id}
                  onClick={() => setSelectedRouteId(route.id)}
                  className={`p-4 border-2 border-[var(--ink)] cursor-pointer transition-all ${tilt} ${
                    isSelected
                      ? 'bg-[var(--postit)] sketch-shadow -translate-y-1 font-bold'
                      : 'bg-[var(--surface)] hover:bg-[var(--erased-soft)] sketch-shadow-sm'
                  }`}
                  style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                >
                  <div className="flex items-start justify-between gap-2">
                    <div>
                      <span className="font-mono text-sm px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded">
                        {route.name}
                      </span>
                      <h4 className="font-heading text-lg mt-1 text-[var(--ink)]">{route.description}</h4>
                    </div>
                    <SketchBadge variant={route.status === 'active' ? 'green' : 'red'}>
                      {route.status}
                    </SketchBadge>
                  </div>

                  <div className="mt-3 pt-2 border-t border-[var(--ink)]/20 flex flex-wrap items-center justify-between gap-x-3 gap-y-1 text-xs font-mono text-[var(--ink)]/70">
                    <span>Strategy: <strong>{route.selectionStrategy}</strong></span>
                    <span>{route.targets.length} targets • {route.totalHops} hops</span>
                    <span>Concurrency: <strong>{route.maxConcurrentRequests ?? 'unlimited'}</strong></span>
                  </div>
                </div>
              );
            })}
          </div>

          {/* Right Column: Selected Route Detail & Target Fallback Chain */}
          {activeRoute && (
            <div className="lg:col-span-2 min-w-0 space-y-5">
              <WobblyCard decoration="tape" className="p-6">
                <div className="flex flex-wrap items-start justify-between gap-4 mb-4 pb-3 border-b-2 border-dashed border-[var(--ink)]/30">
                  <div>
                    <div className="flex items-center gap-2 flex-wrap">
                      <h3 className="text-3xl font-heading font-bold text-[var(--ink)]">
                        Route: <span className="underline decoration-wavy decoration-[var(--marker-red)]">{activeRoute.name}</span>
                      </h3>
                      <SketchBadge variant="blue" rotation="-1deg">
                        {activeRoute.selectionStrategy} strategy
                      </SketchBadge>
                    </div>
                    <p className="text-base font-body text-[var(--ink)]/80 mt-1">
                      {activeRoute.description}
                    </p>
                  </div>

                  <div className="flex flex-wrap items-center gap-2">
                    <span className="text-xs font-mono bg-[var(--erased)] px-2 py-1 border border-[var(--ink)] rounded">
                      Total Fallback Hops: <strong>{activeRoute.totalHops}</strong>
                    </span>

                    <button
                      onClick={handleDryRun}
                      disabled={dryRunning}
                      className="px-2.5 py-1 text-xs font-heading font-bold text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-[var(--pen-blue)]/50 hover:border-[var(--pen-blue)] rounded flex items-center gap-1 cursor-pointer transition-colors disabled:opacity-50"
                      title="Compute candidate ordering and would-be selection without calling upstream (FR-8.7)"
                    >
                      <PlayCircle className="w-3.5 h-3.5" />
                      <span>{dryRunning ? 'Dry Running…' : 'Dry Run'}</span>
                    </button>

                    {confirmDeleteRouteId === activeRoute.id ? (
                      <div className="flex items-center gap-1.5 bg-[var(--tint-red)] px-2.5 py-1 border border-[var(--marker-red)] rounded text-xs font-heading">
                        <span className="text-[var(--danger-text)] font-bold">Delete {activeRoute.name}?</span>
                        <button
                          onClick={() => {
                            onDeleteRoute(activeRoute.id);
                            setConfirmDeleteRouteId(null);
                          }}
                          className="px-2 py-0.5 bg-[var(--marker-red)] text-[var(--surface)] rounded font-bold hover:brightness-90 cursor-pointer"
                        >
                          Confirm
                        </button>
                        <button
                          onClick={() => setConfirmDeleteRouteId(null)}
                          className="px-2 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded hover:bg-[var(--erased)] cursor-pointer"
                        >
                          Cancel
                        </button>
                      </div>
                    ) : (
                      <button
                        onClick={() => setConfirmDeleteRouteId(activeRoute.id)}
                        className="px-2.5 py-1 text-xs font-heading font-bold text-[var(--marker-red)] hover:bg-[var(--tint-red)] border border-[var(--marker-red)]/50 hover:border-[var(--marker-red)] rounded flex items-center gap-1 cursor-pointer transition-colors"
                        title="Delete this route configuration"
                      >
                        <Trash2 className="w-3.5 h-3.5" />
                        <span>Delete Route</span>
                      </button>
                    )}
                  </div>
                </div>

                {/* Targets Fallback Sequence */}
                <div className="space-y-3 mb-6">
                  <h4 className="text-lg font-heading font-bold text-[var(--ink)] flex items-center gap-2">
                    <Layers className="w-5 h-5 text-[var(--marker-red)]" />
                    Fallback Target Hierarchy (Priority Ordered)
                  </h4>

                  <div className="space-y-3">
                    {activeRoute.targets.length === 0 && (
                      <div className="p-6 text-center bg-[var(--erased-soft)] border-2 border-dashed border-[var(--ink)]/40 rounded">
                        <Layers className="w-8 h-8 text-[var(--pen-blue)] mx-auto mb-2 opacity-60" />
                        <p className="text-sm font-body text-[var(--ink)]/80">
                          No targets yet. Add a model below to define this route's fallback order.
                        </p>
                      </div>
                    )}

                    {activeRoute.targets.map((tgt, idx) => (
                      <React.Fragment key={tgt.id}>
                        <div
                          className="p-4 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm flex flex-col md:flex-row items-start md:items-center justify-between gap-3"
                          style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
                        >
                          <div className="flex items-center gap-3">
                            <div className="w-8 h-8 rounded-full border-2 border-[var(--ink)] bg-[var(--pen-blue)] text-[var(--surface)] flex items-center justify-center font-heading font-bold text-base">
                              #{tgt.priority}
                            </div>
                            <div>
                              <div className="flex items-center gap-2 flex-wrap">
                                <span className="font-heading font-bold text-lg text-[var(--ink)]">
                                  {tgt.modelDisplayName}
                                </span>
                                <span className="text-xs font-mono bg-[var(--erased)] px-1.5 py-0.5 rounded border border-[var(--ink)]/30">
                                  {tgt.providerName}
                                </span>
                              </div>
                              <p className="text-sm font-body text-[var(--ink)]/70">
                                Serving Account: <strong>{tgt.accountLabel}</strong>
                              </p>
                            </div>
                          </div>

                          <div className="flex items-center gap-2 text-xs font-mono">
                            <span className="px-2 py-1 bg-[var(--tint-green)] text-[var(--success-text)] border border-[var(--pen-green)] rounded">
                              {idx === 0 ? 'Primary Default' : `Fallback Tier ${idx}`}
                            </span>

                            <div className="flex items-center gap-1">
                              <button
                                onClick={() => moveTarget(activeRoute, idx, -1)}
                                disabled={idx === 0}
                                className="p-1 text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-transparent hover:border-[var(--pen-blue)]/40 rounded cursor-pointer transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
                                title="Move up in fallback order"
                              >
                                <ArrowUp className="w-3.5 h-3.5" />
                              </button>
                              <button
                                onClick={() => moveTarget(activeRoute, idx, 1)}
                                disabled={idx === activeRoute.targets.length - 1}
                                className="p-1 text-[var(--pen-blue)] hover:bg-[var(--tint-blue)] border border-transparent hover:border-[var(--pen-blue)]/40 rounded cursor-pointer transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
                                title="Move down in fallback order"
                              >
                                <ArrowDown className="w-3.5 h-3.5" />
                              </button>
                            </div>

                            {confirmRemoveTargetId === tgt.id ? (
                              <div className="flex items-center gap-1 bg-[var(--tint-red)] px-2 py-0.5 border border-[var(--marker-red)] rounded">
                                <span className="text-[var(--danger-text)] font-bold">Remove tier?</span>
                                <button
                                  onClick={() => {
                                    const updatedTargets = renumber(
                                      activeRoute.targets.filter((t) => t.id !== tgt.id),
                                    );
                                    onUpdateRoute({
                                      ...activeRoute,
                                      targets: updatedTargets,
                                      totalHops: Math.max(0, updatedTargets.length - 1),
                                    });
                                    setConfirmRemoveTargetId(null);
                                  }}
                                  className="px-1.5 py-0.5 bg-[var(--marker-red)] text-[var(--surface)] rounded font-bold hover:brightness-90 cursor-pointer"
                                >
                                  Yes
                                </button>
                                <button
                                  onClick={() => setConfirmRemoveTargetId(null)}
                                  className="px-1.5 py-0.5 bg-[var(--surface)] border border-[var(--ink)] rounded cursor-pointer"
                                >
                                  No
                                </button>
                              </div>
                            ) : (
                              <button
                                onClick={() => setConfirmRemoveTargetId(tgt.id)}
                                className="p-1 text-[var(--marker-red)] hover:bg-[var(--tint-red)] border border-transparent hover:border-[var(--marker-red)]/40 rounded cursor-pointer transition-colors"
                                title="Remove this target tier from route pool"
                              >
                                <Trash2 className="w-3.5 h-3.5" />
                              </button>
                            )}
                          </div>
                        </div>

                        {idx < activeRoute.targets.length - 1 && (
                          <div className="flex justify-center -my-1">
                            <div className="flex items-center gap-1 bg-[var(--postit)] px-3 py-1 border border-[var(--ink)] rounded-full text-xs font-mono sketch-shadow-sm z-10">
                              <ArrowDown className="w-3.5 h-3.5 text-[var(--marker-red)]" />
                              <span>Falls back on 429 / Quota / 5xx error</span>
                            </div>
                          </div>
                        )}
                      </React.Fragment>
                    ))}
                  </div>

                  {/* Add a fallback target */}
                  <div className="p-3 bg-[var(--postit)] border border-[var(--ink)] rounded">
                    <h5 className="text-sm font-heading font-bold text-[var(--ink)] mb-2 flex items-center gap-1">
                      <Plus className="w-4 h-4" />
                      Add Fallback Target
                    </h5>
                    <div className="flex flex-col md:flex-row gap-2">
                      <select
                        value={newTargetModelId}
                        onChange={(e) => setNewTargetModelId(e.target.value)}
                        className="flex-1 min-w-0 bg-[var(--surface)] border-2 border-[var(--ink)] px-2 py-1.5 text-sm font-mono rounded focus:outline-none"
                      >
                        <option value="">Select a model…</option>
                        {models.map((m) => (
                          <option key={m.id} value={m.id}>
                            {m.displayName} — {m.providerName}
                          </option>
                        ))}
                      </select>
                      <select
                        value={newTargetAccountId}
                        onChange={(e) => setNewTargetAccountId(e.target.value)}
                        className="flex-1 min-w-0 bg-[var(--surface)] border-2 border-[var(--ink)] px-2 py-1.5 text-sm font-mono rounded focus:outline-none"
                      >
                        <option value="">Account: auto (lowest priority)</option>
                        {accounts.map((a) => (
                          <option key={a.id} value={a.id}>
                            {a.label} — {a.providerName}
                          </option>
                        ))}
                      </select>
                      <SketchButton
                        type="button"
                        variant="primary"
                        onClick={() => handleAddTarget(activeRoute)}
                        disabled={!newTargetModelId}
                        className="gap-1 font-heading font-bold whitespace-nowrap"
                      >
                        <Plus className="w-4 h-4" />
                        Add
                      </SketchButton>
                    </div>
                    <p className="text-xs font-body text-[var(--ink)]/60 mt-2">
                      Targets are tried in order; the first is primary, the rest are fallbacks.
                      Choose a specific account to pin the tier, or leave it on auto.
                    </p>
                  </div>
                </div>

              {/* Route Policies & Triggers Settings */}
              <div className="grid grid-cols-1 md:grid-cols-2 gap-4 bg-[var(--paper)] p-4 border-2 border-[var(--ink)] rounded-lg">
                <div className="md:col-span-2">
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Selection Strategy
                  </label>
                  <select
                    value={activeRoute.selectionStrategy}
                    onChange={(e) =>
                      onUpdateRoute({
                        ...activeRoute,
                        selectionStrategy: e.target.value as Route['selectionStrategy'],
                      })
                    }
                    className="w-full bg-[var(--surface)] border border-[var(--ink)] px-2 py-1.5 text-sm font-mono rounded"
                  >
                    {ROUTE_STRATEGIES.map(([value, label]) => (
                      <option key={value} value={value}>
                        {label}
                      </option>
                    ))}
                  </select>
                </div>

                <div className="md:col-span-2">
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Max Concurrent Requests
                  </label>
                  <input
                    type="number"
                    min="1"
                    step="1"
                    value={activeRoute.maxConcurrentRequests ?? ''}
                    onChange={(e) => onUpdateRoute({
                      ...activeRoute,
                      maxConcurrentRequests: e.target.value === '' ? null : Number(e.target.value),
                    })}
                    placeholder="Unlimited"
                    className="w-full md:w-64 bg-[var(--surface)] border border-[var(--ink)] px-2 py-1.5 text-sm font-mono rounded"
                  />
                  <p className="text-xs text-[var(--ink)]/60 mt-1">Blank leaves the Route without a concurrency cap.</p>
                </div>

                <div>
                  <h5 className="font-heading font-bold text-base text-[var(--ink)] mb-2 flex items-center gap-1">
                    <Shield className="w-4 h-4 text-[var(--pen-blue)]" />
                    Configured Fallback Triggers
                  </h5>
                  <div className="space-y-1.5 text-sm font-body">
                    <label className="flex items-center gap-2">
                      <input
                        type="checkbox"
                        checked={activeRoute.fallbackTriggers.on429}
                        onChange={(e) =>
                          onUpdateRoute({
                            ...activeRoute,
                            fallbackTriggers: { ...activeRoute.fallbackTriggers, on429: e.target.checked },
                          })
                        }
                        className="accent-[var(--marker-red)]"
                      />
                      <span>Rate limit (HTTP 429) & Cooldown honoring Retry-After</span>
                    </label>

                    <label className="flex items-center gap-2">
                      <input
                        type="checkbox"
                        checked={activeRoute.fallbackTriggers.onQuota}
                        onChange={(e) =>
                          onUpdateRoute({
                            ...activeRoute,
                            fallbackTriggers: { ...activeRoute.fallbackTriggers, onQuota: e.target.checked },
                          })
                        }
                        className="accent-[var(--marker-red)]"
                      />
                      <span>Quota exhaustion (Daily or Monthly provider caps)</span>
                    </label>

                    <label className="flex items-center gap-2">
                      <input
                        type="checkbox"
                        checked={activeRoute.fallbackTriggers.on5xx}
                        onChange={(e) =>
                          onUpdateRoute({
                            ...activeRoute,
                            fallbackTriggers: { ...activeRoute.fallbackTriggers, on5xx: e.target.checked },
                          })
                        }
                        className="accent-[var(--marker-red)]"
                      />
                      <span>Upstream 5xx / connection timeout</span>
                    </label>
                  </div>
                </div>

                <div>
                  <h5 className="font-heading font-bold text-base text-[var(--ink)] mb-2 flex items-center gap-1">
                    <Check className="w-4 h-4 text-[var(--pen-green)]" />
                    Session & Conversation Continuity
                  </h5>
                  <div className="space-y-2 text-sm font-body">
                    <label className="flex items-center gap-2">
                      <input
                        type="checkbox"
                        checked={activeRoute.stickyRouting}
                        onChange={(e) =>
                          onUpdateRoute({
                            ...activeRoute,
                            stickyRouting: e.target.checked,
                          })
                        }
                        className="accent-[var(--pen-blue)]"
                      />
                      <span>Sticky routing (Preserves prompt cache while target healthy)</span>
                    </label>

                    <div className="pt-1">
                      <span className="text-xs font-mono text-[var(--ink)]/70 block mb-1">
                        Opaque-State Portability Policy (FR-2.11):
                      </span>
                      <select
                        value={activeRoute.portabilityPolicy}
                        onChange={(e) =>
                          onUpdateRoute({
                            ...activeRoute,
                            portabilityPolicy: e.target.value as any,
                          })
                        }
                        className="bg-[var(--surface)] border border-[var(--ink)] px-2 py-1 text-xs font-mono rounded w-full"
                      >
                        <option value="strip_with_warning">Strip + warn client (recommended)</option>
                        <option value="reject">Reject the fallback attempt</option>
                      </select>
                    </div>

                    <label className="flex items-center gap-2">
                      <input
                        type="checkbox"
                        checked={activeRoute.cacheAffinity}
                        onChange={(e) =>
                          onUpdateRoute({
                            ...activeRoute,
                            cacheAffinity: e.target.checked,
                          })
                        }
                        className="accent-[var(--pen-blue)]"
                      />
                      <span>Cache affinity (keep a session on the same target, FR-7.3)</span>
                    </label>
                  </div>
                </div>
              </div>

              {dryRunResult && (
                <div
                  className="mt-4 p-4 text-sm font-mono bg-[var(--surface)] border-2 border-[var(--pen-blue)]"
                  style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                >
                  <div className="flex items-center justify-between mb-2">
                    <span className="font-heading font-bold text-[var(--pen-blue)]">
                      Route Dry Run (FR-8.7)
                    </span>
                    <button
                      onClick={() => setDryRunResult(null)}
                      className="text-[var(--ink)] hover:text-[var(--marker-red)] cursor-pointer"
                    >
                      ✕
                    </button>
                  </div>
                  {dryRunResult.error ? (
                    <div style={{ color: 'var(--danger-text)' }}>{dryRunResult.error}</div>
                  ) : (
                    <>
                      <div className="mb-2">
                        Would select:{' '}
                        <strong>
                          {dryRunResult.would_select
                            ? typeof dryRunResult.would_select === 'string'
                              ? dryRunResult.would_select
                              : `${dryRunResult.would_select.model} @ ${dryRunResult.would_select.account}`
                            : '(no eligible target)'}
                        </strong>
                      </div>
                      <table className="w-full text-xs">
                        <thead>
                          <tr className="text-left border-b border-[var(--ink)]/30">
                            <th className="py-1">Target</th>
                            <th>Predicate</th>
                            <th>Caps</th>
                            <th>Account</th>
                            <th>Eligible</th>
                          </tr>
                        </thead>
                        <tbody>
                          {(dryRunResult.candidates || []).map((c: any, i: number) => (
                            <tr key={i} className="border-b border-[var(--ink)]/10">
                              <td className="py-1">
                                {c.model} @ {c.account || '—'}
                              </td>
                              <td>{c.predicate_result ?? '—'}</td>
                              <td>{c.capability_eligible ? 'ok' : 'no'}</td>
                              <td>{c.account_status ?? '—'}</td>
                              <td style={{ color: c.eligible ? 'var(--pen-green)' : 'var(--danger-text)' }}>
                                {c.eligible ? 'yes' : 'no'}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                      <div className="mt-2 text-[var(--ink)]/60">{dryRunResult.note}</div>
                    </>
                  )}
                </div>
              )}
            </WobblyCard>
          </div>
        )}
      </div>
    )}

      {/* Create Route Modal */}
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
                <Shuffle className="w-6 h-6 text-[var(--pen-blue)]" />
                Create New Fallback Route
              </h3>

              <form onSubmit={handleCreateSubmit} className="space-y-4 font-body">
                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Route Slug Name (Clients request this model)
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. coder, fast-chat, vision-route"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none font-mono"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Description
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="e.g. Free Gemini tier falling back to paid Gemini and Groq"
                    value={description}
                    onChange={(e) => setDescription(e.target.value)}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                  />
                </div>

                <div>
                  <label className="block text-sm font-heading font-bold text-[var(--ink)] mb-1">
                    Selection Strategy
                  </label>
                  <select
                    value={strategy}
                    onChange={(e) => setStrategy(e.target.value as Route['selectionStrategy'])}
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none font-mono"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  >
                    {ROUTE_STRATEGIES.map(([value, label]) => (
                      <option key={value} value={value}>
                        {label}
                      </option>
                    ))}
                  </select>
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
                    className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base sketch-shadow-sm focus:outline-none font-mono"
                    style={{ borderRadius: DESIGN_TOKENS.radii.wobblyMd }}
                  />
                  <p className="text-xs text-[var(--ink)]/60 mt-1">Blank leaves the Route without a concurrency cap.</p>
                </div>

                <div className="p-3 bg-[var(--postit)] border border-[var(--ink)] rounded space-y-2 text-sm">
                  <label className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      checked={on429}
                      onChange={(e) => setOn429(e.target.checked)}
                      className="accent-[var(--marker-red)]"
                    />
                    <span>Fallback automatically on 429 Rate Limit</span>
                  </label>

                  <label className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      checked={onQuota}
                      onChange={(e) => setOnQuota(e.target.checked)}
                      className="accent-[var(--marker-red)]"
                    />
                    <span>Fallback on Quota Exhaustion</span>
                  </label>

                  <label className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      checked={sticky}
                      onChange={(e) => setSticky(e.target.checked)}
                      className="accent-[var(--pen-blue)]"
                    />
                    <span>Enable sticky session routing (Prompt cache preservation)</span>
                  </label>
                  <p className="text-xs font-body text-[var(--ink)]/60 pl-6">
                    Keeps a conversation on the same account/model across turns so the
                    upstream provider's prompt cache stays warm. A client must send a
                    session header (X-Kinetix-Session / X-Session-Id) for this to apply;
                    if the sticky target becomes unhealthy, Kinetix falls back as usual.
                  </p>
                </div>

                <div className="pt-2 flex justify-end gap-3">
                  <SketchButton
                    type="button"
                    variant="ghost"
                    onClick={() => setShowCreateModal(false)}
                  >
                    Cancel
                  </SketchButton>
                  <SketchButton type="submit" variant="danger" className="font-bold">
                    Create Route
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
