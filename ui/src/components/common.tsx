import { ActionIcon, Badge, CopyButton, Group, Stack, Table, Text, Title, Tooltip } from '@mantine/core';
import { IconCheck, IconCopy } from '@tabler/icons-react';
import type { ReactNode } from 'react';
import { formatAbsolute, formatRelative, prettyBody } from '../lib/format';

export function PageHeader({ title, description, actions }: { title: string; description?: ReactNode; actions?: ReactNode }) {
  return (
    <Group justify="space-between" align="flex-start" mb="md" wrap="wrap" gap="sm">
      <Stack gap={2}>
        <Title order={2} size="h3">
          {title}
        </Title>
        {description && (
          <Text c="dimmed" size="sm" component="div">
            {description}
          </Text>
        )}
      </Stack>
      {actions && (
        <Group gap="xs" wrap="wrap">
          {actions}
        </Group>
      )}
    </Group>
  );
}

const HTTP_METHODS = new Set(['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS']);

export function MethodBadge({ method }: { method?: string | null }) {
  const m = (method ?? 'ANY').toUpperCase();
  const known = HTTP_METHODS.has(m);
  return (
    <Badge variant="light" className={known ? 'fm-http' : undefined} data-method={known ? m.toLowerCase() : undefined} radius="sm" w={72} styles={{ label: { overflow: 'visible' } }}>
      {m}
    </Badge>
  );
}

export function StatusBadge({ status }: { status: number }) {
  const color = status >= 500 ? 'red' : status >= 400 ? 'orange' : status >= 300 ? 'cyan' : status >= 200 ? 'green' : 'gray';
  return (
    <Badge variant="light" color={color} radius="sm">
      {status}
    </Badge>
  );
}

export function Copy({ value, label = 'Copy' }: { value: string; label?: string }) {
  return (
    <CopyButton value={value} timeout={1500}>
      {({ copied, copy }) => (
        <Tooltip label={copied ? 'Copied' : label} withArrow>
          <ActionIcon variant="subtle" color={copied ? 'teal' : 'gray'} onClick={copy} aria-label={label}>
            {copied ? <IconCheck size={16} /> : <IconCopy size={16} />}
          </ActionIcon>
        </Tooltip>
      )}
    </CopyButton>
  );
}

export function Time({ at }: { at: string | null | undefined }) {
  if (!at) return <Text span c="dimmed">—</Text>;
  return (
    <Tooltip label={formatAbsolute(at)} withArrow>
      <Text span size="sm">
        {formatRelative(at)}
      </Text>
    </Tooltip>
  );
}

export function KeyValueTable({ data, empty = 'none' }: { data: Record<string, unknown> | null | undefined; empty?: string }) {
  const entries = Object.entries(data ?? {});
  if (!entries.length)
    return (
      <Text size="sm" c="dimmed">
        {empty}
      </Text>
    );
  return (
    <Table withTableBorder withColumnBorders fz="sm" verticalSpacing={4}>
      <Table.Tbody>
        {entries.map(([k, v]) => (
          <Table.Tr key={k}>
            <Table.Td w="35%" className="mono">
              {k}
            </Table.Td>
            <Table.Td className="mono" style={{ wordBreak: 'break-all' }}>
              {typeof v === 'string' ? v : JSON.stringify(v)}
            </Table.Td>
          </Table.Tr>
        ))}
      </Table.Tbody>
    </Table>
  );
}

export function BodyView({ body, contentType, empty = 'empty body' }: { body: string | null | undefined; contentType?: string | undefined; empty?: string }) {
  if (!body)
    return (
      <Text size="sm" c="dimmed">
        {empty}
      </Text>
    );
  const pretty = prettyBody(body, contentType);
  return (
    <div style={{ position: 'relative' }}>
      <pre className="pre">{pretty}</pre>
      <div style={{ position: 'absolute', top: 4, right: 4 }}>
        <Copy value={body} label="Copy body" />
      </div>
    </div>
  );
}
