import { MantineProvider } from '@mantine/core';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import { tokenStore } from './api/http';
import { ResultBar } from './components/charts';
import { CommandSnippet, DetailList, KindBadge, Meter, SaveBar, StatStrip } from './components/common';
import { INFO, mockFetch, renderApp } from './test/render';
import { theme } from './theme';

function plain(ui: React.ReactElement) {
  return render(
    <MantineProvider theme={theme} withCssVariables={false} withGlobalClasses={false}>
      {ui}
    </MantineProvider>,
  );
}

beforeEach(() => {
  tokenStore.set('good');
  window.sessionStorage.clear();
  window.localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('shared components', () => {
  it('names an API kind and falls back to generic', () => {
    plain(
      <>
        <KindBadge kind="GRAPHQL" />
        <KindBadge kind="carrier-pigeon" />
      </>,
    );
    expect(screen.getByText('GraphQL')).toHaveAttribute('data-kind', 'graphql');
    expect(screen.getByText('carrier-pigeon')).toHaveAttribute('data-kind', 'generic');
  });

  it('lays out figures and labelled values as description lists', () => {
    plain(
      <>
        <StatStrip label="Summary" items={[{ label: 'Operations', value: 7, hint: 'from openapi', tone: 'success', testId: 'ops' }]} />
        <DetailList items={[{ label: 'Started by', value: 'ops' }, { label: 'Hidden', value: 'x', hidden: true }]} />
      </>,
    );
    expect(screen.getByTestId('ops')).toHaveTextContent('Operations7from openapi');
    expect(screen.getByTestId('ops')).toHaveAttribute('data-tone', 'success');
    expect(screen.getByText('Started by').tagName).toBe('DT');
    expect(screen.queryByText('Hidden')).not.toBeInTheDocument();
  });

  it('shows a share as a meter, clamped to 0–100', () => {
    plain(
      <>
        <Meter value={0.256} label="Low" />
        <Meter value={3} label="High" />
      </>,
    );
    expect(screen.getByRole('meter', { name: 'Low' })).toHaveAttribute('aria-valuenow', '26');
    expect(screen.getByRole('meter', { name: 'High' })).toHaveTextContent('100%');
  });

  it('says the numbers of a result bar', () => {
    plain(<ResultBar passed={5} failed={2} errors={1} total={8} />);
    expect(screen.getByRole('img', { name: '5 of 8 passed, 2 failed, 1 errors' })).toHaveTextContent('5/8');
  });

  it('offers to copy a command', () => {
    plain(<CommandSnippet command="protoc --include_imports" testId="cmd" />);
    expect(screen.getByTestId('cmd')).toHaveTextContent('protoc --include_imports');
    expect(screen.getByRole('button', { name: 'Copy command' })).toBeInTheDocument();
  });

  it('tells whether a save bar has unsaved changes', () => {
    const { rerender } = plain(
      <SaveBar dirty={false} testId="bar">
        <button type="button">Save</button>
      </SaveBar>,
    );
    expect(screen.getByTestId('bar')).toHaveTextContent('All changes saved');
    rerender(
      <MantineProvider theme={theme} withCssVariables={false} withGlobalClasses={false}>
        <SaveBar dirty testId="bar">
          <button type="button">Save</button>
        </SaveBar>
      </MantineProvider>,
    );
    expect(screen.getByTestId('bar')).toHaveTextContent('Unsaved changes');
    expect(screen.getByTestId('bar')).toHaveAttribute('data-dirty', 'true');
  });
});

const RULES = [{ id: 'slow', probability: 0.5, delay: { minMs: 200, maxMs: 900 } }];

function api(role: 'viewer' | 'editor' | 'admin') {
  const puts: unknown[] = [];
  mockFetch((url, init) => {
    if (url.pathname === '/openapi.json') return { body: { paths: { '/__admin/settings': { get: {} } }, components: { schemas: {} } } };
    if (url.pathname === '/__admin/info') return { body: INFO(role) };
    if (url.pathname === '/__admin/chaos' && init.method === 'GET') return { body: { rules: RULES } };
    if (url.pathname === '/__admin/chaos' && init.method === 'PUT') {
      puts.push(JSON.parse(init.body ?? '{}'));
      return { body: { rules: 2 } };
    }
    if (url.pathname === '/__admin/settings' && init.method === 'GET') return { body: { fixedDelayMs: null, delay: { minMs: 10, maxMs: 20 }, proxyPassThrough: true } };
    return undefined;
  });
  return puts;
}

describe('chaos screen', () => {
  it('validates a new rule inline and saves from the save bar', async () => {
    const puts = api('editor');
    renderApp(<AppRoutes />, '/chaos');
    expect(await screen.findByTestId('chaos-row')).toHaveTextContent('slow');
    const bar = screen.getByTestId('chaos-save-bar');
    expect(bar).toHaveTextContent('All changes saved');
    expect(within(bar).getByTestId('save-chaos')).toBeDisabled();

    const id = screen.getByRole('textbox', { name: /^Id/ });
    await userEvent.type(id, 'slow');
    expect(await screen.findByText('A rule with this id already exists')).toBeInTheDocument();
    await userEvent.clear(id);
    await userEvent.type(id, 'outage');
    await userEvent.click(screen.getByRole('button', { name: 'Add rule' }));
    expect(await screen.findAllByTestId('chaos-row')).toHaveLength(2);
    expect(bar).toHaveTextContent('Unsaved changes');

    await userEvent.click(within(bar).getByTestId('save-chaos'));
    await waitFor(() => expect(puts).toHaveLength(1));
    expect(puts[0]).toEqual({ rules: [RULES[0], { id: 'outage', probability: 0.1, status: 503 }] });
  });

  it('shows rules without edit controls to a viewer', async () => {
    api('viewer');
    renderApp(<AppRoutes />, '/chaos');
    expect(await screen.findByTestId('chaos-row')).toBeInTheDocument();
    expect(screen.queryByTestId('chaos-save-bar')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add rule' })).not.toBeInTheDocument();
  });
});

describe('settings screen', () => {
  it('flags an inverted range at the field and enables saving only after a change', async () => {
    api('editor');
    renderApp(<AppRoutes />, '/settings');
    const save = await screen.findByTestId('save-settings');
    expect(save).toBeDisabled();
    const max = screen.getByRole('textbox', { name: 'Maximum' });
    await userEvent.clear(max);
    await userEvent.type(max, '5');
    expect(await screen.findByText('The minimum must not exceed the maximum.')).toBeInTheDocument();
    expect(save).toBeDisabled();
    await userEvent.clear(max);
    await userEvent.type(max, '50');
    await waitFor(() => expect(save).toBeEnabled());
    expect(screen.getByTestId('settings-save-bar')).toHaveTextContent('Unsaved changes');
  });
});

describe('not found', () => {
  it('suggests screens the principal may open', async () => {
    api('viewer');
    renderApp(<AppRoutes />, '/chaos-rules/old');
    const page = await screen.findByTestId('not-found');
    expect(within(page).getByRole('heading', { name: 'Page not found' })).toBeInTheDocument();
    const nav = within(page).getByRole('navigation', { name: 'Suggested screens' });
    expect(within(nav).getAllByRole('link')[0]).toHaveAttribute('href', '/chaos');
    expect(within(page).getByRole('link', { name: /Back to the dashboard/ })).toHaveAttribute('href', '/');
  });
});
