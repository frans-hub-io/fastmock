import { describe, expect, it } from 'vitest';
import { errorFromResponse } from './http';

const answer = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

describe('errorFromResponse', () => {
  it('shows what a forbidden answer is missing instead of the word forbidden', async () => {
    const e = await errorFromResponse(
      answer(403, {
        error: 'forbidden',
        details: ['requires permission files:write in space billing to grant role editor there'],
        required: 'files:write',
      }),
    );
    expect(e.status).toBe(403);
    expect(e.message).toBe('requires permission files:write in space billing to grant role editor there');
  });

  it('keeps the error text of other answers', async () => {
    expect((await errorFromResponse(answer(403, { error: 'requires role admin', details: [] }))).message).toBe('requires role admin');
    expect((await errorFromResponse(answer(403, { error: 'forbidden', details: [] }))).message).toBe('forbidden');
    expect((await errorFromResponse(answer(400, { error: 'bad stub', details: ['x'] }))).message).toBe('bad stub');
  });
});
