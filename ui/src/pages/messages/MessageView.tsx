import { Alert, Anchor, Button, Group, Paper, Stack, Text } from '@mantine/core';
import { IconAlertTriangle, IconArrowLeft, IconDownload, IconFileText, IconInbox, IconList, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { Link, useNavigate } from 'react-router';
import { capturedPath, captureApi, type MessageDetail } from '../../api/capture';
import { isNotFound } from '../../api/http';
import { fetchBlob } from '../../api/mail';
import { confirmAction, notifyError, useApiMutation } from '../../api/mutations';
import { missingText } from '../../auth/permissions';
import { useCan, useSpace } from '../../auth/useAuth';
import { CodeEditor } from '../../components/CodeEditor';
import { Copy, DetailList, KeyValueTable, PageHeader, SectionCard, ToneBadge } from '../../components/common';
import { Denied } from '../../components/Permitted';
import { SpaceBadge } from '../../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { bodyRendering, destinationOf, protocolLabel } from '../../lib/capture';
import { downloadBlob } from '../../lib/download';
import { formatAbsolute, formatBytes } from '../../lib/format';
import { ProtocolBadge } from './parts';

const BASE64_PREVIEW = 16 * 1024;

function bodyFileName(msg: MessageDetail): string {
  const ext = msg.body.encoding === 'json' ? 'json' : msg.body.encoding === 'text' ? 'txt' : 'bin';
  return `${msg.id}.${ext}`;
}

async function downloadBody(msg: MessageDetail) {
  try {
    downloadBlob(bodyFileName(msg), await fetchBlob(capturedPath(msg.id, 'body')));
  } catch (e) {
    notifyError(e, 'Download failed');
  }
}

function Body({ msg }: { msg: MessageDetail }) {
  const r = bodyRendering(msg.body);
  const download = (
    <Button size="compact-sm" variant="light" leftSection={<IconDownload size={14} />} onClick={() => void downloadBody(msg)} data-testid="message-download">
      Download
    </Button>
  );
  const encoding =
    r.kind === 'json' && r.schemaId !== undefined ? (
      <ToneBadge tone="info" testId="body-encoding">
        Avro{r.schemaId != null ? ` · schema ${String(r.schemaId)}` : ''}
      </ToneBadge>
    ) : (
      <ToneBadge tone="neutral" testId="body-encoding">
        {msg.body.encoding}
      </ToneBadge>
    );
  return (
    <SectionCard
      title="Body"
      icon={<IconFileText size={16} />}
      description={
        <Group gap={6} wrap="wrap" mt={2}>
          {encoding}
          <span>{`${formatBytes(msg.size)}${msg.contentType ? ` · ${msg.contentType}` : ''}${msg.truncated ? ' · stored truncated' : ''}`}</span>
        </Group>
      }
      actions={r.kind !== 'empty' ? download : undefined}
      testId="message-body"
    >
      {msg.truncated && (
        <Alert color="yellow" icon={<IconAlertTriangle size={18} />} p="xs" mb="sm">
          The message was larger than FastMock stores; the body is cut off.
        </Alert>
      )}
      {r.kind === 'empty' ? (
        <Text size="sm" c="dimmed">
          Empty body
        </Text>
      ) : r.kind === 'json' ? (
        <CodeEditor value={r.text} readOnly minHeight={120} maxHeight={640} label="Message body" testId="body-json" />
      ) : r.kind === 'text' ? (
        <CodeEditor value={r.text} language="text" readOnly minHeight={80} maxHeight={640} label="Message body" testId="body-text" />
      ) : (
        <Stack gap="xs">
          {r.error && (
            <Alert color="yellow" icon={<IconAlertTriangle size={18} />} p="xs" data-testid="body-error">
              {r.error}
            </Alert>
          )}
          <Text size="xs" c="dimmed">
            Binary data, shown as base64{r.text.length > BASE64_PREVIEW ? `; the preview shows the first ${formatBytes(BASE64_PREVIEW)}` : ''}. Download it for the raw bytes.
          </Text>
          <pre className="pre break-all" data-testid="body-base64">
            {r.text.slice(0, BASE64_PREVIEW)}
          </pre>
        </Stack>
      )}
    </SectionCard>
  );
}

export function MessageView({ id }: { id: string }) {
  const can = useCan();
  const spaces = useSpace();
  const navigate = useNavigate();
  const q = useQuery({ queryKey: ['capture', 'message', id], queryFn: () => captureApi.get(id), staleTime: 60_000 });
  const msg = q.data;
  const del = useApiMutation({
    fn: () => captureApi.remove(id),
    invalidate: [['capture', 'list'], ['capture', 'find'], ['capture', 'sources']],
    success: 'Message deleted',
    onSuccess: () => void navigate('/messages'),
  });

  if (q.isLoading) return <LoadingState />;
  if (q.isError || !msg)
    return isNotFound(q.error) ? (
      <EmptyState
        title="Message not found"
        icon={<IconInbox size={28} />}
        action={
          <Button component={Link} to="/messages" variant="light" leftSection={<IconArrowLeft size={16} />}>
            Back to messages
          </Button>
        }
        testId="message-not-found"
      >
        It was deleted, dropped by the source's message limit, or is in a space you cannot read.
      </EmptyState>
    ) : (
      <ErrorState error={q.error} onRetry={() => void q.refetch()} />
    );

  const title = destinationOf(msg);
  const amqp = msg.protocol === 'amqp';
  const canDelete = can('messages:delete', msg.space ?? null);
  const deleteButton = (
    <Button variant="light" color="red" leftSection={<IconTrash size={16} />} data-testid="message-delete" onClick={() => confirmAction({ title: 'Delete this message?', message: 'It is removed from the stored messages for everyone.', confirm: 'Delete', onConfirm: () => del.mutate() })}>
      Delete
    </Button>
  );

  return (
    <Stack gap="md">
      <PageHeader
        title={title}
        breadcrumbs={[{ label: 'Messages', to: '/messages' }, { label: title }]}
        icon={<IconInbox size={22} stroke={1.6} />}
        meta={
          <Group gap={6} wrap="wrap">
            <ProtocolBadge protocol={msg.protocol} />
            <span className="fm-chip">{msg.source}</span>
            {spaces.enabled && <SpaceBadge space={msg.space} />}
            {msg.truncated && <ToneBadge tone="warning">truncated</ToneBadge>}
          </Group>
        }
        actions={
          canDelete ? (
            deleteButton
          ) : (
            <Denied perm="messages:delete" why={missingText('messages:delete', msg.space ?? null)}>
              {deleteButton}
            </Denied>
          )
        }
      />
      <Paper withBorder radius="md" p="md" data-testid="message-meta">
        <DetailList
          items={[
            { label: 'Source', value: <Anchor component={Link} to={`/messages?source=${encodeURIComponent(msg.source)}`} size="sm">{msg.source}</Anchor> },
            { label: 'Protocol', value: protocolLabel(msg.protocol) },
            { label: 'Exchange', value: <span className="mono break-all">{msg.exchange || '(default)'}</span>, hidden: !amqp },
            { label: amqp ? 'Routing key' : 'Topic', value: <span className="mono break-all">{msg.topic || '—'}</span> },
            { label: 'Key', value: <span className="mono break-all">{msg.key}</span>, hidden: msg.key == null },
            { label: 'Content type', value: <span className="mono">{msg.contentType}</span>, hidden: !msg.contentType },
            { label: 'Size', value: <span className="tabular">{formatBytes(msg.size)}{msg.truncated ? ' (truncated)' : ''}</span> },
            { label: 'Received', value: formatAbsolute(msg.receivedAt) },
            {
              label: 'Message id',
              value: (
                <Group gap={2} wrap="nowrap">
                  <span className="mono break-all">{msg.id}</span>
                  <Copy value={msg.id} label="Copy message id" />
                </Group>
              ),
            },
          ]}
        />
      </Paper>
      <div className="fm-aside-col">
        <SectionCard title="Headers" icon={<IconList size={16} />} description={amqp ? 'AMQP headers' : msg.protocol === 'mqtt' ? 'MQTT 5 user properties' : 'Kafka record headers'} testId="message-headers">
          <KeyValueTable data={msg.headers} empty="No headers" />
        </SectionCard>
        <SectionCard title="Properties" icon={<IconList size={16} />} description={`What the ${protocolLabel(msg.protocol)} broker reported`} testId="message-properties">
          <KeyValueTable data={msg.properties} empty="No properties" />
        </SectionCard>
      </div>
      <Body msg={msg} />
      <Group>
        <Anchor component={Link} to="/messages" size="sm">
          <Group gap={4}>
            <IconArrowLeft size={14} /> All messages
          </Group>
        </Anchor>
      </Group>
    </Stack>
  );
}
