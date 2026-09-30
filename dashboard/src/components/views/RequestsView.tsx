import React, { useState } from 'react';
import { Radio, Search, Filter, CheckCircle2, AlertTriangle, ArrowRight, Eye, Sparkles } from 'lucide-react';
import { LiveRequest, RequestLog } from '../../types';
import { WobblyCard, SketchBadge, SketchButton } from '../HandDrawnElements';
import { formatCurrency, formatLatency } from '../../lib/designSystem';

interface RequestsViewProps {
  requests: RequestLog[];
  liveRequests?: LiveRequest[];
}

export const RequestsView: React.FC<RequestsViewProps> = ({ requests, liveRequests = [] }) => {
  const [filterText, setFilterText] = useState('');
  const [statusFilter, setStatusFilter] = useState<'all' | 'success' | 'fallback_recovered' | 'rate_limited'>('all');
  const [selectedRequest, setSelectedRequest] = useState<RequestLog | null>(null);
  const [trace, setTrace] = useState<any | null>(null);
  const [diagnostics, setDiagnostics] = useState<any | null>(null);
  const [panel, setPanel] = useState<'trace' | 'diagnostics' | null>(null);
  const [panelError, setPanelError] = useState<string | null>(null);

  const loadTrace = () => {
    if (!selectedRequest) return;
    setPanelError(null);
    setPanel('trace');
    fetch(`/admin/api/requests/${selectedRequest.requestId}/route-trace`, { credentials: 'same-origin' })
      .then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
      .then(setTrace)
      .catch((e) => setPanelError(`Route trace unavailable (${e}).`));
  };
  const loadDiagnostics = () => {
    if (!selectedRequest) return;
    setPanelError(null);
    setPanel('diagnostics');
    fetch(`/admin/api/requests/${selectedRequest.requestId}/diagnostics`, { credentials: 'same-origin' })
      .then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
      .then(setDiagnostics)
      .catch((e) => setPanelError(`Diagnostics unavailable (${e}).`));
  };

  const filtered = requests.filter((r) => {
    if (statusFilter !== 'all' && r.status !== statusFilter) return false;
    if (filterText) {
      const q = filterText.toLowerCase();
      return (
        r.virtualKeyName.toLowerCase().includes(q) ||
        r.requestedModel.toLowerCase().includes(q) ||
        r.effectiveTarget.toLowerCase().includes(q) ||
        r.requestId.toLowerCase().includes(q)
      );
    }
    return true;
  });

  return (
    <div className="space-y-6">
      {/* Top Banner */}
      <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-4">
        <div>
          <h2 className="text-3xl font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <span>Request Inspector & Live Stream</span>
            <SketchBadge variant="red" rotation="-1deg" className="animate-pulse">
              Live Stream
            </SketchBadge>
          </h2>
          <p className="text-base font-body text-[var(--ink)]/80">
            Inspect in-flight streams, TTFT latency, exact fallback traces, and upstream response headers.
          </p>
        </div>

        {/* Filter controls */}
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative">
            <Search className="w-4 h-4 absolute left-3 top-1/2 -translate-y-1/2 text-[var(--ink)]/50" />
            <input
              type="text"
              placeholder="Search key, model, req ID..."
              value={filterText}
              onChange={(e) => setFilterText(e.target.value)}
              className="bg-[var(--surface)] border-2 border-[var(--ink)] pl-9 pr-3 py-1.5 text-sm font-body sketch-shadow-sm focus:outline-none"
              style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
            />
          </div>

          <select
            value={statusFilter}
            onChange={(e) => setStatusFilter(e.target.value as any)}
            className="bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-1.5 text-sm font-heading sketch-shadow-sm focus:outline-none"
            style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
          >
            <option value="all">All Statuses</option>
            <option value="success">200 OK Direct</option>
            <option value="fallback_recovered">⚡ Fallback Recovered</option>
            <option value="rate_limited">429 Rate Limited</option>
          </select>
        </div>
      </div>


      {/* Live in-flight view (FR-8.3): metadata-only, no bodies. */}
      <WobblyCard decoration="tack-blue" className="p-4">
        <div className="flex items-center justify-between mb-3">
          <h3 className="text-lg font-heading font-bold text-[var(--ink)] flex items-center gap-2">
            <Radio className="w-4 h-4 text-[var(--marker-red)] animate-pulse" />
            In-Flight Requests
          </h3>
          <span className="text-xs font-mono text-[var(--ink)]/60">
            {liveRequests.filter((l) => !l.finished).length} active
          </span>
        </div>
        {liveRequests.length === 0 ? (
          <p className="text-sm font-body text-[var(--ink)]/60">
            No in-flight requests right now. Send one through the Live Proxy Test to watch it
            commit, stream, and finalize here.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left font-mono text-xs">
              <thead className="text-[var(--ink)]/70 font-heading">
                <tr>
                  <th className="py-1 pr-3">Phase</th>
                  <th className="py-1 pr-3">Request ID</th>
                  <th className="py-1 pr-3">Key</th>
                  <th className="py-1 pr-3">Model</th>
                  <th className="py-1 pr-3">Fallback</th>
                  <th className="py-1 pr-3">TTFT</th>
                  <th className="py-1 pr-3">In / Out</th>
                  <th className="py-1 pr-3">Total</th>
                </tr>
              </thead>
              <tbody>
                {liveRequests.map((l) => (
                  <tr key={l.requestId} className="border-t border-[var(--ink)]/15">
                    <td className="py-1 pr-3">
                      {l.finished ? (
                        <SketchBadge variant={l.status === 'success' ? 'green' : 'red'}>
                          {l.status}
                        </SketchBadge>
                      ) : (
                        <SketchBadge variant="blue">{l.phase}</SketchBadge>
                      )}
                    </td>
                    <td className="py-1 pr-3 truncate max-w-[160px]">{l.requestId}</td>
                    <td className="py-1 pr-3">{l.keyName || '—'}</td>
                    <td className="py-1 pr-3">
                      {l.requestedModel}
                      {l.routeName ? ` (${l.routeName})` : ''}
                    </td>
                    <td className="py-1 pr-3">
                      {l.fallbackHops > 0 ? `⚡ ${l.fallbackHops}` : '—'}
                    </td>
                    <td className="py-1 pr-3">{l.ttftMs != null ? formatLatency(l.ttftMs) : '—'}</td>
                    <td className="py-1 pr-3">
                      {l.inputTokens ?? '?'} / {l.outputTokens ?? '?'}
                    </td>
                    <td className="py-1 pr-3">
                      {l.finished ? formatLatency(l.latencyMs) : 'streaming…'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </WobblyCard>

      {/* Requests Table */}
      <WobblyCard decoration="tape" className="p-0 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-left font-body text-base">
            <thead className="bg-[var(--erased)] border-b-2 border-[var(--ink)] font-heading font-bold text-sm">
              <tr>
                <th className="p-3">Status</th>
                <th className="p-3">Request ID & Time</th>
                <th className="p-3">Virtual Key</th>
                <th className="p-3">Requested → Served</th>
                <th className="p-3">Fallback Hops</th>
                <th className="p-3">TTFT / Total</th>
                <th className="p-3">Tokens (In / Out)</th>
                <th className="p-3">Cost</th>
                <th className="p-3 text-right">Inspect</th>
              </tr>
            </thead>
            <tbody className="divide-y-2 divide-[var(--ink)]/15 bg-[var(--surface)] font-mono text-xs">
              {filtered.map((r) => {
                const isFallback = r.fallbackHops > 0;

                return (
                  <tr
                    key={r.id}
                    onClick={() => setSelectedRequest(r)}
                    className="hover:bg-[var(--paper)] cursor-pointer transition-colors"
                  >
                    <td className="p-3 whitespace-nowrap">
                      {isFallback ? (
                        <SketchBadge variant="yellow" rotation="-1deg">
                          ⚡ Recovered
                        </SketchBadge>
                      ) : r.status === 'success' ? (
                        <SketchBadge variant="green">
                          200 OK
                        </SketchBadge>
                      ) : (
                        <SketchBadge variant="red">
                          {r.statusCode} Err
                        </SketchBadge>
                      )}
                    </td>
                    <td className="p-3 whitespace-nowrap">
                      <div className="font-bold text-[var(--ink)]">{r.requestId}</div>
                      <div className="text-[var(--ink)]/60 text-[10px]">
                        {new Date(r.timestamp).toLocaleTimeString()}
                      </div>
                    </td>
                    <td className="p-3 font-body text-sm text-[var(--ink)] max-w-[140px] truncate">
                      {r.virtualKeyName}
                    </td>
                    <td className="p-3 whitespace-nowrap">
                      <div className="flex items-center gap-1.5 font-bold">
                        <span className="text-[var(--pen-blue)]">{r.requestedModel}</span>
                        <ArrowRight className="w-3 h-3 text-[var(--ink)]" />
                        <span>{r.effectiveTarget}</span>
                      </div>
                      <div className="text-[10px] text-[var(--ink)]/60 truncate max-w-[180px]">
                        Account: {r.servingAccount}
                      </div>
                    </td>
                    <td className="p-3 whitespace-nowrap">
                      {isFallback ? (
                        <span className="font-bold text-[var(--marker-orange)] bg-[var(--postit)] px-1.5 py-0.5 border border-[var(--ink)] rounded">
                          {r.fallbackHops} hop (429 retried)
                        </span>
                      ) : (
                        <span className="text-[var(--ink)]/50">0 (Direct)</span>
                      )}
                    </td>
                    <td className="p-3 whitespace-nowrap">
                      <div>
                        TTFT: <strong>{r.ttftMs}ms</strong>
                      </div>
                      <div className="text-[var(--ink)]/60">
                        Total: {formatLatency(r.latencyMs)}
                      </div>
                    </td>
                    <td className="p-3 whitespace-nowrap">
                      <div>{r.inputTokens.toLocaleString()} in</div>
                      <div className="text-[var(--ink)]/60">{r.outputTokens.toLocaleString()} out</div>
                      {r.cachedTokens > 0 && (
                        <div className="text-[var(--pen-green)] font-bold">
                          +{r.cachedTokens.toLocaleString()} cache
                        </div>
                      )}
                    </td>
                    <td className="p-3 whitespace-nowrap font-bold text-[var(--ink)]">
                      {formatCurrency(r.costUsd)}
                    </td>
                    <td className="p-3 text-right">
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          setSelectedRequest(r);
                        }}
                        className="p-1.5 hover:bg-[var(--erased)] border border-[var(--ink)] rounded cursor-pointer"
                        title="Inspect request detail"
                      >
                        <Eye className="w-4 h-4 text-[var(--pen-blue)]" />
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </WobblyCard>

      {/* Detailed Request Inspection Modal / Drawer */}
      {selectedRequest && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/40 backdrop-blur-xs">
          <div className="w-full max-w-3xl max-h-[90vh] overflow-y-auto">
            <WobblyCard decoration="tape" className="bg-[var(--paper)] p-6 relative">
              <button
                onClick={() => setSelectedRequest(null)}
                className="absolute top-4 right-4 text-[var(--ink)] font-bold text-xl hover:text-[var(--marker-red)] cursor-pointer"
              >
                ✕
              </button>

              <div className="flex items-center gap-2 mb-4">
                <Radio className="w-6 h-6 text-[var(--marker-red)]" />
                <h3 className="text-2xl font-heading font-bold text-[var(--ink)]">
                  Request Inspector: {selectedRequest.requestId}
                </h3>
              </div>

              {/* Status Header */}
              <div className="p-4 bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm rounded-lg mb-4 space-y-2">
                <div className="flex flex-wrap items-center justify-between gap-2 border-b border-[var(--ink)]/20 pb-2">
                  <div className="flex items-center gap-2">
                    <span className="font-heading font-bold text-lg">
                      {selectedRequest.virtualKeyName}
                    </span>
                    <span className="text-xs font-mono bg-[var(--erased)] px-2 py-0.5 rounded border border-[var(--ink)]/30">
                      {selectedRequest.clientFormat.toUpperCase()} Format
                    </span>
                  </div>

                  {selectedRequest.fallbackHops > 0 ? (
                    <SketchBadge variant="yellow" rotation="-1deg">
                      ⚡ Fallback Recovered ({selectedRequest.fallbackHops} hop)
                    </SketchBadge>
                  ) : (
                    <SketchBadge variant="green">
                      Direct Primary Key
                    </SketchBadge>
                  )}
                </div>

                {/* HTTP Headers returned by Kinetix */}
                <div className="bg-[var(--paper)] p-3 border border-[var(--ink)] rounded text-xs font-mono space-y-1">
                  <strong className="font-heading text-sm text-[var(--ink)] block mb-1">
                    Response Headers Injected by Kinetix
                  </strong>
                  <div><code>X-Request-Id: {selectedRequest.requestId}</code></div>
                  <div><code>X-Kinetix-Cache: {selectedRequest.cacheStatus}</code></div>
                  <div><code>X-Kinetix-Route-Id: {selectedRequest.opaqueRouteId || '(none)'}</code> <span className="text-[var(--ink)]/50">(opaque; serving topology is admin-only)</span></div>
                  {selectedRequest.fallbackHops > 0 && (
                    <div className="text-[var(--marker-red)] font-bold">
                      <code>X-Kinetix-Fallback: true (Hops: {selectedRequest.fallbackHops})</code>
                    </div>
                  )}
                  <div>
                    <code>Usage: {selectedRequest.usageConfidence} | commit: {selectedRequest.commitState || 'n/a'} | retries: {selectedRequest.retryCount}</code>
                  </div>
                </div>

                {/* Fallback Path Trace */}
                {selectedRequest.fallbackHops > 0 && (
                  <div className="p-3 bg-[var(--postit)] border border-[var(--ink)] rounded text-xs font-mono">
                    <strong className="font-heading text-sm text-[var(--ink)] block mb-1">
                      Zero-Interruption Fallback Hop Sequence:
                    </strong>
                    <div className="flex flex-wrap items-center gap-1.5">
                      {selectedRequest.fallbackPath.map((step, idx) => (
                        <React.Fragment key={idx}>
                          <span className={step.includes('429') ? 'text-[var(--marker-red)] font-bold' : 'text-[var(--pen-green)]'}>
                            {step}
                          </span>
                          {idx < selectedRequest.fallbackPath.length - 1 && (
                            <ArrowRight className="w-3.5 h-3.5 text-[var(--ink)]" />
                          )}
                        </React.Fragment>
                      ))}
                    </div>
                  </div>
                )}
              </div>

              {/* Payload Snippets. Request/response bodies are not persisted by
                  default (privacy, FR-9.x); only metadata is retained. */}
              <div className="space-y-4 text-xs font-mono">
                <div>
                  <strong className="font-heading text-base text-[var(--ink)] block mb-1">
                    Client Prompt Preview
                  </strong>
                  <div className="p-3 bg-[var(--surface)] border-2 border-[var(--ink)] rounded max-h-32 overflow-y-auto whitespace-pre-wrap">
                    {selectedRequest.promptPreview || (
                      <span className="text-[var(--ink)]/50 font-body text-sm">
                        Bodies are not stored by default. Enable body logging on the virtual key to capture short-retention previews.
                      </span>
                    )}
                  </div>
                </div>

                <div>
                  <strong className="font-heading text-base text-[var(--ink)] block mb-1">
                    Upstream Streamed Completion
                  </strong>
                  <div className="p-3 bg-[var(--surface)] border-2 border-[var(--ink)] rounded max-h-40 overflow-y-auto whitespace-pre-wrap">
                    {selectedRequest.responsePreview || (
                      <span className="text-[var(--ink)]/50 font-body text-sm">
                        Not captured (body logging disabled).
                      </span>
                    )}
                  </div>
                </div>
              </div>

              <div className="pt-4 flex flex-wrap justify-between items-center gap-2">
                <div className="flex gap-2">
                  <SketchButton variant="secondary" onClick={loadTrace}>
                    Route Trace
                  </SketchButton>
                  <SketchButton variant="secondary" onClick={loadDiagnostics}>
                    Diagnostics
                  </SketchButton>
                </div>
                <SketchButton
                  variant="secondary"
                  onClick={() => {
                    setSelectedRequest(null);
                    setPanel(null);
                    setTrace(null);
                    setDiagnostics(null);
                  }}
                >
                  Close Inspector
                </SketchButton>
              </div>

              {panelError && (
                <div className="mt-3 text-sm text-[var(--marker-red)] font-mono">{panelError}</div>
              )}

              {panel === 'trace' && trace && (
                <div className="mt-4 border-t-2 border-dashed border-[var(--ink)]/20 pt-4">
                  <div className="text-sm font-bold text-[var(--pen-blue)] mb-2">
                    <div>Route Trace · outcome: {trace.outcome} · commit: {trace.commit_state}</div>
                    <div className="mt-1 text-xs font-normal text-[var(--ink)]/60">
                      stream: {trace.stream_outcome ?? 'unknown'} · failure:{' '}
                      {trace.terminal_failure_kind ?? (trace.stream_outcome ? 'none' : 'unknown')} · fallback:{' '}
                      {trace.fallback_allowed == null ? 'unknown' : trace.fallback_allowed ? 'allowed' : 'blocked'}
                    </div>
                  </div>
                  <div className="font-mono text-xs space-y-1">
                    {(trace.steps || []).map((s: any, i: number) => (
                      <div key={i} className="flex gap-2">
                        <span className="text-[var(--ink)]/50 w-20 shrink-0">{s.elapsed_ms}ms</span>
                        <SketchBadge variant={s.stage === 'skip' ? 'red' : s.stage === 'commit' ? 'green' : 'blue'}>
                          {s.stage}
                        </SketchBadge>
                        <span className="flex-1">
                          {s.target ? <b>{s.target}</b> : null} {s.detail}
                        </span>
                      </div>
                    ))}
                  </div>
                  {(trace.warnings || []).length > 0 && (
                    <div className="mt-3 text-xs text-[var(--marker-orange)]">
                      ⚠ {(trace.warnings || []).join('; ')}
                    </div>
                  )}
                </div>
              )}

              {panel === 'diagnostics' && diagnostics && (
                <div className="mt-4 border-t-2 border-dashed border-[var(--ink)]/20 pt-4">
                  <div className="text-sm font-bold text-[var(--pen-blue)] mb-2">
                    Flight Recorder · {diagnostics.flight_events?.length || 0} events
                  </div>
                  <div className="font-mono text-xs space-y-1">
                    {(diagnostics.flight_events || []).map((e: any, i: number) => (
                      <div key={i} className="flex gap-2">
                        <span className="text-[var(--ink)]/50 w-16 shrink-0">{e.elapsed_ms}ms</span>
                        <span className="text-[var(--pen-green)] w-44 shrink-0">{e.event}</span>
                        <span className="flex-1">{e.detail}</span>
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </WobblyCard>
          </div>
        </div>
      )}
    </div>
  );
};
