import { Alert, Badge, Button, Code, Group, Paper, Select, Stack, Tabs, Text, Textarea, TextInput } from '@mantine/core';
import { IconSend } from '@tabler/icons-react';
import { useEffect, useState } from 'react';
import { defaultDataPlaneBase, storeDataPlaneBase, storedDataPlaneBase, useUiConfig } from '../api/uiConfig';
import { headerLines, joinUrl, parseHeaderLines, toCurl, type SampleRequest } from '../lib/curl';
import { BodyView, Copy, KeyValueTable, StatusBadge } from './common';

const METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'];

interface Result {
  status: number;
  headers: Record<string, string>;
  body: string;
  ms: number;
}

const FORBIDDEN_HEADERS = new Set(['host', 'content-length', 'connection', 'origin', 'referer', 'cookie', 'user-agent', 'accept-encoding']);

export function TestRequestPanel({ initial }: { initial: SampleRequest }) {
  const cfg = useUiConfig();
  const [base, setBase] = useState<string>(() => storedDataPlaneBase() ?? '');
  const [method, setMethod] = useState(initial.method);
  const [path, setPath] = useState(initial.path);
  const [query, setQuery] = useState(initial.query);
  const [headers, setHeaders] = useState(headerLines(initial.headers));
  const [body, setBody] = useState(initial.body);
  const [result, setResult] = useState<Result | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!base && cfg.data) setBase(defaultDataPlaneBase(cfg.data));
  }, [cfg.data, base]);

  const initKey = JSON.stringify(initial);
  useEffect(() => {
    const i = JSON.parse(initKey) as SampleRequest;
    setMethod(i.method);
    setPath(i.path);
    setQuery(i.query);
    setHeaders(headerLines(i.headers));
    setBody(i.body);
  }, [initKey]);

  const req: SampleRequest = { method, path, query, headers: parseHeaderLines(headers), body };
  const curl = toCurl(base || 'http://localhost:8080', req);

  const send = async () => {
    setBusy(true);
    setFailure(null);
    setResult(null);
    storeDataPlaneBase(base);
    const started = performance.now();
    try {
      const h = new Headers();
      for (const [k, v] of Object.entries(req.headers)) if (!FORBIDDEN_HEADERS.has(k.toLowerCase())) h.set(k, v);
      const res = await fetch(joinUrl(base, path, query), {
        method,
        headers: h,
        body: method === 'GET' || method === 'HEAD' ? null : body || null,
        mode: 'cors',
        credentials: 'omit',
        cache: 'no-store',
        redirect: 'manual',
      });
      const text = await res.text();
      const rh: Record<string, string> = {};
      res.headers.forEach((v, k) => {
        rh[k] = v;
      });
      setResult({ status: res.status, headers: rh, body: text, ms: Math.round(performance.now() - started) });
    } catch {
      setFailure(
        'The browser could not complete the request. The data plane is on another origin, so CORS (or the admin page security policy) can block it. Run the curl command below instead, or add CORS headers to the stub response.',
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Stack gap="sm">
      <TextInput
        label="Data plane base URL"
        description="Where the mock port is reachable from your browser"
        value={base}
        onChange={(e) => setBase(e.currentTarget.value)}
        data-testid="dataplane-base"
      />
      <Group gap="xs" align="flex-end" wrap="nowrap">
        <Select data={METHODS} value={method} onChange={(v) => v && setMethod(v)} w={110} aria-label="Method" allowDeselect={false} />
        <TextInput value={path} onChange={(e) => setPath(e.currentTarget.value)} aria-label="Path" style={{ flex: 2 }} className="mono" />
        <TextInput value={query} onChange={(e) => setQuery(e.currentTarget.value)} aria-label="Query string" placeholder="a=1&b=2" style={{ flex: 1 }} />
      </Group>
      <Textarea label="Headers" description="One per line: Name: value" value={headers} onChange={(e) => setHeaders(e.currentTarget.value)} autosize minRows={2} maxRows={8} classNames={{ input: 'mono' }} />
      {method !== 'GET' && method !== 'HEAD' && (
        <Textarea label="Body" value={body} onChange={(e) => setBody(e.currentTarget.value)} autosize minRows={3} maxRows={14} classNames={{ input: 'mono' }} />
      )}
      <Group>
        <Button leftSection={<IconSend size={16} />} onClick={() => void send()} loading={busy} data-testid="send-test-request">
          Send
        </Button>
      </Group>
      {failure && (
        <Alert color="yellow" title="Request blocked" role="alert">
          {failure}
        </Alert>
      )}
      {result && (
        <Paper withBorder p="sm" radius="md" data-testid="test-result">
          <Group gap="xs" mb="xs">
            <StatusBadge status={result.status} />
            <Badge variant="outline" color="gray">
              {result.ms} ms
            </Badge>
          </Group>
          <Tabs defaultValue="body">
            <Tabs.List>
              <Tabs.Tab value="body">Body</Tabs.Tab>
              <Tabs.Tab value="headers">Headers</Tabs.Tab>
            </Tabs.List>
            <Tabs.Panel value="body" pt="xs">
              <BodyView body={result.body} contentType={result.headers['content-type']} />
            </Tabs.Panel>
            <Tabs.Panel value="headers" pt="xs">
              <KeyValueTable data={result.headers} />
            </Tabs.Panel>
          </Tabs>
        </Paper>
      )}
      <div>
        <Group justify="space-between" mb={4}>
          <Text size="sm" fw={500}>
            curl
          </Text>
          <Copy value={curl} label="Copy curl command" />
        </Group>
        <Code block data-testid="curl-command">
          {curl}
        </Code>
      </div>
    </Stack>
  );
}
