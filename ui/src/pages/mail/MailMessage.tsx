import { Alert, Anchor, Button, Group, Paper, Popover, Stack, Table, Tabs, TagsInput, Text, TextInput, Tooltip } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import {
  IconAlertTriangle,
  IconArrowLeft,
  IconCode,
  IconDownload,
  IconFileText,
  IconHash,
  IconLink,
  IconList,
  IconMailOpened,
  IconPaperclip,
  IconPhoto,
  IconPhotoOff,
  IconShieldLock,
  IconTerminal2,
  IconTestPipe,
  IconTrash,
} from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { isNotFound } from '../../api/http';
import { fetchBlob, fetchText, mailApi, messagePath, type MailAddress, type MailAttachment, type MailMessage } from '../../api/mail';
import { confirmAction, notifyError, useApiMutation } from '../../api/mutations';
import { useCan, useSpace } from '../../auth/useAuth';
import { Copy, DetailList, KeyValueTable, PageHeader, SectionCard, ToneBadge } from '../../components/common';
import { Denied } from '../../components/Permitted';
import { SpaceBadge } from '../../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { downloadBlob } from '../../lib/download';
import { formatAbsolute, formatBytes, pretty } from '../../lib/format';
import { loadSeen, markSeen, prepareMailHtml, waitSnippet } from '../../lib/mail';
import { styleNonce } from '../../lib/nonce';
import { missingText } from '../../auth/permissions';
import classes from './Mail.module.css';
import { LabelChip, Snippet, SourceBadge } from './parts';

const RAW_PREVIEW = 64 * 1024;

/**
 * The HTML body in a sandboxed `srcdoc` frame (no scripts, no same-origin access). The admin server forbids
 * framing its responses, so the body is fetched with the API client and prepared for the UI's CSP.
 */
export function HtmlBody({ id }: { id: string }) {
  const [allow, setAllow] = useState(false);
  const q = useQuery({
    queryKey: ['mail', 'html', id, allow],
    queryFn: () => fetchText(messagePath(id, 'html'), { images: allow ? 'allow' : undefined }, 'text/html'),
    staleTime: 60_000,
  });
  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const prepared = prepareMailHtml(q.data ?? '', { nonce: styleNonce(), allowImages: allow });
  return (
    <Stack gap="xs">
      <Group justify="space-between" gap="xs" wrap="wrap">
        <Group gap={6} wrap="nowrap">
          <IconShieldLock size={16} aria-hidden />
          <Text size="xs" c="dimmed">
            Shown in a sandbox: scripts, forms and links do not work here; use Links &amp; codes.
          </Text>
        </Group>
        {prepared.remoteImages > 0 &&
          (allow ? (
            <Button size="compact-sm" variant="subtle" color="gray" leftSection={<IconPhotoOff size={14} />} onClick={() => setAllow(false)} data-testid="block-images">
              Block remote images
            </Button>
          ) : (
            <Group gap={6} wrap="nowrap">
              <Text size="xs" c="dimmed" data-testid="images-blocked">
                {prepared.remoteImages} remote image{prepared.remoteImages === 1 ? '' : 's'} blocked
              </Text>
              <Tooltip label="Loading them tells their senders the message was opened" withArrow>
                <Button size="compact-sm" variant="light" leftSection={<IconPhoto size={14} />} onClick={() => setAllow(true)} data-testid="load-images">
                  Load remote images
                </Button>
              </Tooltip>
            </Group>
          ))}
      </Group>
      <div className={classes.frameWrap}>
        <iframe className={classes.frame} sandbox="" srcDoc={prepared.html} title="Message HTML body" referrerPolicy="no-referrer" data-testid="mail-html-frame" />
      </div>
    </Stack>
  );
}

/** Addresses one per line: the name, and the address dimmed. */
function Addresses({ list }: { list: readonly MailAddress[] }) {
  if (!list.length) return <>—</>;
  return (
    <Stack gap={2}>
      {list.map((a, i) => (
        <span key={i} className={classes.addr}>
          {a.name ? (
            <>
              {a.name}
              <span className={classes.addrMuted}>
                <span className="sr-only"> &lt;</span>
                {a.address}
                <span className="sr-only">&gt;</span>
              </span>
            </>
          ) : (
            a.address
          )}
        </span>
      ))}
    </Stack>
  );
}

function TextBody({ msg }: { msg: MailMessage }) {
  if (!msg.text)
    return (
      <Text size="sm" c="dimmed">
        This message has no text body.
      </Text>
    );
  return (
    <div className="fm-cmd" data-testid="mail-text">
      <pre className="pre-flat">{msg.text}</pre>
    </div>
  );
}

function HeadersTable({ msg }: { msg: MailMessage }) {
  return (
    <Paper withBorder radius="md" className="fm-table-card">
      <div className="fm-table-wrap">
        <Table fz="sm" verticalSpacing={4} data-testid="mail-headers">
          <Table.Thead>
            <Table.Tr>
              <Table.Th w={64}>#</Table.Th>
              <Table.Th>Field</Table.Th>
              <Table.Th>Value</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {msg.headers.map((h, i) => (
              <Table.Tr key={i}>
                <Table.Td c="dimmed" className="tabular">
                  {i + 1}
                </Table.Td>
                <Table.Td className={classes.headerName}>{h.name}</Table.Td>
                <Table.Td className={classes.headerValue}>{h.value}</Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      </div>
    </Paper>
  );
}

async function downloadRaw(id: string) {
  try {
    downloadBlob(`${id}.eml`, await fetchBlob(messagePath(id, 'raw')));
  } catch (e) {
    notifyError(e, 'Download failed');
  }
}

function RawView({ id, size }: { id: string; size: number }) {
  const q = useQuery({ queryKey: ['mail', 'raw', id], queryFn: () => fetchText(messagePath(id, 'raw')), staleTime: 60_000 });
  return (
    <Stack gap="xs">
      <Group justify="space-between" wrap="wrap">
        <Text size="xs" c="dimmed">
          {formatBytes(size)} as received{size > RAW_PREVIEW ? `; the preview shows the first ${formatBytes(RAW_PREVIEW)}` : ''}.
        </Text>
        <Button size="compact-sm" variant="light" leftSection={<IconDownload size={14} />} onClick={() => void downloadRaw(id)} data-testid="download-raw">
          Download .eml
        </Button>
      </Group>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : (
        <div className="fm-cmd" data-testid="mail-raw">
          <pre className="pre-flat">{(q.data ?? '').slice(0, RAW_PREVIEW)}</pre>
        </div>
      )}
    </Stack>
  );
}

function Attachments({ msg }: { msg: MailMessage }) {
  const save = async (a: MailAttachment) => {
    try {
      downloadBlob(a.name || `part-${a.partId}`, await fetchBlob(messagePath(msg.id, `parts/${encodeURIComponent(a.partId)}`)));
    } catch (e) {
      notifyError(e, 'Download failed');
    }
  };
  if (!msg.attachments.length)
    return (
      <Text size="sm" c="dimmed">
        No attachments or inline parts.
      </Text>
    );
  return (
    <Paper withBorder radius="md" className="fm-table-card">
      <div className="fm-table-wrap">
        <Table fz="sm" verticalSpacing="xs" className="fm-table stack-table" data-testid="mail-attachments">
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Name</Table.Th>
              <Table.Th>Type</Table.Th>
              <Table.Th>Size</Table.Th>
              <Table.Th>Disposition</Table.Th>
              <Table.Th w={120}>
                <span className="sr-only">Download</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {msg.attachments.map((a) => (
              <Table.Tr key={a.partId}>
                <Table.Td>
                  <Group gap={6} wrap="nowrap">
                    <IconPaperclip size={15} aria-hidden />
                    <Text size="sm" className="break-all">
                      {a.name || `part ${a.partId}`}
                    </Text>
                  </Group>
                </Table.Td>
                <Table.Td data-label="Type" className="mono">
                  {a.contentType}
                </Table.Td>
                <Table.Td data-label="Size" className="tabular">
                  {formatBytes(a.size)}
                </Table.Td>
                <Table.Td data-label="Disposition">{a.inline ? <span className="fm-chip">inline{a.contentId ? ` cid:${a.contentId}` : ''}</span> : <span className="fm-chip">attachment</span>}</Table.Td>
                <Table.Td>
                  <Group justify="flex-end">
                    <Button size="compact-sm" variant="light" leftSection={<IconDownload size={14} />} onClick={() => void save(a)} aria-label={`Download ${a.name || `part ${a.partId}`}`}>
                      Download
                    </Button>
                  </Group>
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      </div>
    </Paper>
  );
}

function Transcript({ id, source }: { id: string; source: string }) {
  const q = useQuery({ queryKey: ['mail', 'transcript', id], queryFn: () => mailApi.transcript(id), staleTime: 60_000 });
  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} />;
  const lines = q.data?.transcript ?? [];
  if (!lines.length)
    return (
      <Text size="sm" c="dimmed">
        {source === 'smtp' ? 'No SMTP session was recorded for this message.' : 'This message did not come over SMTP, so there is no session to show.'}
      </Text>
    );
  const t0 = Date.parse(lines[0]?.at ?? '');
  return (
    <ol className={classes.transcript} aria-label="SMTP session" data-testid="mail-transcript">
      {lines.map((l, i) => {
        const dt = Date.parse(l.at) - t0;
        return (
          <li key={i} className={classes.line} data-dir={l.direction}>
            <span className={classes.lineAt}>+{Number.isFinite(dt) ? `${String(Math.max(0, Math.round(dt)))} ms` : ''}</span>
            <span className={classes.lineDir} aria-label={l.direction === 'client' ? 'client' : 'server'}>
              {l.direction === 'client' ? 'C:' : 'S:'}
            </span>
            <span className={classes.lineText}>{l.text}</span>
          </li>
        );
      })}
    </ol>
  );
}

function LinksAndCodes({ id }: { id: string }) {
  const [pattern, setPattern] = useState('');
  const [debounced] = useDebouncedValue(pattern.trim(), 400);
  const links = useQuery({ queryKey: ['mail', 'links', id], queryFn: () => mailApi.links(id), staleTime: 60_000 });
  const codes = useQuery({ queryKey: ['mail', 'codes', id, debounced], queryFn: () => mailApi.codes(id, debounced || undefined), retry: false });
  return (
    <div className="fm-two-col">
      <SectionCard title="Codes" icon={<IconHash size={16} />} description="One-time codes in the subject and text" testId="mail-codes">
        <Stack gap="sm">
          <TextInput
            label="Pattern"
            description="A regular expression; default 4 to 8 digits"
            placeholder={codes.data?.pattern ?? '\\b\\d{4,8}\\b'}
            value={pattern}
            onChange={(e) => setPattern(e.currentTarget.value)}
            classNames={{ input: 'mono' }}
            error={codes.isError ? (codes.error instanceof Error ? codes.error.message : 'Invalid pattern') : undefined}
            data-testid="code-pattern"
          />
          {codes.isLoading ? (
            <LoadingState />
          ) : codes.data?.codes.length ? (
            <Group gap="xs">
              {codes.data.codes.map((c) => (
                <span key={c} className={classes.code} data-testid="mail-code">
                  {c}
                  <Copy value={c} label={`Copy ${c}`} />
                </span>
              ))}
            </Group>
          ) : (
            !codes.isError && (
              <Text size="sm" c="dimmed">
                No codes match.
              </Text>
            )
          )}
        </Stack>
      </SectionCard>
      <SectionCard title="Links" icon={<IconLink size={16} />} description="http(s) links of the text and HTML bodies; FastMock never follows them" testId="mail-links">
        {links.isLoading ? (
          <LoadingState />
        ) : links.isError ? (
          <ErrorState error={links.error} />
        ) : !links.data?.links.length ? (
          <Text size="sm" c="dimmed">
            No links.
          </Text>
        ) : (
          <div>
            {links.data.links.map((l) => (
              <div key={l.url} className={classes.linkRow} data-testid="mail-link">
                <Stack gap={0} className="fm-grow" miw={0}>
                  {l.text && (
                    <Text size="sm" fw={600}>
                      {l.text}
                    </Text>
                  )}
                  <span className={classes.linkUrl}>{l.url}</span>
                </Stack>
                <Copy value={l.url} label="Copy link" />
              </div>
            ))}
          </div>
        )}
      </SectionCard>
    </div>
  );
}

function LabelsEditor({ msg }: { msg: MailMessage }) {
  const can = useCan();
  const editable = can('mail:write', msg.space ?? null);
  const [labels, setLabels] = useState(msg.labels);
  useEffect(() => setLabels(msg.labels), [msg.labels]);
  const save = useApiMutation<string[]>({
    fn: (l) => mailApi.labels(msg.id, l),
    invalidate: [['mail', 'message', msg.id], ['mail', 'list']],
    success: 'Labels saved',
  });
  if (!editable)
    return msg.labels.length ? (
      <Group gap={6}>
        {msg.labels.map((l) => (
          <LabelChip key={l} label={l} />
        ))}
      </Group>
    ) : (
      <Text size="sm" c="dimmed">
        none
      </Text>
    );
  const dirty = labels.join(',') !== msg.labels.join(',');
  const bad = labels.find((l) => !/^[A-Za-z0-9\-_.:/]{1,64}$/.test(l));
  return (
    <Group gap="xs" align="flex-start" wrap="nowrap">
      <TagsInput
        aria-label="Labels"
        placeholder="Add a label"
        value={labels}
        onChange={setLabels}
        splitChars={[',', ' ']}
        maxTags={32}
        acceptValueOnBlur
        size="xs"
        className="fm-grow"
        error={bad ? `Not a label: ${bad}` : undefined}
        data-testid="mail-labels-input"
      />
      {dirty && (
        <Button size="compact-sm" mt={2} disabled={!!bad} loading={save.isPending} onClick={() => save.mutate(labels.map((l) => l.toLowerCase()))} data-testid="mail-labels-save">
          Save
        </Button>
      )}
    </Group>
  );
}

function ProviderInfo({ msg }: { msg: MailMessage }) {
  const x = msg.providerExtras;
  if (!x) return null;
  const meta = x.metadata ?? {};
  return (
    <SectionCard title="Provider details" icon={<IconCode size={16} />} description={`What the ${msg.provider ?? 'provider'} API call carried`} testId="mail-provider">
      <Stack gap="sm">
        <DetailList
          items={[
            { label: 'Provider message id', value: <span className="mono break-all">{x.providerMessageId ?? '—'}</span> },
            { label: 'Template', value: <span className="mono">{x.templateId ?? '—'}</span> },
            { label: 'Test mode', value: x.testMode ? <ToneBadge tone="info">sandbox / test mode</ToneBadge> : 'no' },
            {
              label: 'Tags',
              value: x.tags?.length ? (
                <Group gap={4}>
                  {x.tags.map((t) => (
                    <span key={t} className="fm-chip">
                      {t}
                    </span>
                  ))}
                </Group>
              ) : (
                '—'
              ),
            },
          ]}
        />
        {x.templateData != null && (
          <div>
            <Text size="xs" fw={700} c="dimmed" mb={4}>
              Template data
            </Text>
            <div className="fm-cmd">
              <pre className="pre-flat">{pretty(x.templateData)}</pre>
            </div>
          </div>
        )}
        {Object.keys(meta).length > 0 && (
          <div>
            <Text size="xs" fw={700} c="dimmed" mb={4}>
              Metadata
            </Text>
            <KeyValueTable data={meta} />
          </div>
        )}
      </Stack>
    </SectionCard>
  );
}

function TestSnippet({ msg }: { msg: MailMessage }) {
  const code = waitSnippet(msg, window.location.origin);
  return (
    <Popover position="bottom-end" width={560} withArrow shadow="md">
      <Popover.Target>
        <Button variant="default" leftSection={<IconTestPipe size={16} />} data-testid="mail-test-snippet">
          Test snippet
        </Button>
      </Popover.Target>
      <Popover.Dropdown maw="calc(100vw - 32px)">
        <Stack gap="xs">
          <Text size="sm" fw={700}>
            Wait for a message like this one
          </Text>
          <Text size="xs" c="dimmed">
            Long-polls until a message from the same sender to the same recipient with this subject is stored. Set FASTMOCK_TOKEN to a token with mail:read.
          </Text>
          <Snippet code={code} label="Copy as test snippet" testId="mail-wait-snippet" />
        </Stack>
      </Popover.Dropdown>
    </Popover>
  );
}

export function MessageView({ id }: { id: string }) {
  const navigate = useNavigate();
  const can = useCan();
  const spaces = useSpace();
  const [params, setParams] = useSearchParams();
  const q = useQuery({ queryKey: ['mail', 'message', id], queryFn: () => mailApi.get(id), retry: false });
  const msg = q.data;

  useEffect(() => {
    if (msg) markSeen(loadSeen(), msg.id);
  }, [msg]);

  const del = useApiMutation<void>({
    fn: () => mailApi.remove(id),
    invalidate: [['mail', 'list']],
    success: 'Message deleted',
    onSuccess: () => void navigate('/mail'),
  });

  if (q.isLoading) return <LoadingState />;
  if (q.isError || !msg)
    return isNotFound(q.error) ? (
      <EmptyState
        title="Message not found"
        icon={<IconMailOpened size={28} />}
        action={
          <Button component={Link} to="/mail" variant="light" leftSection={<IconArrowLeft size={16} />}>
            Back to mail
          </Button>
        }
        testId="mail-not-found"
      >
        It was deleted, removed by retention, or is in a space you cannot read.
      </EmptyState>
    ) : (
      <ErrorState error={q.error} onRetry={() => void q.refetch()} />
    );

  const subject = msg.subject ?? msg.summary.subject ?? '(no subject)';
  const tabs = [
    ...(msg.hasHtml ? [{ value: 'html', label: 'HTML', icon: IconCode }] : []),
    { value: 'text', label: 'Text', icon: IconFileText },
    { value: 'headers', label: 'Headers', icon: IconList, count: msg.headers.length },
    { value: 'raw', label: 'Raw', icon: IconTerminal2 },
    { value: 'attachments', label: 'Attachments', icon: IconPaperclip, count: msg.attachments.length },
    { value: 'transcript', label: 'Transcript', icon: IconTerminal2 },
    { value: 'links', label: 'Links & codes', icon: IconLink },
  ];
  const wanted = params.get('tab');
  const tab = tabs.some((t) => t.value === wanted) ? (wanted as string) : (tabs[0]?.value ?? 'text');
  const canDelete = can('mail:delete', msg.space ?? null);
  const env = msg.envelope;

  return (
    <Stack gap="md">
      <PageHeader
        title={subject}
        breadcrumbs={[{ label: 'Mail', to: '/mail' }, { label: subject }]}
        icon={<IconMailOpened size={22} stroke={1.6} />}
        meta={
          <Group gap={6} wrap="wrap">
            <SourceBadge entry={msg} />
            {spaces.enabled && <SpaceBadge space={msg.space} />}
            {msg.unparsed && <ToneBadge tone="warning">unparsed: raw only</ToneBadge>}
          </Group>
        }
        actions={
          <>
            <TestSnippet msg={msg} />
            <Button variant="default" leftSection={<IconDownload size={16} />} onClick={() => void downloadRaw(msg.id)}>
              .eml
            </Button>
            {canDelete ? (
              <Button
                variant="light"
                color="red"
                leftSection={<IconTrash size={16} />}
                data-testid="mail-delete"
                onClick={() =>
                  confirmAction({
                    title: 'Delete this message?',
                    message: 'It is removed for everyone, also from the IMAP and POP3 mailboxes of its recipients.',
                    confirm: 'Delete',
                    onConfirm: () => del.mutate(),
                  })
                }
              >
                Delete
              </Button>
            ) : (
              <Denied perm="mail:delete" why={missingText('mail:delete', msg.space ?? null)}>
                <Button variant="light" color="red" leftSection={<IconTrash size={16} />}>
                  Delete
                </Button>
              </Denied>
            )}
          </>
        }
      />
      <div className="fm-aside-col">
        <Paper withBorder radius="md" p="md" data-testid="mail-header-block">
          <DetailList
            items={[
              { label: 'From', value: <Addresses list={msg.from} /> },
              { label: 'To', value: <Addresses list={msg.to} /> },
              { label: 'Cc', value: <Addresses list={msg.cc} />, hidden: !msg.cc.length },
              { label: 'Bcc', value: <Addresses list={msg.bcc} />, hidden: !msg.bcc.length },
              { label: 'Reply-To', value: <Addresses list={msg.replyTo} />, hidden: !msg.replyTo.length },
              { label: 'Date', value: msg.date ? formatAbsolute(msg.date) : '—' },
              { label: 'Received', value: formatAbsolute(msg.receivedAt) },
              { label: 'Size', value: <span className="tabular">{formatBytes(msg.summary.size)}</span> },
              {
                label: 'Message-ID',
                value: msg.messageId ? (
                  <Group gap={2} wrap="nowrap">
                    <span className="mono break-all">{msg.messageId}</span>
                    <Copy value={msg.messageId} label="Copy Message-ID" />
                  </Group>
                ) : (
                  '—'
                ),
              },
              { label: 'Labels', value: <LabelsEditor msg={msg} /> },
            ]}
          />
        </Paper>
        <Paper withBorder radius="md" p="md" className={classes.envelope} data-testid="mail-envelope">
          <Text className="fm-subhead" mb="xs">
            Envelope
          </Text>
          <DetailList
            columns={1}
            items={[
              { label: 'MAIL FROM', value: <span className="mono break-all">{env.mailFrom || '<>'}</span> },
              { label: 'RCPT TO', value: <span className="mono break-all">{env.rcptTo.join(', ') || '—'}</span> },
              { label: 'HELO', value: <span className="mono">{env.helo}</span>, hidden: !env.helo },
              { label: 'Client IP', value: <span className="mono">{env.clientIp}</span>, hidden: !env.clientIp },
              { label: 'TLS', value: env.tls ? <ToneBadge tone="success">TLS</ToneBadge> : <ToneBadge tone="neutral">plain</ToneBadge> },
              { label: 'AUTH user', value: <span className="mono">{env.authUser}</span>, hidden: !env.authUser },
            ]}
          />
        </Paper>
      </div>
      <ProviderInfo msg={msg} />
      {msg.unparsed && (
        <Alert color="yellow" icon={<IconAlertTriangle size={18} />}>
          FastMock could not parse this message as MIME (too large, or no header block). Only the raw form is available.
        </Alert>
      )}
      <Tabs
        value={tab}
        onChange={(v) => {
          const next = new URLSearchParams(params);
          if (v) next.set('tab', v);
          setParams(next, { replace: true });
        }}
        variant="pills"
        radius="md"
        keepMounted={false}
        classNames={{ list: 'fm-tabs-list' }}
      >
        <Tabs.List>
          {tabs.map((t) => (
            <Tabs.Tab key={t.value} value={t.value} leftSection={<t.icon size={16} />} data-testid={`mail-tab-${t.value}`}>
              {t.label}
              {t.count !== undefined && <span className="fm-tab-count">{t.count}</span>}
            </Tabs.Tab>
          ))}
        </Tabs.List>
        <Tabs.Panel value="html" pt="md">
          <HtmlBody id={msg.id} />
        </Tabs.Panel>
        <Tabs.Panel value="text" pt="md">
          <TextBody msg={msg} />
        </Tabs.Panel>
        <Tabs.Panel value="headers" pt="md">
          <HeadersTable msg={msg} />
        </Tabs.Panel>
        <Tabs.Panel value="raw" pt="md">
          <RawView id={msg.id} size={msg.summary.size} />
        </Tabs.Panel>
        <Tabs.Panel value="attachments" pt="md">
          <Attachments msg={msg} />
        </Tabs.Panel>
        <Tabs.Panel value="transcript" pt="md">
          <Transcript id={msg.id} source={msg.source} />
        </Tabs.Panel>
        <Tabs.Panel value="links" pt="md">
          <LinksAndCodes id={msg.id} />
        </Tabs.Panel>
      </Tabs>
      <Group>
        <Anchor component={Link} to="/mail" size="sm">
          <Group gap={4}>
            <IconArrowLeft size={14} /> All mail
          </Group>
        </Anchor>
      </Group>
    </Stack>
  );
}

