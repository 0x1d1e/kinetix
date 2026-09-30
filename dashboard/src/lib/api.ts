// Typed client for the Kinetix admin API (/admin/api/*) and public API
// (/v1/*). All dashboard data flows through here; there is no mock data.

export type WireFormat = 'openai' | 'anthropic' | 'gemini';

export interface FieldError {
  field: string;
  code: string;
  message: string;
}

export interface ApiErrorBody {
  error: { code: string; message: string; fields: FieldError[] };
}

export interface Page {
  limit: number;
  offset: number;
  total: number;
  next_offset: number | null;
}

/** Thrown for non-2xx responses; carries the HTTP status. */
export class ApiError extends Error {
  status: number;
  code: string;
  fields: FieldError[];
  constructor(status: number, message: string, code = 'unknown_error', fields: FieldError[] = []) {
    super(message);
    this.status = status;
    this.name = 'ApiError';
    this.code = code;
    this.fields = fields;
  }
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: body !== undefined ? { 'Content-Type': 'application/json' } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });

  const text = await res.text();
  const data = text ? safeJson(text) : undefined;

  if (!res.ok) {
    const error = data?.error;
    const message = (typeof error === 'object' ? error?.message : error) ||
      data?.message || `request failed (HTTP ${res.status})`;
    const fields: FieldError[] = Array.isArray(error?.fields) ? error.fields : [];
    const details = fields.map(({ field, message }) => `${field}: ${message}`).join('; ');
    const displayMessage = !details ? String(message) :
      fields.length === 1 && fields[0].message === message ? details : `${message}: ${details}`;
    throw new ApiError(res.status, displayMessage,
      typeof error?.code === 'string' ? error.code : 'unknown_error', fields);
  }
  return data as T;
}

function safeJson(text: string): any {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

async function collection<T>(path: string, key: string): Promise<T[]> {
  const result: T[] = [];
  const url = new URL(path, window.location.origin);
  let offset = 0;
  for (;;) {
    url.searchParams.set('limit', '500');
    url.searchParams.set('offset', String(offset));
    const data = await request<Record<string, unknown>>('GET', `${url.pathname}${url.search}`);
    if (!Array.isArray(data[key])) throw new Error(`invalid collection response: ${key}`);
    result.push(...data[key] as T[]);
    const page = data.page as Page | undefined;
    // Allow dashboard upgrades against an older server during deployment.
    if (!page || page.next_offset === null) return result;
    if (!Number.isSafeInteger(page.next_offset) || page.next_offset <= offset) {
      throw new Error('invalid pagination response');
    }
    offset = page.next_offset;
  }
}

export const api = {
  collection,
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body),
  put: <T>(path: string, body?: unknown) => request<T>('PUT', path, body),
  del: <T>(path: string) => request<T>('DELETE', path),
};
