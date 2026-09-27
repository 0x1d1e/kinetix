import React, { useCallback, useEffect, useState } from 'react';
import { Activity, RefreshCw, ShieldAlert, TimerReset } from 'lucide-react';
import { api } from '../../lib/api';
import { SketchBadge, SketchButton } from '../HandDrawnElements';

type WindowKey = '5m' | '1h' | '24h';
type ScopeKey = 'all' | 'provider' | 'account' | 'model';

interface Circuit {
  provider_id: string;
  state: 'closed' | 'open' | 'half_open';
  recent_qualifying_failures: number;
  distinct_failing_accounts: number;
  distinct_failing_targets: number;
  recent_failures: Array<{ at: string; account_id: string; target_id: string }>;
  retry_at?: string | null;
  last_successful_probe?: string | null;
}

interface Quota {
  provider_id: string;
  account_id: string;
  remaining_fraction?: number | null;
  reset_at?: string | null;
  observed_at: string;
  source: string;
  freshness: 'fresh' | 'stale';
}

interface Telemetry {
  scope: 'provider' | 'account' | 'model';
  provider_id: string;
  account_id?: string | null;
  model_id?: string | null;
  attempts: number;
  success_rate?: number | null;
  fallback_failures: number;
  fallbacks: number;
  cancellations: number;
  rate_limits: number;
  quota_exhausted: number;
  auth_errors: number;
  target_errors: number;
  bad_requests: number;
  server_5xx: number;
  connection_errors: number;
  timeouts: number;
  adaptive_saturation: number;
  provider_circuit_rejects: number;
  ttft_p50_ms?: number | null;
  ttft_p95_ms?: number | null;
  ttft_p99_ms?: number | null;
  duration_p50_ms?: number | null;
  duration_p95_ms?: number | null;
  duration_p99_ms?: number | null;
}

interface RuntimeHealth {
  window: WindowKey;
  telemetry: Telemetry[];
  provider_circuits: Circuit[];
  quota: Quota[];
  dropped: { queue: number; persistence: number };
}

function pct(value?: number | null): string {
  return value == null ? 'unknown' : `${(value * 100).toFixed(1)}%`;
}

function ms(value?: number | null): string {
  return value == null ? '—' : `${value} ms`;
}

function when(value?: string | null): string {
  if (!value) return '—';
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? value : parsed.toLocaleString();
}

export function HealthView() {
  const [windowKey, setWindowKey] = useState<WindowKey>('1h');
  const [scopeKey, setScopeKey] = useState<ScopeKey>('model');
  const [data, setData] = useState<RuntimeHealth | null>(null);
  const [error, setError] = useState<string>('');
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      setData(await api.get<RuntimeHealth>(`/admin/api/health/runtime?window=${windowKey}`));
    } catch (err) {
      setError(err instanceof Error ? err.message : 'failed to load runtime health');
    } finally {
      setLoading(false);
    }
  }, [windowKey]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const telemetryRows =
    data?.telemetry.filter((row) => scopeKey === 'all' || row.scope === scopeKey) ?? [];
  const openCircuits = data?.provider_circuits.filter((circuit) => circuit.state !== 'closed') ?? [];

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="font-heading text-3xl font-bold text-[var(--ink)]">Runtime Health</h2>
          <p className="font-body text-sm text-[var(--ink)]/65 mt-1">
            Live provider circuits and quota evidence plus persisted target telemetry.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {(['5m', '1h', '24h'] as WindowKey[]).map((value) => (
            <SketchButton
              key={value}
              onClick={() => setWindowKey(value)}
              variant={windowKey === value ? 'primary' : 'secondary'}
              size="sm"
            >
              {value}
            </SketchButton>
          ))}
          <SketchButton onClick={() => void refresh()} variant="secondary" size="sm">
            <RefreshCw className={`w-4 h-4 ${loading ? 'animate-spin' : ''}`} />
            Refresh
          </SketchButton>
        </div>
      </div>

      {error && (
        <div className="border-2 border-[var(--marker-red)] p-3 font-body text-sm text-[var(--marker-red)]">
          {error}
        </div>
      )}

      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        <div className="border-2 border-[var(--ink)] p-4 bg-[var(--surface)] sketch-shadow-sm">
          <div className="flex items-center gap-2 text-sm font-heading font-bold">
            <ShieldAlert className="w-4 h-4" /> Provider circuits
          </div>
          <div className="text-3xl font-heading font-bold mt-2">{openCircuits.length}</div>
          <div className="font-body text-xs text-[var(--ink)]/60">open or half-open</div>
        </div>
        <div className="border-2 border-[var(--ink)] p-4 bg-[var(--surface)] sketch-shadow-sm">
          <div className="flex items-center gap-2 text-sm font-heading font-bold">
            <TimerReset className="w-4 h-4" /> Fresh quota observations
          </div>
          <div className="text-3xl font-heading font-bold mt-2">
            {data?.quota.filter((quota) => quota.freshness === 'fresh').length ?? 0}
          </div>
          <div className="font-body text-xs text-[var(--ink)]/60">stale/unknown stay neutral</div>
        </div>
        <div className="border-2 border-[var(--ink)] p-4 bg-[var(--surface)] sketch-shadow-sm">
          <div className="flex items-center gap-2 text-sm font-heading font-bold">
            <Activity className="w-4 h-4" /> Telemetry drops
          </div>
          <div className="text-3xl font-heading font-bold mt-2">
            {(data?.dropped.queue ?? 0) + (data?.dropped.persistence ?? 0)}
          </div>
          <div className="font-body text-xs text-[var(--ink)]/60">queue + persistence</div>
        </div>
      </div>

      <section className="border-2 border-[var(--ink)] bg-[var(--surface)] overflow-x-auto">
        <div className="p-4 border-b-2 border-[var(--ink)]">
          <h3 className="font-heading text-xl font-bold">Provider circuits</h3>
        </div>
        <table className="w-full min-w-[760px] text-sm">
          <thead>
            <tr className="text-left border-b border-[var(--ink)]/30">
              <th className="p-3">Provider</th>
              <th className="p-3">State</th>
              <th className="p-3">Failures</th>
              <th className="p-3">Accounts / targets</th>
              <th className="p-3">Latest failure</th>
              <th className="p-3">Retry</th>
              <th className="p-3">Last recovery</th>
            </tr>
          </thead>
          <tbody>
            {(data?.provider_circuits ?? []).map((circuit) => (
              <tr key={circuit.provider_id} className="border-b border-[var(--ink)]/10">
                <td className="p-3 font-mono">{circuit.provider_id}</td>
                <td className="p-3">
                  <SketchBadge variant={circuit.state === 'closed' ? 'green' : 'red'}>
                    {circuit.state}
                  </SketchBadge>
                </td>
                <td className="p-3">{circuit.recent_qualifying_failures}</td>
                <td className="p-3">
                  {circuit.distinct_failing_accounts} / {circuit.distinct_failing_targets}
                </td>
                <td className="p-3 font-mono text-xs">
                  {circuit.recent_failures.at(-1)
                    ? `${circuit.recent_failures.at(-1)?.account_id} · ${circuit.recent_failures.at(-1)?.target_id}`
                    : '—'}
                </td>
                <td className="p-3">{when(circuit.retry_at)}</td>
                <td className="p-3">{when(circuit.last_successful_probe)}</td>
              </tr>
            ))}
            {data && data.provider_circuits.length === 0 && (
              <tr><td className="p-4 text-[var(--ink)]/60" colSpan={7}>No provider circuit observations yet.</td></tr>
            )}
          </tbody>
        </table>
      </section>

      <section className="border-2 border-[var(--ink)] bg-[var(--surface)] overflow-x-auto">
        <div className="p-4 border-b-2 border-[var(--ink)]">
          <h3 className="font-heading text-xl font-bold">Quota evidence</h3>
        </div>
        <table className="w-full min-w-[760px] text-sm">
          <thead>
            <tr className="text-left border-b border-[var(--ink)]/30">
              <th className="p-3">Provider</th>
              <th className="p-3">Account</th>
              <th className="p-3">Remaining</th>
              <th className="p-3">Reset</th>
              <th className="p-3">Source</th>
              <th className="p-3">Freshness</th>
              <th className="p-3">Observed</th>
            </tr>
          </thead>
          <tbody>
            {(data?.quota ?? []).map((quota) => (
              <tr key={`${quota.provider_id}:${quota.account_id}`} className="border-b border-[var(--ink)]/10">
                <td className="p-3 font-mono">{quota.provider_id}</td>
                <td className="p-3 font-mono">{quota.account_id}</td>
                <td className="p-3">{pct(quota.remaining_fraction)}</td>
                <td className="p-3">{when(quota.reset_at)}</td>
                <td className="p-3 font-mono text-xs">{quota.source}</td>
                <td className="p-3">
                  <SketchBadge variant={quota.freshness === 'fresh' ? 'green' : 'yellow'}>
                    {quota.freshness}
                  </SketchBadge>
                </td>
                <td className="p-3">{when(quota.observed_at)}</td>
              </tr>
            ))}
            {data && data.quota.length === 0 && (
              <tr><td className="p-4 text-[var(--ink)]/60" colSpan={7}>No quota observations yet.</td></tr>
            )}
          </tbody>
        </table>
      </section>

      <section className="border-2 border-[var(--ink)] bg-[var(--surface)] overflow-x-auto">
        <div className="p-4 border-b-2 border-[var(--ink)] flex flex-wrap items-center justify-between gap-3">
          <h3 className="font-heading text-xl font-bold">Target telemetry · {windowKey}</h3>
          <div className="flex flex-wrap gap-2">
            {(['all', 'provider', 'account', 'model'] as ScopeKey[]).map((value) => (
              <SketchButton
                key={value}
                onClick={() => setScopeKey(value)}
                variant={scopeKey === value ? 'primary' : 'secondary'}
                size="sm"
              >
                {value}
              </SketchButton>
            ))}
          </div>
        </div>
        <table className="w-full min-w-[1320px] text-sm">
          <thead>
            <tr className="text-left border-b border-[var(--ink)]/30">
              <th className="p-3">Scope</th>
              <th className="p-3">Provider / account / model</th>
              <th className="p-3">Attempts</th>
              <th className="p-3">Success</th>
              <th className="p-3">TTFT p50 / p95 / p99</th>
              <th className="p-3">Duration p50 / p95 / p99</th>
              <th className="p-3">5xx / conn / timeout</th>
              <th className="p-3">Rate / quota</th>
              <th className="p-3">Auth / target / bad request</th>
              <th className="p-3">Fallback use / failures</th>
              <th className="p-3">Cancelled</th>
              <th className="p-3">Saturation / circuit rejects (candidates)</th>
            </tr>
          </thead>
          <tbody>
            {telemetryRows.map((row) => (
              <tr key={`${row.scope}:${row.provider_id}:${row.account_id ?? ''}:${row.model_id ?? ''}`} className="border-b border-[var(--ink)]/10">
                <td className="p-3">{row.scope}</td>
                <td className="p-3 font-mono text-xs">
                  {row.provider_id}
                  {row.account_id ? <><br />{row.account_id}</> : null}
                  {row.model_id ? <><br />{row.model_id}</> : null}
                </td>
                <td className="p-3">{row.attempts}</td>
                <td className="p-3">{pct(row.success_rate)}</td>
                <td className="p-3">{ms(row.ttft_p50_ms)} / {ms(row.ttft_p95_ms)} / {ms(row.ttft_p99_ms)}</td>
                <td className="p-3">
                  {ms(row.duration_p50_ms)} / {ms(row.duration_p95_ms)} / {ms(row.duration_p99_ms)}
                </td>
                <td className="p-3">{row.server_5xx} / {row.connection_errors} / {row.timeouts}</td>
                <td className="p-3">{row.rate_limits} / {row.quota_exhausted}</td>
                <td className="p-3">{row.auth_errors} / {row.target_errors} / {row.bad_requests}</td>
                <td className="p-3">
                  {row.fallbacks} ({row.attempts > 0 ? pct(row.fallbacks / row.attempts) : '—'}) / {row.fallback_failures}
                </td>
                <td className="p-3">{row.cancellations}</td>
                <td className="p-3">{row.adaptive_saturation} / {row.provider_circuit_rejects}</td>
              </tr>
            ))}
            {data && telemetryRows.length === 0 && (
              <tr><td className="p-4 text-[var(--ink)]/60" colSpan={12}>No persisted target telemetry in this window.</td></tr>
            )}
          </tbody>
        </table>
      </section>
    </div>
  );
}
