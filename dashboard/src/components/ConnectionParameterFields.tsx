import React from 'react';
import { ConnectionParameters } from '../types';

export const ConnectionParameterFields: React.FC<{
  declarations: ConnectionParameters['declarations'];
  values: Record<string, string>;
  onChange: (values: Record<string, string>) => void;
  disabled?: boolean;
}> = ({ declarations, values, onChange, disabled }) => (
  <fieldset disabled={disabled} className="space-y-3 min-w-0">
    <legend className="text-sm font-heading font-bold mb-2">Public connection parameters</legend>
    {Object.entries(declarations).map(([name, field]) => (
      <label key={name} className="block text-sm font-heading font-bold">
        {name.replaceAll('_', ' ')} (required)
        <input
          type="text"
          required
          minLength={field.min_length}
          maxLength={field.max_length}
          pattern="[A-Za-z0-9_\-]+"
          autoComplete="off"
          spellCheck={false}
          value={values[name] || ''}
          onChange={(event) => onChange({ ...values, [name]: event.target.value })}
          className="mt-1 w-full min-w-0 bg-[var(--surface)] border-2 border-[var(--ink)] px-3 py-2 text-base font-mono sketch-shadow-sm focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--pen-blue)]"
        />
        <span className="block mt-1 text-xs font-body font-normal text-[var(--ink)]/70">
          {field.min_length}-{field.max_length} characters: letters, digits, underscores or hyphens. Non-secret; included in configuration exports.
        </span>
      </label>
    ))}
  </fieldset>
);
