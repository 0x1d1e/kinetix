import assert from 'node:assert/strict';
import { test, afterEach } from 'node:test';
import { api, ApiError } from './api.ts';

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; delete globalThis.window; });

test('structured errors expose status, code, and actionable field details', async () => {
  globalThis.fetch = async () => new Response(JSON.stringify({ error: {
    code: 'validation_failed', message: 'validation failed',
    fields: [{ field: 'targets[0].weight', code: 'invalid_field', message: 'expected an integer' }],
  } }), { status: 422 });
  await assert.rejects(api.post('/admin/api/routes', {}), error => {
    assert.ok(error instanceof ApiError);
    assert.equal(error.status, 422);
    assert.equal(error.code, 'validation_failed');
    assert.equal(error.fields[0].field, 'targets[0].weight');
    assert.match(error.message, /targets\[0\]\.weight: expected an integer/);
    assert.ok(!error.message.includes('[object Object]'));
    return true;
  });
});

test('field details do not repeat an identical summary', async () => {
  globalThis.fetch = async () => new Response(JSON.stringify({ error: {
    code: 'invalid_request', message: 'endpoint must use https',
    fields: [{ field: 'base_url', code: 'invalid_value', message: 'endpoint must use https' }],
  } }), { status: 400 });
  await assert.rejects(api.post('/admin/api/providers', {}), error => {
    assert.equal(error.message, 'base_url: endpoint must use https');
    return true;
  });
});

test('legacy and non-JSON errors retain readable fallback messages', async () => {
  globalThis.fetch = async () => new Response('{"error":"old server error"}', { status: 400 });
  await assert.rejects(api.get('/admin/api/keys'), /old server error/);
  globalThis.fetch = async () => new Response('proxy unavailable', { status: 502 });
  await assert.rejects(api.get('/admin/api/keys'), /HTTP 502/);
});

test('collections follow next_offset without silently truncating resources', async () => {
  globalThis.window = { location: { origin: 'http://localhost' } };
  const paths = [];
  globalThis.fetch = async path => {
    paths.push(path);
    const offset = Number(new URL(path, 'http://localhost').searchParams.get('offset'));
    return new Response(JSON.stringify({ keys: [{ id: offset === 0 ? 'key_a' : 'key_b' }],
      page: { limit: 500, offset, total: 2, next_offset: offset === 0 ? 1 : null },
    }));
  };
  assert.deepEqual(await api.collection('/admin/api/keys?q=test', 'keys'), [{ id: 'key_a' }, { id: 'key_b' }]);
  assert.equal(paths.length, 2);
  assert.match(paths[1], /offset=1/);
  assert.match(paths[1], /q=test/);
});

test('invalid pagination cannot loop forever', async () => {
  globalThis.window = { location: { origin: 'http://localhost' } };
  globalThis.fetch = async () => new Response(JSON.stringify({ keys: [], page: { next_offset: 0 } }));
  await assert.rejects(api.collection('/admin/api/keys', 'keys'), /invalid pagination response/);
});
