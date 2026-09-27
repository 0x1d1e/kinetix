import React from 'react';
import {
  ShieldCheck,
  Activity,
  DollarSign,
  Play,
  Key,
  Shuffle,
  Server,
  Users,
  BarChart3,
  Radio,
  Compass,
  History,
  LogOut,
  RefreshCw,
  Menu,
  X,
  Settings,
  Sun,
  Moon,
  Monitor,
  Puzzle,
} from 'lucide-react';
import { ProxyMetrics } from '../types';
import { formatCurrency } from '../lib/designSystem';
import { SketchButton, SketchBadge } from './HandDrawnElements';
import type { ThemeMode } from '../lib/theme';

export type NavTab = 'keys' | 'routes' | 'providers' | 'accounts' | 'plugins' | 'usage' | 'requests' | 'health' | 'aliases' | 'audit' | 'settings';

export const TAB_ROUTES: Record<NavTab, string> = {
  keys: '/admin/keys',
  routes: '/admin/routes',
  providers: '/admin/providers',
  accounts: '/admin/accounts',
  plugins: '/admin/plugins',
  usage: '/admin/usage',
  requests: '/admin/requests',
  health: '/admin/health',
  aliases: '/admin/aliases',
  audit: '/admin/audit',
  settings: '/admin/settings',
};

interface NavItem {
  id: NavTab;
  label: string;
  icon: React.ReactNode;
  badge?: string;
}

/**
 * The navigation is grouped by intent — what a new operator does first (hand out
 * a key, route it), then how the gateway is wired (upstreams, accounts, names),
 * then how to observe it — so the sidebar reads top-to-bottom as a workflow
 * rather than an undifferentiated row of tabs.
 */
export const NAV_GROUPS: { label: string; items: NavItem[] }[] = [
  {
    label: 'Gateway',
    items: [
      { id: 'keys', label: 'Virtual Keys', icon: <Key className="w-5 h-5" /> },
      { id: 'routes', label: 'Routes & Fallback', icon: <Shuffle className="w-5 h-5" />, badge: 'Active' },
    ],
  },
  {
    label: 'Configuration',
    items: [
      { id: 'providers', label: 'Upstream Providers', icon: <Server className="w-5 h-5" /> },
      { id: 'accounts', label: 'Accounts & Pools', icon: <Users className="w-5 h-5" /> },
      { id: 'plugins', label: 'Plugins & Integrations', icon: <Puzzle className="w-5 h-5" /> },
      { id: 'aliases', label: 'Model Aliases', icon: <Compass className="w-5 h-5" /> },
    ],
  },
  {
    label: 'Observability',
    items: [
      { id: 'usage', label: 'Usage & Spend', icon: <BarChart3 className="w-5 h-5" /> },
      { id: 'requests', label: 'Request Inspector', icon: <Radio className="w-5 h-5" />, badge: 'Live' },
      { id: 'health', label: 'Runtime Health', icon: <Activity className="w-5 h-5" /> },
      { id: 'audit', label: 'Audit Log', icon: <History className="w-5 h-5" /> },
    ],
  },
  {
    label: 'System',
    items: [{ id: 'settings', label: 'Settings & Security', icon: <Settings className="w-5 h-5" /> }],
  },
];

export function tabLabel(tab: NavTab): string {
  for (const g of NAV_GROUPS) {
    const found = g.items.find((i) => i.id === tab);
    if (found) return found.label;
  }
  return 'Kinetix';
}

function Brand({ compact = false }: { compact?: boolean }) {
  return (
    <div className="flex items-center gap-3">
      <div
        className="w-11 h-11 bg-[var(--marker-red)] text-[var(--surface)] flex items-center justify-center font-heading font-bold text-2xl border-2 border-[var(--ink)] sketch-shadow -rotate-2 select-none shrink-0"
        style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
      >
        K
      </div>
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <h1 className="text-2xl font-heading font-bold tracking-tight text-[var(--ink)]">Kinetix</h1>
          <SketchBadge variant="yellow" rotation="1deg" className="text-xs font-heading">
            v0.1
          </SketchBadge>
        </div>
        {!compact && (
          <p className="text-xs text-[var(--ink)]/70 font-body leading-tight">
            Multi-Protocol LLM Proxy
          </p>
        )}
      </div>
    </div>
  );
}

interface SidebarProps {
  activeTab: NavTab;
  onSelectTab: (tab: NavTab) => void;
  currentUser?: string;
  onLogout?: () => void;
  /** Mobile drawer open state (ignored on lg+ where the rail is always shown). */
  mobileOpen: boolean;
  onCloseMobile: () => void;
}

export const Sidebar: React.FC<SidebarProps> = ({
  activeTab,
  onSelectTab,
  currentUser,
  onLogout,
  mobileOpen,
  onCloseMobile,
}) => {
  const nav = (
    <nav className="flex-1 overflow-y-auto px-3 py-4 space-y-5">
      {NAV_GROUPS.map((group) => (
        <div key={group.label}>
          <div className="px-2 mb-1.5 text-[0.7rem] font-heading font-bold uppercase tracking-[0.15em] text-[var(--ink)]/45">
            {group.label}
          </div>
          <div className="space-y-1">
            {group.items.map((item) => {
              const isActive = activeTab === item.id;
              return (
                <a
                  key={item.id}
                  id={`tab-${item.id}`}
                  href={TAB_ROUTES[item.id]}
                  onClick={(e) => {
                    e.preventDefault();
                    onSelectTab(item.id);
                    onCloseMobile();
                  }}
                  className={`group relative flex items-center gap-3 pl-3 pr-2 py-2 border-2 transition-all select-none no-underline cursor-pointer ${
                    isActive
                      ? 'bg-[var(--surface)] border-[var(--ink)] sketch-shadow-sm font-bold -translate-y-0.5'
                      : 'bg-transparent border-transparent hover:bg-[var(--erased)]/60 hover:border-[var(--ink)]/30'
                  }`}
                  style={{ borderRadius: '14px 10px 16px 10px / 10px 16px 10px 14px' }}
                >
                  {/* active marker bar */}
                  <span
                    className={`absolute left-0 top-1.5 bottom-1.5 w-1.5 rounded-full ${
                      isActive ? 'bg-[var(--marker-red)]' : 'bg-transparent'
                    }`}
                  />
                  <span className={isActive ? 'text-[var(--marker-red)]' : 'text-[var(--ink)]/60 group-hover:text-[var(--ink)]'}>
                    {item.icon}
                  </span>
                  <span className="flex-1 text-base font-heading text-[var(--ink)]">{item.label}</span>
                  {item.badge && (
                    <span
                      className={`text-[0.65rem] px-1.5 py-0.5 rounded-full border border-[var(--ink)] font-heading ${
                        item.badge === 'Live' ? 'bg-[var(--marker-red)] text-[var(--surface)] animate-pulse' : 'bg-[var(--postit)] text-[var(--ink)]'
                      }`}
                    >
                      {item.badge}
                    </span>
                  )}
                </a>
              );
            })}
          </div>
        </div>
      ))}
    </nav>
  );

  const footer = currentUser && (
    <div className="px-3 pb-4 pt-2 border-t-2 border-dashed border-[var(--ink)]/20">
      <div className="flex items-center gap-2 mb-2 px-1">
        <div className="w-2 h-2 rounded-full bg-[var(--pen-green)] shrink-0" />
        <span className="text-xs font-mono text-[var(--ink)]/80 truncate font-bold" title={`Session: ${currentUser}`}>
          {currentUser}
        </span>
      </div>
      {onLogout && (
        <button
          id="btn-logout"
          onClick={onLogout}
          className="w-full px-3 py-2 bg-[var(--surface)] hover:bg-[var(--tint-red)] text-[var(--ink)] hover:text-[var(--marker-red)] border-2 border-[var(--ink)] cursor-pointer transition-colors flex items-center justify-center gap-2 text-sm font-heading font-bold sketch-shadow-sm"
          style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
          title="Sign Out / Lock Gateway"
        >
          <LogOut className="w-4 h-4" />
          Sign Out
        </button>
      )}
    </div>
  );

  return (
    <>
      {/* Desktop rail */}
      <aside className="hidden lg:flex flex-col w-64 shrink-0 h-screen sticky top-0 bg-[var(--paper)] border-r-2 border-[var(--ink)]">
        <div className="px-4 pt-4 pb-3 border-b-2 border-dashed border-[var(--ink)]/20">
          <Brand />
        </div>
        {nav}
        {footer}
      </aside>

      {/* Mobile drawer */}
      <div
        className={`lg:hidden fixed inset-0 z-50 ${mobileOpen ? '' : 'pointer-events-none'}`}
        aria-hidden={!mobileOpen}
      >
        <div
          className={`absolute inset-0 bg-black/40 transition-opacity ${mobileOpen ? 'opacity-100' : 'opacity-0'}`}
          onClick={onCloseMobile}
        />
        <aside
          className={`absolute left-0 top-0 bottom-0 w-72 max-w-[85vw] flex flex-col bg-[var(--paper)] border-r-2 border-[var(--ink)] transition-transform duration-200 ${
            mobileOpen ? 'translate-x-0' : '-translate-x-full'
          }`}
        >
          <div className="flex items-center justify-between px-4 pt-4 pb-3 border-b-2 border-dashed border-[var(--ink)]/20">
            <Brand compact />
            <button
              onClick={onCloseMobile}
              className="p-1.5 border-2 border-[var(--ink)] bg-[var(--surface)] sketch-shadow-sm cursor-pointer"
              style={{ borderRadius: '10px 14px 10px 14px / 14px 10px 14px 10px' }}
              aria-label="Close navigation"
            >
              <X className="w-5 h-5" />
            </button>
          </div>
          {nav}
          {footer}
        </aside>
      </div>
    </>
  );
};

interface TopBarProps {
  activeTab: NavTab;
  metrics: ProxyMetrics;
  onOpenTester: () => void;
  onOpenNav: () => void;
  onRefresh?: () => void;
  isRefreshing?: boolean;
  themeMode: ThemeMode;
  onThemeChange: (mode: ThemeMode) => void;
}

const THEME_OPTIONS: { mode: ThemeMode; icon: React.ReactNode; label: string }[] = [
  { mode: 'light', icon: <Sun className="w-4 h-4" />, label: 'Light' },
  { mode: 'dark', icon: <Moon className="w-4 h-4" />, label: 'Dark' },
  { mode: 'system', icon: <Monitor className="w-4 h-4" />, label: 'System' },
];

const ThemeSwitch: React.FC<{ mode: ThemeMode; onChange: (m: ThemeMode) => void }> = ({
  mode,
  onChange,
}) => (
  <div
    className="flex items-center bg-[var(--surface)] border-2 border-[var(--ink)] sketch-shadow-sm overflow-hidden"
    style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
    role="group"
    aria-label="Color theme"
  >
    {THEME_OPTIONS.map((opt) => (
      <button
        key={opt.mode}
        onClick={() => onChange(opt.mode)}
        title={`${opt.label} theme`}
        aria-pressed={mode === opt.mode}
        className={`px-2 py-1.5 cursor-pointer transition-colors ${
          mode === opt.mode
            ? 'bg-[var(--ink)] text-[var(--surface)]'
            : 'text-[var(--ink)] hover:bg-[var(--erased)]'
        }`}
      >
        {opt.icon}
      </button>
    ))}
  </div>
);

/**
 * A slim, low-noise top bar: it holds only the current page title, at-a-glance
 * health, and the primary action — the navigation and the account control now
 * live in the sidebar, so the header no longer competes for attention.
 */
export const TopBar: React.FC<TopBarProps> = ({
  activeTab,
  metrics,
  onOpenTester,
  onOpenNav,
  onRefresh,
  isRefreshing,
  themeMode,
  onThemeChange,
}) => {
  return (
    <header className="sticky top-0 z-30 w-full bg-[var(--paper)]/95 backdrop-blur-sm border-b-2 border-[var(--ink)]">
      <div className="w-full px-4 md:px-8 py-2.5 flex items-center gap-3">
        {/* Mobile nav trigger */}
        <button
          onClick={onOpenNav}
          className="lg:hidden p-2 border-2 border-[var(--ink)] bg-[var(--surface)] sketch-shadow-sm cursor-pointer shrink-0"
          style={{ borderRadius: '12px 16px 12px 16px / 16px 12px 16px 12px' }}
          aria-label="Open navigation"
        >
          <Menu className="w-5 h-5" />
        </button>

        {/* Current page */}
        <div className="min-w-0">
          <h2 className="text-xl md:text-2xl font-heading font-bold text-[var(--ink)] truncate leading-tight">
            {tabLabel(activeTab)}
          </h2>
        </div>

        <div className="flex-1" />

        {/* At-a-glance status — grouped, quiet, wraps on small screens */}
        <div className="flex items-center gap-2 flex-wrap justify-end">
          <div
            className="hidden sm:flex items-center gap-1.5 bg-[var(--surface)] px-2.5 py-1 border-2 border-[var(--ink)] sketch-shadow-sm text-xs"
            style={{ borderRadius: '15px 225px 255px 25px / 255px 25px 225px 15px' }}
            title="Cloudflare Tunnel status"
          >
            <span className="w-2 h-2 rounded-full bg-[var(--pen-green)] animate-pulse border border-[var(--ink)]" />
            <ShieldCheck className="w-3.5 h-3.5 text-[var(--pen-blue)]" />
            <span className="font-body text-[var(--ink)]">
              Tunnel <strong className="font-heading">Online</strong>
            </span>
          </div>

          <div
            className="flex items-center gap-1.5 bg-[var(--surface)] px-2.5 py-1 border-2 border-[var(--ink)] sketch-shadow-sm text-xs"
            style={{ borderRadius: '255px 25px 225px 25px / 25px 225px 25px 255px' }}
            title="Active upstream streams"
          >
            <Activity className="w-3.5 h-3.5 text-[var(--marker-red)]" />
            <span className="font-body text-[var(--ink)]">
              <strong className="font-heading text-sm">{metrics.activeStreams}</strong> streams
            </span>
          </div>

          <div
            className="flex items-center gap-1.5 bg-[var(--postit)] px-2.5 py-1 border-2 border-[var(--ink)] sketch-shadow-sm text-xs"
            style={{ borderRadius: '20px 300px 20px 280px / 280px 20px 300px 20px' }}
            title="Total recorded spend"
          >
            <DollarSign className="w-3.5 h-3.5 text-[var(--pen-blue)]" />
            <span className="font-body text-[var(--ink)]">
              <strong className="font-heading text-sm">{formatCurrency(metrics.totalSpendUsd)}</strong>
            </span>
          </div>

          {onRefresh && (
            <button
              onClick={onRefresh}
              disabled={isRefreshing}
              className="flex items-center gap-1.5 bg-[var(--surface)] px-2.5 py-1.5 border-2 border-[var(--ink)] sketch-shadow-sm text-sm font-heading font-bold cursor-pointer hover:bg-[var(--erased)] disabled:opacity-60"
              style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
              title="Reload all data from the admin API"
            >
              <RefreshCw className={`w-4 h-4 ${isRefreshing ? 'animate-spin' : ''}`} />
              <span className="hidden md:inline">Refresh</span>
            </button>
          )}

          <ThemeSwitch mode={themeMode} onChange={onThemeChange} />

          <SketchButton
            id="btn-test-proxy"
            variant="danger"
            size="sm"
            onClick={onOpenTester}
            className="gap-1.5 font-heading font-bold"
          >
            <Play className="w-4 h-4 fill-[var(--surface)]" />
            <span className="hidden sm:inline">Live Proxy Test</span>
            <span className="sm:hidden">Test</span>
          </SketchButton>
        </div>
      </div>
    </header>
  );
};
