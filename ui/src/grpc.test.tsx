import { MantineProvider } from '@mantine/core';
import { render, screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import type { JournalEntry, Stub } from './api/types';
import { GrpcCallView, GrpcStubSummary } from './components/GrpcView';
import { grpcCodeName } from './lib/grpc';
import { theme } from './theme';

function plain(ui: React.ReactElement) {
  return render(
    <MantineProvider theme={theme} withCssVariables={false} withGlobalClasses={false}>
      {ui}
    </MantineProvider>,
  );
}

const entry: JournalEntry = {
  id: 7,
  at: '2026-11-20T10:00:00Z',
  source: 'stubs',
  method: 'POST',
  path: '/shop.v1.OrderService/UploadItems',
  query: '',
  headers: { 'content-type': 'application/grpc' },
  body: '[{"sku":"A","qty":1},{"sku":"B","qty":2}]',
  status: 200,
  responseBody: '{"count":2}',
  matched: true,
  durationMs: 12,
  grpc: {
    kind: 'clientStreaming',
    status: 3,
    message: 'bad sku',
    requestMessages: 40,
    responseMessages: 0,
    requestTruncated: true,
  },
};

describe('gRPC views', () => {
  it('names status codes', () => {
    expect(grpcCodeName(5)).toBe('NOT_FOUND');
    expect(grpcCodeName('unavailable')).toBe('UNAVAILABLE');
    expect(grpcCodeName(undefined)).toBe('OK');
  });

  it('lists the messages of a journaled stream with the status and a truncation marker', () => {
    plain(
      <>
        <GrpcCallView entry={entry} side="request" />
        <GrpcCallView entry={entry} side="response" />
      </>,
    );
    const req = screen.getByTestId('grpc-request');
    expect(within(req).getByText('gRPC client streaming')).toBeInTheDocument();
    expect(within(req).getByText('40 messages')).toBeInTheDocument();
    expect(within(req).getByText('#1')).toBeInTheDocument();
    expect(screen.getByTestId('grpc-request-truncated')).toHaveTextContent('journal shows the first 2');
    const resp = screen.getByTestId('grpc-response');
    expect(within(resp).getByTestId('grpc-status')).toHaveTextContent('INVALID_ARGUMENT');
    expect(within(resp).getByText('bad sku')).toBeInTheDocument();
  });

  it('summarises the stream settings of a stub', () => {
    const stub: Stub = {
      request: { urlPath: '/shop.v1.OrderService/Chat', grpc: { messageCount: { min: 1 }, lastMessage: [{ jsonPath: '$.text' }] } },
      response: {
        status: 200,
        templated: true,
        grpc: {
          messages: [{ jsonBody: { text: 'welcome' } }, { jsonBody: { text: 'tick' }, delayMs: 500 }],
          repeat: true,
          maxMessages: 10,
          status: { code: 'UNAVAILABLE', message: 'gone', details: [{ '@type': 'type.googleapis.com/google.rpc.RetryInfo' }] },
          onMessage: [{ match: [{ jsonPath: '$.text' }], messages: [{ jsonBody: { text: 'ok' } }], status: { code: 0 } }],
        },
      },
    };
    plain(<GrpcStubSummary request={stub.request} response={stub.response} />);
    expect(screen.getByTestId('grpc-request')).toHaveTextContent('messages 1…∞');
    const rows = within(screen.getByTestId('grpc-messages')).getAllByRole('row');
    expect(rows).toHaveLength(3);
    expect(rows[2]).toHaveTextContent('500 ms');
    expect(screen.getByText('repeat · max 10 messages')).toBeInTheDocument();
    expect(screen.getByTestId('grpc-status')).toHaveTextContent('UNAVAILABLE');
    expect(screen.getByText('detail type.googleapis.com/google.rpc.RetryInfo')).toBeInTheDocument();
    expect(screen.getByTestId('grpc-rules')).toHaveTextContent('1 matcher → 1 message, then OK');
  });

  it('renders nothing for a plain HTTP stub', () => {
    const { container } = plain(<GrpcStubSummary request={{ urlPath: '/x' }} response={{ status: 200, templated: true }} />);
    expect(container.querySelector('[data-testid="grpc-summary"]')).toBeNull();
  });
});
