import React, { useState } from 'react';
import { Lock, Eye, EyeOff, ShieldCheck, KeyRound, ArrowRight } from 'lucide-react';
import { WobblyCard, SketchButton, SketchBadge } from './HandDrawnElements';
import { DESIGN_TOKENS } from '../lib/designSystem';
import { Kinetix } from '../lib/resources';

interface LoginScreenProps {
  onLogin?: (username: string) => void;
  onLoginSuccess?: (username: string) => void;
}

export const LoginScreen: React.FC<LoginScreenProps> = ({ onLogin, onLoginSuccess }) => {
  const [password, setPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);

  const notifySuccess = (user: string) => {
    if (onLoginSuccess) {
      onLoginSuccess(user);
    } else if (onLogin) {
      onLogin(user);
    }
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    const trimmedPass = password.trim();
    if (!trimmedPass) {
      setError('Please enter the admin token.');
      return;
    }

    setIsSubmitting(true);
    try {
      const r = await Kinetix.login(trimmedPass);
      notifySuccess(r.user || 'admin');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed');
    } finally {
      setIsSubmitting(false);
    }
  };

  return (
    <div className="min-h-screen bg-[var(--erased-soft)] flex flex-col items-center justify-center p-4 relative overflow-hidden">
      {/* Background hand-drawn decorative graph lines */}
      <div
        className="absolute inset-0 pointer-events-none opacity-20"
        style={{
          backgroundImage:
            'linear-gradient(var(--ink) 1px, transparent 1px), linear-gradient(90deg, var(--ink) 1px, transparent 1px)',
          backgroundSize: '40px 40px',
        }}
      />

      {/* Decorative background badges / doodles */}
      <div className="absolute top-8 left-8 hidden md:block rotate-[-4deg]">
        <div className="p-3 bg-[var(--postit)] border-2 border-[var(--ink)] sketch-shadow-sm rounded-lg max-w-[200px] text-xs font-mono">
          <span className="font-heading font-bold text-sm block mb-1">⚡ Gateway Rule #1</span>
          Ordinary API reads mask credentials. Explicit secret exports need careful handling.
        </div>
      </div>

      <div className="absolute bottom-8 right-8 hidden md:block rotate-[3deg]">
        <div className="p-3 bg-[var(--tint-green)] border-2 border-[var(--ink)] sketch-shadow-sm rounded-lg max-w-[220px] text-xs font-mono">
          <span className="font-heading font-bold text-sm text-[var(--success-text)] block mb-1">🛡️ Administrator Access</span>
          Use virtual keys for clients. Keep administrator credentials private.
        </div>
      </div>

      {/* Central Login Card */}
      <div className="w-full max-w-md relative z-10 my-8">
        <WobblyCard decoration="tape" className="p-7 md:p-8 bg-[var(--paper)]">
          {/* Logo & Header */}
          <div className="flex items-center justify-between mb-6 pb-4 border-b-2 border-dashed border-[var(--ink)]/30">
            <div className="flex items-center gap-3">
              <div
                className="w-12 h-12 bg-[var(--marker-red)] text-[var(--surface)] flex items-center justify-center font-heading font-bold text-3xl border-2 border-[var(--ink)] sketch-shadow -rotate-2 select-none"
                style={{ borderRadius: '255px 15px 225px 15px / 15px 225px 15px 255px' }}
              >
                K
              </div>
              <div>
                <h1 className="text-3xl font-heading font-bold tracking-tight text-[var(--ink)]">
                  Kinetix
                </h1>
                <p className="text-xs font-mono text-[var(--ink)]/70 -mt-0.5">
                  LLM Proxy & Routing Gateway
                </p>
              </div>
            </div>

            <SketchBadge variant="yellow" rotation="2deg" className="text-xs font-heading">
              Admin Portal
            </SketchBadge>
          </div>

          <div className="mb-5">
            <h2 className="text-xl font-heading font-bold text-[var(--ink)]">
              Sign in to Gateway
            </h2>
            <p className="text-sm font-body text-[var(--ink)]/80 mt-0.5">
              Sign in to manage Routes, Accounts, and Providers.
            </p>
          </div>

          {error && (
            <div className="mb-4 p-3 bg-[var(--tint-red)] border-2 border-[var(--marker-red)] rounded-lg text-xs font-mono text-[var(--danger-text)] flex items-center gap-2">
              <span className="font-bold">⚠️ Error:</span>
              <span>{error}</span>
            </div>
          )}

          {/* Form */}
          <form onSubmit={handleSubmit} className="space-y-4">
            <div>
              <label
                htmlFor="login-password"
                className="block text-sm font-heading font-bold text-[var(--ink)] mb-1"
              >
                Admin Token
              </label>
              <div className="relative">
                <div className="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none text-[var(--ink)]/60">
                  <Lock className="w-4 h-4" />
                </div>
                <input
                  id="login-password"
                  type={showPassword ? 'text' : 'password'}
                  required
                  autoFocus
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  placeholder="KINETIX_ADMIN_TOKEN…"
                  className="w-full bg-[var(--surface)] border-2 border-[var(--ink)] pl-9 pr-10 py-2 text-base font-mono sketch-shadow-sm focus:outline-none focus:bg-[var(--tint-yellow)]"
                  style={{ borderRadius: DESIGN_TOKENS.radii.wobbly }}
                />
                <button
                  type="button"
                  onClick={() => setShowPassword(!showPassword)}
                  className="absolute inset-y-0 right-0 pr-3 flex items-center text-[var(--ink)]/60 hover:text-[var(--ink)] cursor-pointer"
                  title={showPassword ? 'Hide token' : 'Show token'}
                >
                  {showPassword ? <EyeOff className="w-4 h-4" /> : <Eye className="w-4 h-4" />}
                </button>
              </div>
              <p className="text-xs text-[var(--ink)]/60 mt-1 font-body">
                The value of <code className="font-mono">KINETIX_ADMIN_TOKEN</code> configured on the server.
              </p>
            </div>

            {/* Buttons */}
            <div className="space-y-2.5 pt-2">
              <SketchButton
                id="btn-submit-login"
                type="submit"
                variant="primary"
                size="md"
                className="w-full justify-center gap-2 font-heading font-bold text-lg"
                disabled={isSubmitting}
              >
                <KeyRound className="w-5 h-5" />
                {isSubmitting ? 'Verifying Gateway...' : 'Unlock Gateway Dashboard'}
                <ArrowRight className="w-4 h-4" />
              </SketchButton>
            </div>
          </form>

          {/* Handwritten Sticky Note attached at bottom */}
          <div className="mt-6 pt-4 border-t-2 border-dashed border-[var(--ink)]/30">
            <div className="p-3 bg-[var(--tint-orange)] border border-[var(--ink)] rounded-md text-xs font-mono text-[var(--ink)]/80 relative">
              <span className="font-heading font-bold text-[var(--warn-text)] block mb-1">
                📌 Authentication Note:
              </span>
              <div>
                Sessions are validated server-side using an httpOnly cookie.
              </div>
              <div className="mt-1 text-[11px] text-[var(--ink)]/60">
                🔒 Protect credential exports and generated client profiles.
              </div>
            </div>
          </div>
        </WobblyCard>
      </div>

      <div className="text-xs font-mono text-[var(--ink)]/60 text-center relative z-10 flex items-center gap-1.5">
        <ShieldCheck className="w-4 h-4 text-[var(--pen-green)]" />
        Kinetix LLM Gateway • Administrator Access
      </div>
    </div>
  );
};
