import {
  ActionIcon,
  Alert,
  Badge,
  Button,
  Code,
  CopyButton,
  Group,
  Modal,
  MultiSelect,
  NumberInput,
  Paper,
  SegmentedControl,
  Select,
  Stack,
  Table,
  Text,
  TextInput,
  Tooltip,
} from '@mantine/core';
import { IconAlertTriangle, IconBan, IconCheck, IconCopy, IconKey, IconPlus } from '@tabler/icons-react';
import { keepPreviousData, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { errorMessage } from '../api/http';
import { confirmAction, notifySuccess, useApiMutation } from '../api/mutations';
import type { components } from '../api/schema';
import { can as allowed } from '../auth/permissions';
import { useAuth, useCan, useSpace, useSpaceQuery } from '../auth/useAuth';
import { PageHeader, Time } from '../components/common';
import { Permitted } from '../components/Permitted';
import { SpaceBadge } from '../components/spaces';
import { EmptyState, ErrorState, LoadingState, TokensGate } from '../components/States';

type TokenInfo = components['schemas']['TokenInfo'];
type TokenOptions = components['schemas']['TokenOptions'];
type GrantableRole = components['schemas']['GrantableRole'];

const STATUS_COLOR: Record<string, string> = { active: 'green', expired: 'gray', revoked: 'red' };
const NAME_RE = /^[A-Za-z0-9._@-]{1,128}$/;

function RoleBadge({ role, options }: { role: string; options: TokenOptions | undefined }) {
  const def = options?.roles.find((r) => r.name === role);
  const badge = (
    <Badge variant="light" color={def?.builtin === false ? 'teal' : undefined} tt="none">
      {role}
    </Badge>
  );
  if (!def) return badge;
  return (
    <Tooltip label={def.permissions.join(', ')} multiline w={320} withArrow>
      {badge}
    </Tooltip>
  );
}

/** Scopes the role may be granted in, as offered to the caller. */
function scopesOf(role: GrantableRole | undefined, spacesEnabled: boolean): { global: boolean; spaces: string[] } {
  if (!role) return { global: false, spaces: [] };
  return { global: role.global, spaces: spacesEnabled ? role.spaces : [] };
}

function CreateTokenModal({
  opened,
  onClose,
  onCreated,
  options,
}: {
  opened: boolean;
  onClose: () => void;
  onCreated: (t: components['schemas']['CreatedToken']) => void;
  options: TokenOptions | undefined;
}) {
  const spacesCtx = useSpace();
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [role, setRole] = useState<string | null>(null);
  const [scope, setScope] = useState<'global' | 'spaces'>('spaces');
  const [spaces, setSpaces] = useState<string[]>([]);
  const [days, setDays] = useState<number | ''>('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const roles = (options?.roles ?? []).filter((r) => r.global || (spacesCtx.enabled && r.spaces.length > 0));
  const def = roles.find((r) => r.name === role);
  const offered = scopesOf(def, spacesCtx.enabled);
  const effectiveScope: 'global' | 'spaces' = !offered.spaces.length ? 'global' : !offered.global ? 'spaces' : scope;
  const max = options?.maxDays ?? 365;
  const lifetime = days === '' ? (options?.defaultDays ?? Math.min(90, max)) : days;
  const nameOk = NAME_RE.test(name.trim());
  const scopeOk = effectiveScope === 'global' ? offered.global : spaces.length > 0 && spaces.every((s) => offered.spaces.includes(s));
  const valid = nameOk && !!def && scopeOk && lifetime >= 1 && lifetime <= max;

  const reset = () => {
    setName('');
    setDescription('');
    setRole(null);
    setScope('spaces');
    setSpaces([]);
    setDays('');
    setError(null);
  };
  const close = () => {
    reset();
    onClose();
  };
  const pickRole = (r: string | null) => {
    setRole(r);
    const next = roles.find((x) => x.name === r);
    const current = spacesCtx.current;
    setSpaces(next && current && next.spaces.includes(current) ? [current] : []);
  };

  const submit = async () => {
    if (!valid) return;
    setBusy(true);
    setError(null);
    try {
      const created = await unwrap(
        client.POST('/__admin/tokens', {
          body: {
            name: name.trim(),
            role: def.name,
            ...(effectiveScope === 'global' ? { global: true } : { spaces }),
            expiresInDays: lifetime,
            ...(description.trim() ? { description: description.trim() } : {}),
          },
        }),
      );
      reset();
      onCreated(created);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal opened={opened} onClose={close} title="Create a managed token" size="lg">
      <Stack gap="sm">
        <TextInput
          label="Name"
          description="Letters, digits and . _ @ -; unique among active tokens"
          placeholder="ci-payments"
          value={name}
          onChange={(e) => setName(e.currentTarget.value)}
          error={name && !nameOk ? 'Use 1 to 128 characters of A-Z a-z 0-9 . _ @ -' : null}
          required
          data-testid="token-name"
        />
        <TextInput label="Description" placeholder="Pipeline of the payments team" value={description} onChange={(e) => setDescription(e.currentTarget.value)} maxLength={1024} />
        <Select
          label="Role"
          description="Only roles whose permissions you hold where you grant them"
          data={roles.map((r) => ({ value: r.name, label: r.builtin ? r.name : `${r.name} (custom)` }))}
          value={role}
          onChange={pickRole}
          placeholder={roles.length ? 'Pick a role' : 'You cannot grant any role'}
          required
          allowDeselect={false}
          data-testid="token-role"
          comboboxProps={{ withinPortal: true }}
        />
        {def && (
          <Group gap={4} data-testid="token-role-permissions">
            {def.permissions.map((p) => (
              <Badge key={p} size="sm" variant="outline" color="gray" tt="none">
                {p}
              </Badge>
            ))}
          </Group>
        )}
        {def && offered.global && offered.spaces.length > 0 && (
          <SegmentedControl
            value={effectiveScope}
            onChange={setScope}
            data={[
              { label: 'In spaces', value: 'spaces' },
              { label: 'Global', value: 'global' },
            ]}
            aria-label="Scope"
            w="fit-content"
            data-testid="token-scope"
          />
        )}
        {def && effectiveScope === 'spaces' && (
          <MultiSelect
            label="Spaces"
            description="The token can act only in these spaces"
            data={offered.spaces}
            value={spaces}
            onChange={setSpaces}
            required
            searchable
            data-testid="token-spaces"
            comboboxProps={{ withinPortal: true }}
          />
        )}
        {def && effectiveScope === 'global' && spacesCtx.enabled && (
          <Text size="xs" c="dimmed">
            A global token acts in every space and in the global area.
          </Text>
        )}
        <NumberInput
          label="Lifetime (days)"
          description={`1 to ${max} days (server maximum); ${options?.defaultDays ?? 90} when left empty`}
          value={days}
          onChange={(v) => setDays(typeof v === 'number' ? v : '')}
          min={1}
          max={max}
          clampBehavior="strict"
          allowDecimal={false}
          placeholder={String(options?.defaultDays ?? 90)}
          w={{ base: '100%', sm: 260 }}
          data-testid="token-days"
        />
        {error && (
          <Alert color="red" role="alert">
            {error}
          </Alert>
        )}
        <Group justify="flex-end">
          <Button variant="default" onClick={close}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={!valid} loading={busy} data-testid="token-create-submit">
            Create token
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}

/** The new token's value, shown once. It lives only in this component's state and is dropped on close. */
function ShowOnceModal({ created, onClose }: { created: { name: string; token: string } | null; onClose: () => void }) {
  return (
    <Modal opened={!!created} onClose={onClose} title={created ? `Token ${created.name} created` : ''} size="lg" closeOnClickOutside={false}>
      {created && (
        <Stack gap="md">
          <Alert color="orange" variant="light" icon={<IconAlertTriangle size={18} />} title="Copy the token now" data-testid="show-once-warning">
            This is the only time FastMock shows this token. Store it in a secret manager or CI variable before you close
            this dialog; if it is lost, revoke it and create a new one. Anyone who has it can act with its permissions
            until it expires.
          </Alert>
          <Group gap="xs" wrap="nowrap" align="center">
            <Code block className="token-value" data-testid="token-value">
              {created.token}
            </Code>
            <CopyButton value={created.token} timeout={2000}>
              {({ copied, copy }) => (
                <Tooltip label={copied ? 'Copied' : 'Copy token'} withArrow>
                  <ActionIcon variant={copied ? 'filled' : 'light'} color={copied ? 'teal' : undefined} size="lg" onClick={copy} aria-label="Copy token" data-testid="token-copy">
                    {copied ? <IconCheck size={18} /> : <IconCopy size={18} />}
                  </ActionIcon>
                </Tooltip>
              )}
            </CopyButton>
          </Group>
          <Group justify="flex-end">
            <Button onClick={onClose} data-testid="token-done">
              I have stored the token
            </Button>
          </Group>
        </Stack>
      )}
    </Modal>
  );
}

function TokensInner() {
  const { permissions } = useAuth();
  const can = useCan();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const qc = useQueryClient();
  const [status, setStatus] = useState('active');
  const [createOpen, setCreateOpen] = useState(false);
  const [created, setCreated] = useState<{ name: string; token: string } | null>(null);
  const canRead = allowed(permissions, 'tokens:read');
  const canWriteSomewhere = allowed(permissions, 'tokens:write');
  const query = { ...(status === 'all' ? {} : { status }), ...spaceQ };
  const list = useQuery({
    queryKey: ['tokens', query],
    queryFn: () => unwrap(client.GET('/__admin/tokens', { params: { query } })),
    enabled: canRead,
    placeholderData: keepPreviousData,
  });
  const options = useQuery({
    queryKey: ['tokens', 'options'],
    queryFn: () => unwrap(client.GET('/__admin/tokens/options')),
    enabled: canWriteSomewhere,
    staleTime: 60_000,
  });
  const revoke = useApiMutation<TokenInfo>({
    fn: (t) => unwrap(client.DELETE('/__admin/tokens/{id}', { params: { path: { id: t.id } } })),
    invalidate: [['tokens']],
    success: (_d, t) => `Revoked ${t.name}`,
  });

  if (!canRead) return <ErrorState error={new Error('Managing tokens needs the tokens:read permission.')} />;

  const revocable = (t: TokenInfo) => (t.global ? can('tokens:write', null) : t.spaces.every((s) => can('tokens:write', s)));
  const grantable = (options.data?.roles ?? []).some((r) => r.global || r.spaces.length > 0);

  return (
    <Stack gap="md">
      <PageHeader
        title="Admin tokens"
        description="Managed tokens for the admin API, with expiry, last use and revocation. A token's value is shown only once, when it is created."
        actions={
          <Permitted perm="tokens:write" anywhere>
            <Button leftSection={<IconPlus size={16} />} onClick={() => setCreateOpen(true)} disabled={canWriteSomewhere && options.isSuccess && !grantable} data-testid="create-token">
              Create token
            </Button>
          </Permitted>
        }
      />
      <Group gap="sm" wrap="wrap">
        <SegmentedControl
          value={status}
          onChange={setStatus}
          data={[
            { label: 'Active', value: 'active' },
            { label: 'Expired', value: 'expired' },
            { label: 'Revoked', value: 'revoked' },
            { label: 'All', value: 'all' },
          ]}
          aria-label="Token status"
        />
      </Group>
      {list.isLoading ? (
        <LoadingState />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : !list.data?.tokens.length ? (
        <EmptyState title={status === 'active' ? 'No active tokens' : 'No tokens'} icon={<IconKey size={26} />}>
          Tokens created here appear in this list{spaces.current ? ` when they are granted in space ${spaces.current}` : ''}. Static tokens from
          FASTMOCK_ADMIN_TOKENS are not listed.
        </EmptyState>
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm" highlightOnHover miw={1040} data-testid="tokens-table">
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Name</Table.Th>
                  <Table.Th>Role</Table.Th>
                  <Table.Th>Scope</Table.Th>
                  <Table.Th>Created by</Table.Th>
                  <Table.Th>Created</Table.Th>
                  <Table.Th>Expires</Table.Th>
                  <Table.Th>Last used</Table.Th>
                  <Table.Th>Status</Table.Th>
                  <Table.Th w={50}>
                    <span className="sr-only">Actions</span>
                  </Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {list.data.tokens.map((t) => (
                  <Table.Tr key={t.id} data-testid="token-row">
                    <Table.Td maw={260}>
                      <Text size="sm" fw={500} className="truncate">
                        {t.name}
                      </Text>
                      {t.description && (
                        <Text size="xs" c="dimmed" lineClamp={2}>
                          {t.description}
                        </Text>
                      )}
                    </Table.Td>
                    <Table.Td>
                      <RoleBadge role={t.role} options={options.data} />
                    </Table.Td>
                    <Table.Td>
                      {t.global ? (
                        <Badge variant="outline" color="grape" tt="none">
                          global
                        </Badge>
                      ) : (
                        <Group gap={4}>
                          {t.spaces.map((s) => (
                            <SpaceBadge key={s} space={s} />
                          ))}
                        </Group>
                      )}
                    </Table.Td>
                    <Table.Td className="truncate" maw={160}>
                      {t.createdBy}
                    </Table.Td>
                    <Table.Td>
                      <Time at={t.createdAt} />
                    </Table.Td>
                    <Table.Td>
                      <Time at={t.expiresAt} />
                    </Table.Td>
                    <Table.Td>
                      <Time at={t.lastUsedAt} />
                    </Table.Td>
                    <Table.Td>
                      <Badge color={STATUS_COLOR[t.status] ?? 'gray'} variant="light">
                        {t.status}
                      </Badge>
                    </Table.Td>
                    <Table.Td>
                      {t.status === 'active' && (
                        <Permitted perm="tokens:write" where={t.global ? null : t.spaces.find((s) => !can('tokens:write', s)) ?? t.spaces[0]}>
                          <Tooltip label={`Revoke ${t.name}`} withArrow disabled={!revocable(t)}>
                            <ActionIcon
                              variant="subtle"
                              color="red"
                              aria-label={`Revoke ${t.name}`}
                              onClick={() =>
                                confirmAction({
                                  title: `Revoke token ${t.name}?`,
                                  message: 'Every replica rejects the token within 10 seconds. Revoking cannot be undone; create a new token instead.',
                                  confirm: 'Revoke',
                                  onConfirm: () => revoke.mutate(t),
                                })
                              }
                              data-testid="revoke-token"
                            >
                              <IconBan size={16} />
                            </ActionIcon>
                          </Tooltip>
                        </Permitted>
                      )}
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
      <CreateTokenModal
        opened={createOpen}
        onClose={() => setCreateOpen(false)}
        options={options.data}
        onCreated={(t) => {
          setCreateOpen(false);
          setCreated({ name: t.name, token: t.token });
          notifySuccess(`Created token ${t.name}`);
          void qc.invalidateQueries({ queryKey: ['tokens'] });
        }}
      />
      <ShowOnceModal created={created} onClose={() => setCreated(null)} />
    </Stack>
  );
}

export default function Tokens() {
  return (
    <TokensGate>
      <TokensInner />
    </TokensGate>
  );
}
