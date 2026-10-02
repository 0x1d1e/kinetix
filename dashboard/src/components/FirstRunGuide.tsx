import React from 'react';
import { ArrowRight, CheckCircle2, Circle } from 'lucide-react';
import { WobblyCard, SketchButton } from './HandDrawnElements';

interface FirstRunGuideProps {
  providerCount: number;
  modelCount: number;
  keyCount: number;
  onOpenProviders: () => void;
  onCreateKey: () => void;
}

export const FirstRunGuide: React.FC<FirstRunGuideProps> = ({
  providerCount,
  modelCount,
  keyCount,
  onOpenProviders,
  onCreateKey,
}) => {
  const steps = [
    {
      title: 'Connect an upstream provider',
      description: 'Set its endpoint and connect a credential or sign-in integration.',
      done: providerCount > 0,
      action: onOpenProviders,
      actionLabel: 'Configure provider',
    },
    {
      title: 'Register a model',
      description: 'Discover model IDs from the provider or add one manually.',
      done: modelCount > 0,
      action: onOpenProviders,
      actionLabel: 'Open providers',
    },
    {
      title: 'Create a virtual key',
      description: 'Give a client a Kinetix key. Your upstream credential stays here.',
      done: keyCount > 0,
      action: onCreateKey,
      actionLabel: 'Create virtual key',
    },
  ];
  const nextIndex = steps.findIndex((step) => !step.done);

  if (nextIndex < 0) return null;

  return (
    <WobblyCard variant="muted" className="mb-6 p-5" id="first-run-guide">
      <div className="flex flex-col sm:flex-row sm:items-start sm:justify-between gap-4">
        <div>
          <h2 className="text-2xl font-heading font-bold">Get Kinetix ready</h2>
          <p className="text-sm font-body text-[var(--ink)]/75 mt-1">
            Complete these steps to send your first request. Routes and fallback are optional for later.
          </p>
        </div>
        <span className="text-sm font-mono text-[var(--ink)]/70 whitespace-nowrap">
          {steps.filter((step) => step.done).length} of {steps.length} complete
        </span>
      </div>

      <ol className="mt-4 divide-y divide-[var(--ink)]/15">
        {steps.map((step, index) => {
          const current = index === nextIndex;
          return (
            <li key={step.title} className="flex flex-col sm:flex-row sm:items-center gap-3 py-3 first:pt-0 last:pb-0">
              <div className="flex items-start gap-3 min-w-0 flex-1">
                {step.done ? (
                  <CheckCircle2 className="w-5 h-5 mt-0.5 shrink-0 text-[var(--success-text)]" aria-hidden="true" />
                ) : (
                  <Circle className="w-5 h-5 mt-0.5 shrink-0 text-[var(--ink)]/40" aria-hidden="true" />
                )}
                <div className="min-w-0">
                  <h3 className="font-heading font-bold">{step.title}</h3>
                  <p className="text-sm font-body text-[var(--ink)]/70">{step.description}</p>
                </div>
              </div>
              {current && (
                <SketchButton
                  type="button"
                  variant="primary"
                  size="sm"
                  onClick={step.action}
                  className="gap-2 self-start sm:self-center shrink-0"
                >
                  {step.actionLabel}
                  <ArrowRight className="w-4 h-4" />
                </SketchButton>
              )}
              {step.done && (
                <span className="text-xs font-heading font-bold text-[var(--success-text)] sm:w-40 sm:text-right">
                  Complete
                </span>
              )}
            </li>
          );
        })}
      </ol>
    </WobblyCard>
  );
};
