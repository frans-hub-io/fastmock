import { Alert, Badge, Code, Group, Paper, Stack, Table, Text, Title, Tooltip } from '@mantine/core';
import { IconInfoCircle } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { client, unwrap } from '../api/client';
import type { components } from '../api/schema';
import { boundSpaces, can, effectiveInSpace, kindLabel, seesAllSpaces, type Permission } from '../auth/permissions';
import { useAuth } from '../auth/useAuth';
import { PageHeader } from '../components/common';
import { ScopeBadges, SpaceBadge } from '../components/spaces';
import { ErrorState, LoadingState } from '../components/States';

type AccessView = components['schemas']['AccessView'];

function PermissionBadges({ list, testId }: { list: readonly Permission[]; testId?: string }) {
  if (!list.length)
    return (
      <Text size="sm" c="dimmed" data-testid={testId}>
        none
      </Text>
    );
  return (
    <Group gap={4} data-testid={testId}>
      {list.map((p) => (
        <Badge key={p} size="sm" variant="outline" color="gray" tt="none">
          {p}
        </Badge>
      ))}
    </Group>
  );
}

function Section({ title, description, children, testId }: { title: string; description?: ReactNode; children: ReactNode; testId?: string }) {
  return (
    <Paper withBorder p="md" radius="md" data-testid={testId}>
      <Stack gap="sm">
        <Stack gap={2}>
          <Title order={3} size="h5">
            {title}
          </Title>
          {description && (
            <Text size="sm" c="dimmed" component="div">
              {description}
            </Text>
          )}
        </Stack>
        {children}
      </Stack>
    </Paper>
  );
}

/**
 * A read-only table; below the `sm` breakpoint each row is stacked as a card (cells labelled by `data-label`),
 * so phones need no sideways scrolling.
 */
function TableBox({ children, testId, miw }: { children: ReactNode; testId: string; miw: number }) {
  return (
    <Paper withBorder radius="md">
      <div className="table-scroll">
        <Table fz="sm" miw={miw} className="stack-table" data-testid={testId}>
          {children}
        </Table>
      </div>
    </Paper>
  );
}

function YourAccess() {
  const { name, kind, info, permissions } = useAuth();
  const groups = info?.principal.groups ?? [];
  const spaces = boundSpaces(permissions);
  const all = permissions?.allSpaces ?? [];
  return (
    <Section
      title="Your access"
      testId="your-access"
      description={
        <>
          Signed in as <strong>{name || 'anonymous'}</strong> ({kindLabel(kind)})
          {groups.length > 0 ? `, groups: ${groups.join(', ')}` : ''}. The server checks every call against these permissions.
        </>
      }
    >
      <Stack gap={4}>
        <Text size="sm" fw={600}>
          Global
        </Text>
        <Text size="xs" c="dimmed">
          Granted everywhere: in every space and in the global area.
        </Text>
        <PermissionBadges list={permissions?.global ?? []} testId="your-global" />
      </Stack>
      {all.length > 0 && (
        <Stack gap={4} data-testid="your-all-spaces">
          <Text size="sm" fw={600}>
            All spaces
          </Text>
          <Text size="xs" c="dimmed">
            Granted in every space, including spaces added later, but not in the global area.
          </Text>
          <PermissionBadges list={all} />
        </Stack>
      )}
      {spaces.length > 0 ? (
        <Stack gap={4}>
          <Text size="sm" fw={600}>
            By space
          </Text>
          <Text size="xs" c="dimmed">
            Effective permissions in each space: the space&apos;s own grants plus the global ones that apply in spaces.
          </Text>
          <TableBox testId="your-spaces" miw={520}>
            <Table.Thead>
              <Table.Tr>
                <Table.Th w={180}>Space</Table.Th>
                <Table.Th>Permissions</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {spaces.map((s) => (
                <Table.Tr key={s} data-testid="your-space-row">
                  <Table.Td>
                    <SpaceBadge space={s} />
                  </Table.Td>
                  <Table.Td data-label="Permissions">
                    <PermissionBadges list={effectiveInSpace(permissions, s)} />
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </TableBox>
        </Stack>
      ) : (
        seesAllSpaces(permissions) && (
          <Text size="sm" c="dimmed">
            You are not bound to single spaces; your global permissions apply in every space.
          </Text>
        )
      )}
    </Section>
  );
}

function SpacesTable({ view }: { view: AccessView }) {
  return (
    <Section title="Spaces" testId="access-spaces">
      {view.spaces.length === 0 ? (
        <Text size="sm" c="dimmed">
          The access file defines no spaces; everything is in the global area.
        </Text>
      ) : (
        <TableBox testId="access-spaces-table" miw={560}>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Name</Table.Th>
              <Table.Th>Path prefixes</Table.Th>
              <Table.Th>Hosts</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {view.spaces.map((s) => (
              <Table.Tr key={s.name} data-testid="access-space-row">
                <Table.Td>
                  <SpaceBadge space={s.name} />
                </Table.Td>
                <Table.Td data-label="Path prefixes">
                  <Values list={s.pathPrefixes} />
                </Table.Td>
                <Table.Td data-label="Hosts">
                  <Values list={s.hosts} />
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </TableBox>
      )}
    </Section>
  );
}

function Values({ list }: { list: readonly string[] }) {
  if (!list.length)
    return (
      <Text span size="sm" c="dimmed">
        —
      </Text>
    );
  return (
    <Group gap={4}>
      {list.map((v) => (
        <Code key={v}>{v}</Code>
      ))}
    </Group>
  );
}

function RolesTable({ view }: { view: AccessView }) {
  return (
    <Section title="Roles" testId="access-roles">
      <TableBox testId="access-roles-table" miw={560}>
        <Table.Thead>
          <Table.Tr>
            <Table.Th w={200}>Name</Table.Th>
            <Table.Th>Permissions</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {view.roles.map((r) => (
            <Table.Tr key={r.name} data-testid="access-role-row">
              <Table.Td>
                <Group gap={6} wrap="nowrap">
                  <Text size="sm" fw={500}>
                    {r.name}
                  </Text>
                  {r.builtin ? (
                    <Badge size="sm" variant="light" color="gray">
                      built-in
                    </Badge>
                  ) : (
                    <Badge size="sm" variant="light" color="teal">
                      custom
                    </Badge>
                  )}
                </Group>
              </Table.Td>
              <Table.Td data-label="Permissions">
                <PermissionBadges list={r.permissions} />
              </Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </TableBox>
    </Section>
  );
}

function BindingsTable({ view }: { view: AccessView }) {
  return (
    <Section
      title="Bindings"
      testId="access-bindings"
      description="Bindings of the access file in file order, then the global roles of static admin tokens written name:role:hash (FASTMOCK_ADMIN_TOKENS)."
    >
      {view.bindings.length === 0 ? (
        <Text size="sm" c="dimmed">
          The access file has no bindings.
        </Text>
      ) : (
        <TableBox testId="access-bindings-table" miw={560}>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Subject</Table.Th>
              <Table.Th>Role</Table.Th>
              <Table.Th>Scope</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {view.bindings.map((b, i) => (
              <Table.Tr key={`${i}:${b.subject}`} data-testid="access-binding-row">
                <Table.Td>
                  <Group gap={6} wrap="nowrap">
                    <Text span size="sm" className="mono">
                      {b.subject}
                    </Text>
                    {b.source === 'adminTokens' && (
                      <Tooltip label="The role of a static admin token (FASTMOCK_ADMIN_TOKENS), granted globally" withArrow multiline maw={280}>
                        <Badge size="sm" variant="light" color="orange" tt="none" data-testid="binding-source-admin-tokens">
                          admin tokens
                        </Badge>
                      </Tooltip>
                    )}
                  </Group>
                </Table.Td>
                <Table.Td data-label="Role">
                  <Badge variant="light" tt="none">
                    {b.role}
                  </Badge>
                </Table.Td>
                <Table.Td data-label="Scope">
                  <ScopeBadges global={b.global} spaces={b.spaces} />
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </TableBox>
      )}
    </Section>
  );
}

const DOCS_HINT = 'See Operations → Access control (operations/access-control/) in the FastMock documentation.';

function Configuration() {
  const q = useQuery({ queryKey: ['access'], queryFn: () => unwrap(client.GET('/__admin/access')), staleTime: 60_000 });
  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const view = q.data;
  if (!view) return null;
  return (
    <Stack gap="md" data-testid="access-config">
      <Group gap="xs">
        <Badge variant="light" color={view.configured ? 'teal' : 'gray'} tt="none" data-testid="access-file-badge">
          {view.configured ? 'Access file loaded' : 'No access file'}
        </Badge>
        <Badge variant="light" color={view.oidc ? 'teal' : 'gray'} tt="none" data-testid="oidc-badge">
          {view.oidc ? 'OIDC enabled' : 'OIDC disabled'}
        </Badge>
      </Group>
      {view.configured ? (
        <Text size="sm" c="dimmed" data-testid="access-source">
          Read-only: this configuration comes from the access file. Changes take effect after a restart (with the operator, after
          its ConfigMap is updated).
        </Text>
      ) : (
        <Alert variant="light" color="gray" icon={<IconInfoCircle size={18} />} title="No access file" data-testid="access-not-configured">
          Access is controlled by the static admin tokens and the built-in roles viewer, editor and admin; there are no spaces,
          custom roles or access-file bindings. {DOCS_HINT}
        </Alert>
      )}
      {view.configured && <SpacesTable view={view} />}
      <RolesTable view={view} />
      {(view.configured || view.bindings.length > 0) && <BindingsTable view={view} />}
    </Stack>
  );
}

export default function Access() {
  const { permissions } = useAuth();
  const canRead = can(permissions, 'access:read');
  return (
    <Stack gap="md">
      <PageHeader title="Access" description="Your permissions on this server and, with access:read, the spaces, roles and bindings it uses." />
      <YourAccess />
      {canRead ? (
        <Configuration />
      ) : (
        <Text size="sm" c="dimmed" data-testid="access-config-hidden">
          The access configuration (spaces, roles and bindings) is shown to principals with the access:read permission.
        </Text>
      )}
    </Stack>
  );
}
