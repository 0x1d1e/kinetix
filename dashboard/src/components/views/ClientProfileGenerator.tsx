import React, { useEffect, useMemo, useState } from 'react';
import { Check, Copy, Download, FileCode2, KeyRound, WandSparkles } from 'lucide-react';
import { VirtualKey } from '../../types';
import {
  ClientProfileClient,
  ClientProfileFile,
  GeneratedClientProfile,
  Kinetix,
} from '../../lib/resources';
import { WobblyCard, SketchBadge } from '../HandDrawnElements';

interface ClientProfileGeneratorProps {
  keys: VirtualKey[];
  newlyCreatedKey: { id: string; name: string; key: string } | null;
}

const clients: Array<{
  id: ClientProfileClient;
  name: string;
  description: string;
}> = [
  { id: 'pi', name: 'Pi', description: 'OpenAI Chat Completions via Kinetix.' },
  { id: 'claude_code', name: 'Claude Code', description: 'Anthropic Messages via Kinetix.' },
  { id: 'codex', name: 'Codex', description: 'OpenAI Responses API via Kinetix.' },
  { id: 'open_code', name: 'OpenCode', description: 'OpenAI-compatible Chat Completions via Kinetix.' },
];

function usageInstruction(file: ClientProfileFile): string {
  switch (file.usage) {
    case 'write_to':
      return file.destination ? `Save this complete file to ${file.destination}.` : 'Save this complete file.';
    case 'merge_into':
      return `Merge into ${file.destination ?? 'the existing client configuration'}; do not overwrite the whole file.`;
    case 'source':
      return 'Source this helper in the shell that starts your client.';
    case 'execute':
      return 'Run this helper with bash; it starts the configured client.';
  }
}

const ClientProfileGenerator: React.FC<ClientProfileGeneratorProps> = ({ keys, newlyCreatedKey }) => {
  const activeKeys = useMemo(() => keys.filter((key) => key.status === 'active'), [keys]);
  const [keyId, setKeyId] = useState('');
  const [client, setClient] = useState<ClientProfileClient>('pi');
  const [apiKey, setApiKey] = useState('');
  const [models, setModels] = useState<Array<{ id: string }>>([]);
  const [model, setModel] = useState('');
  const [loadingModels, setLoadingModels] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const [generating, setGenerating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [profile, setProfile] = useState<GeneratedClientProfile | null>(null);
  const [activeFilename, setActiveFilename] = useState('');
  const [copiedFilename, setCopiedFilename] = useState<string | null>(null);

  useEffect(() => {
    if (newlyCreatedKey) {
      setKeyId(newlyCreatedKey.id);
      setApiKey(newlyCreatedKey.key);
      setProfile(null);
    }
  }, [newlyCreatedKey]);

  useEffect(() => {
    if (!keyId) {
      setModels([]);
      setModel('');
      setModelsError(null);
      return;
    }
    let current = true;
    setLoadingModels(true);
    setModelsError(null);
    setModels([]);
    setModel('');
    Kinetix.clientProfileModels(keyId)
      .then(({ models: available }) => {
        if (!current) return;
        setModels(available);
        setModel(available[0]?.id ?? '');
      })
      .catch((cause: unknown) => {
        if (!current) return;
        setModelsError(cause instanceof Error ? cause.message : 'Could not load authorized models.');
      })
      .finally(() => {
        if (current) setLoadingModels(false);
      });
    return () => {
      current = false;
    };
  }, [keyId]);

  const activeFile = profile?.files.find((file) => file.filename === activeFilename)
    ?? profile?.files[0]
    ?? null;

  const handleGenerate = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!keyId || !model) return;
    setGenerating(true);
    setError(null);
    setProfile(null);
    try {
      const generated = await Kinetix.generateClientProfile({
        key_id: keyId,
        client,
        model,
        api_key: apiKey.trim() || undefined,
      });
      setProfile(generated);
      setActiveFilename(generated.files[0]?.filename ?? '');
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Could not generate this profile.');
    } finally {
      setGenerating(false);
    }
  };

  const copyFile = async (file: ClientProfileFile) => {
    try {
      await navigator.clipboard.writeText(file.content);
      setCopiedFilename(file.filename);
      window.setTimeout(() => setCopiedFilename(null), 1800);
    } catch {
      setError('Clipboard access failed. Select and copy the profile text instead.');
    }
  };

  const downloadFile = (file: ClientProfileFile) => {
    const blob = new Blob([file.content], { type: `${file.content_type};charset=utf-8` });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = file.filename;
    link.click();
    window.setTimeout(() => URL.revokeObjectURL(url), 1000);
  };

  const selectedKey = activeKeys.find((key) => key.id === keyId);
  const hasNewKey = !!newlyCreatedKey && newlyCreatedKey.id === keyId;

  return (
    <section aria-labelledby="client-profile-heading">
      <WobblyCard decoration="tack" variant="muted" className="p-5">
        <div className="flex flex-wrap items-start justify-between gap-4 mb-4">
          <div className="flex items-start gap-3">
            <WandSparkles className="w-6 h-6 text-[var(--pen-blue)] shrink-0 mt-1" />
            <div>
              <h3 id="client-profile-heading" className="text-2xl font-heading font-bold text-[var(--ink)]">
                Generate a client connection profile
              </h3>
              <p className="text-sm font-body text-[var(--ink)]/75 mt-1">
                Create client-ready configuration for Pi, Claude Code, Codex, or OpenCode. Files are previewed and downloaded only; Kinetix does not write to your filesystem.
              </p>
            </div>
          </div>
          <SketchBadge variant="blue">Kinetix endpoint only</SketchBadge>
        </div>

        <form onSubmit={handleGenerate} className="grid grid-cols-1 md:grid-cols-2 gap-4">
          <label className="block text-sm font-heading font-bold text-[var(--ink)]">
            Virtual key
            <select
              value={keyId}
              onChange={(event) => {
                const nextId = event.target.value;
                setKeyId(nextId);
                setApiKey(nextId === newlyCreatedKey?.id ? newlyCreatedKey.key : '');
                setProfile(null);
                setError(null);
              }}
              className="mt-1 w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
              required
            >
              <option value="">Select an active virtual key</option>
              {activeKeys.map((key) => (
                <option key={key.id} value={key.id}>{key.name} ({key.id})</option>
              ))}
            </select>
          </label>

          <label className="block text-sm font-heading font-bold text-[var(--ink)]">
            Client
            <select
              value={client}
              onChange={(event) => {
                setClient(event.target.value as ClientProfileClient);
                setProfile(null);
              }}
              className="mt-1 w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-sm focus:outline-none focus:border-[var(--pen-blue)]"
            >
              {clients.map((option) => <option key={option.id} value={option.id}>{option.name}</option>)}
            </select>
          </label>

          <label className="block text-sm font-heading font-bold text-[var(--ink)]">
            Authorized model or Route
            <select
              value={model}
              onChange={(event) => {
                setModel(event.target.value);
                setProfile(null);
              }}
              className="mt-1 w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
              disabled={!keyId || loadingModels || models.length === 0}
              required
            >
              {loadingModels && <option value="">Loading available models...</option>}
              {!loadingModels && models.length === 0 && <option value="">No available models</option>}
              {models.map((entry) => <option key={entry.id} value={entry.id}>{entry.id}</option>)}
            </select>
            {modelsError && <span className="block mt-1 text-xs text-[var(--marker-red)]">{modelsError}</span>}
          </label>

          <label className="block text-sm font-heading font-bold text-[var(--ink)]">
            Full virtual key <span className="font-normal text-[var(--ink)]/60">(optional)</span>
            <input
              type="password"
              autoComplete="off"
              value={apiKey}
              onChange={(event) => {
                setApiKey(event.target.value);
                setProfile(null);
              }}
              placeholder="Paste the selected sk-kinetix-... key"
              className="mt-1 w-full bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 font-mono text-sm focus:outline-none focus:border-[var(--pen-blue)]"
            />
            <span className="block mt-1 text-xs font-body text-[var(--ink)]/65">
              {hasNewKey
                ? 'The newly issued key is filled in. It will only appear in the generated credential file.'
                : 'Kinetix stores only the key hash. Leave blank to generate a placeholder and add the key in your client.'}
            </span>
          </label>

          <div className="md:col-span-2 flex flex-wrap items-center gap-3">
            <button
              type="submit"
              disabled={!keyId || !model || loadingModels || generating}
              className="inline-flex items-center gap-2 px-4 py-2 font-heading font-bold border-2 border-[var(--ink)] bg-[var(--tint-blue)] hover:bg-[var(--pen-blue)] hover:text-white disabled:opacity-50 disabled:cursor-not-allowed sketch-shadow-sm"
            >
              <KeyRound className="w-4 h-4" />
              {generating ? 'Generating...' : 'Generate preview'}
            </button>
            <span className="text-sm font-body text-[var(--ink)]/70">
              {clients.find((option) => option.id === client)?.description}
            </span>
          </div>
        </form>

        {error && (
          <div role="alert" className="mt-3 text-sm font-body text-[var(--marker-red)]">
            {error}
          </div>
        )}

        {profile && activeFile && (
          <div className="mt-6 border-t-2 border-dashed border-[var(--ink)]/25 pt-5">
            <div className="flex flex-wrap items-start justify-between gap-3 mb-3">
              <div>
                <h4 className="font-heading font-bold text-lg text-[var(--ink)]">Profile preview</h4>
                <p className="flex flex-wrap items-center gap-x-2 text-xs font-mono text-[var(--ink)]/65 mt-1">
                  <span><span className="font-body">Endpoint:</span> {profile.public_base_url}</span>
                  <span aria-hidden="true">·</span>
                  <span><span className="font-body">Model:</span> {profile.model}</span>
                </p>
              </div>
              <p className="max-w-lg text-xs font-body text-[var(--marker-red)]">
                Credential files contain a virtual key. Keep them private. Only the selected Kinetix key is included; upstream credentials are never returned.
              </p>
            </div>

            <div role="tablist" aria-label="Generated profile files" className="flex flex-wrap gap-2 mb-2">
              {profile.files.map((file) => (
                <button
                  key={file.filename}
                  type="button"
                  role="tab"
                  aria-selected={activeFile.filename === file.filename}
                  onClick={() => setActiveFilename(file.filename)}
                  className={`px-3 py-1.5 border-2 border-[var(--ink)] font-mono text-xs ${
                    activeFile.filename === file.filename
                      ? 'bg-[var(--ink)] text-[var(--surface)]'
                      : 'bg-[var(--surface)] text-[var(--ink)] hover:bg-[var(--postit)]'
                  }`}
                >
                  <FileCode2 className="inline w-3.5 h-3.5 mr-1.5" />{file.filename}
                </button>
              ))}
            </div>

            <div className="flex flex-wrap items-center justify-between gap-2 mb-2 text-xs font-body text-[var(--ink)]/65">
              <span>{usageInstruction(activeFile)}</span>
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => void copyFile(activeFile)}
                  className="inline-flex items-center gap-1 px-2 py-1 border border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--postit)] font-heading font-bold text-[var(--ink)]"
                >
                  {copiedFilename === activeFile.filename ? <Check className="w-3.5 h-3.5" /> : <Copy className="w-3.5 h-3.5" />}
                  {copiedFilename === activeFile.filename ? 'Copied' : 'Copy'}
                </button>
                <button
                  type="button"
                  onClick={() => downloadFile(activeFile)}
                  className="inline-flex items-center gap-1 px-2 py-1 border border-[var(--ink)] bg-[var(--surface)] hover:bg-[var(--postit)] font-heading font-bold text-[var(--ink)]"
                >
                  <Download className="w-3.5 h-3.5" /> Download
                </button>
              </div>
            </div>

            <pre
              role="tabpanel"
              aria-label={activeFile.filename}
              className="max-h-[28rem] overflow-auto bg-[var(--code-bg)] text-[var(--code-fg)] p-4 rounded-lg font-mono text-xs leading-relaxed border-2 border-[var(--ink)] whitespace-pre-wrap break-words"
            >
              {activeFile.content}
            </pre>
            {selectedKey && (
              <p className="mt-2 text-xs font-body text-[var(--ink)]/60">
                Authorized for {selectedKey.name}. Profile generation does not change this key's model, provider, IP, budget, or expiry restrictions.
              </p>
            )}
          </div>
        )}
      </WobblyCard>
    </section>
  );
};

export default ClientProfileGenerator;
